# Context Bridge — Phase 22.1 (local, coctl-based)

> Status: shipped (Linux). macOS port owned by the user. Original SSH-flavored OSC design preserved in [§ Out of scope but designed for revival](#out-of-scope-but-designed-for-revival).

## Purpose

The [Phase 22 dossier panel](./roadmap.md#phase-22-context-aware-workstation-hub) needs to know the active pane's repo / branch / cwd / tmux session in real time. The shell already has that information — it's the only thing in the system that does, reliably, on every prompt. The bridge is just plumbing to get it from the shell to copad's bus.

## Mechanism — `coctl event publish` from precmd

Each prompt redraw, the shell's precmd hook constructs a `pane.context_changed` payload and publishes it via `coctl event publish pane.context_changed '<json>'`:

```
shell precmd ─►  coctl event publish  ─►  copadd events.publish  ─►  EventBus
                                                                       │
                                              ┌────────────────────────┘
                                              ▼
                                    GUI ContextService + GUI → daemon forwarder
                                              │
                                              ▼
                                    daemon ContextService + ring buffer + triggers
```

The shell is a direct child of copad (via VTE / SwiftTerm), so it inherits `$COPAD_SOCKET` and `$COPAD_PANEL_ID`. The first is a "this is a copad-spawned shell" marker for the hook to gate emission on; the second is the per-pane id the hook stamps onto every payload so `ContextService` can key its per-panel cache correctly.

(Implementation note: `coctl event publish` dials the daemon socket at `daemon_socket_path()` directly — it doesn't actually *use* `$COPAD_SOCKET` as the transport. The env var is presence-only. This means **daemon availability is a hard prerequisite** — if `copadd` is down, the hook silently no-ops and `pane_context` stays stale until the next prompt.)

## Wire format — `PaneContext`

The payload is a flat JSON object matching [`copad_core::context::PaneContext`](../copad-core/src/context.rs):

```json
{
  "panel_id": "8a1c…uuid",
  "host":     "arch",
  "cwd":      "/home/marshall/dev/copad",
  "git_remote": "marshallku/copad",
  "branch":   "master",
  "tmux_session": "",
  "pane_cmd": "zsh",
  "timestamp_ms": 1748419200000,
  "v": 1
}
```

Every string field is `String` (not `Option<String>`) — missing or undeterminable values are empty strings `""`, not `null`. `timestamp_ms` is an `i64` (zsh `$EPOCHSECONDS * 1000`; second precision is enough for per-prompt rate). `v` is a schema version; older / newer payloads round-trip cleanly because the deserializer uses `#[serde(default)]` per field — extra fields are ignored, missing fields default to empty.

`panel_id` is the only required field: events with an empty `panel_id` are dropped by `ContextService::apply_event` (debug-logged, no panic).

## Trust boundary

`coctl event publish` reaches the daemon over the Unix socket, which carries `SO_PEERCRED` source stamping ([decision #23](./decisions.md)). The daemon marks the resulting event `Origin::External` ([decision #37](./decisions.md)). That gating model is the entire application-layer security model — there is no HMAC, no per-session secret, no state machine.

Consequence: **any same-UID process on the workstation can publish `pane.context_changed` with an arbitrary `panel_id`.** The dossier panel (Phase 22.2) and any other consumer should treat `pane_context` as best-effort display data, not authoritative state. When trigger conditions eventually interpolate `pane_context.*` fields (a future engine extension — not enforceable in the current TriggerEngine), each consuming trigger must opt in via `[triggers.security] accept_external = true`, the existing harness pattern.

This trust model holds because the threat model is "single user on their own workstation." If that ever needs to expand (e.g., multi-user systems, remote shells over SSH), the [revival design](#out-of-scope-but-designed-for-revival) below is what to bring back.

## Why not OSC for the local case

Bytes-through-PTY only buys anything when copad and the shell aren't already in the same trust domain — i.e., remote SSH where the shell can't reach copad's socket. For a copad-spawned local shell, the socket *is* available, IPC is microseconds, and OSC parsing requires custom VTE bindings or an FFI shim that's not exposed in the `vte4` 0.8.x Rust API today. The coctl path is strictly simpler.

## macOS parity

The macOS shell-spawn already injects both `$COPAD_SOCKET` and `$COPAD_PANEL_ID` (`copad-macos/Sources/Copad/TerminalViewController.swift:527-528`) and ships `copad-macos/shell-hooks/copad-cwd.zsh` as a precedent for backgrounded coctl-from-precmd. The Linux script's structure is mirror-able, with one platform-specific gotcha:

**Timestamp**: zsh's `$EPOCHSECONDS` (from `zmodload zsh/datetime`) works on both platforms (zsh ≥ 5.0 ships on modern macOS). `$EPOCHREALTIME` is a float and would fail `i64` deserialization. `date +%s%N` is GNU-only — don't use it. If the macOS port targets older zsh, fall back to `python3 -c 'import time;print(int(time.time()*1000))'`.

Otherwise the macOS port is one-for-one — same payload shape, same `coctl` call, same detached background subshell pattern.

## Per-shell init

The canonical zsh implementation ships at [`examples/shell/copad-context.zsh`](../examples/shell/copad-context.zsh). Three properties that matter:

1. **Silent no-op outside copad shells**: the `[[ -n "$COPAD_PANEL_ID" && -n "$COPAD_SOCKET" ]] && command -v coctl >/dev/null 2>&1` guard makes the script safe to `source` from a global `.zshrc`.
2. **Never blocks the prompt**: *all* data gathering (git remote / branch / tmux query / hostname) runs inside the detached `( ... &!) 2>/dev/null` subshell. Synchronous git calls on slow mounts or unresponsive tmux servers won't reach the prompt.
3. **JSON escaping is lossy for rare control bytes**: backslash / `"` / `\b\f\n\r\t` are escaped. Other control bytes (0x01-0x1f outside this set) would need `\uXXXX` form; instead the daemon rejects with a parse error and the hook silently skips that one prompt's emission. Acceptable — those characters are vanishingly rare in real paths.

**bash and fish** are mechanical follow-ups: same payload, same `coctl` call, different precmd hook surface (`PROMPT_COMMAND` for bash, `fish_prompt` for fish). Not in the v1 ship.

## Open questions

- **`pane_cmd` refinement** — the shell can only report its own name (`$ZSH_NAME` = `"zsh"`). The actual foreground command during execution is something copad already tracks via VTE. If the dossier panel ever wants live `pane_cmd` during long-running commands, copad can override the shell-reported value with its own — but only at panel-render time, not in the published event (the event timing is precmd, which is between commands).
- **Trigger interpolation over `pane_context.*`** — the current TriggerEngine exposes `context.active_panel` / `context.active_cwd` / `context.presence` only. Surfacing `pane_context.git_remote` etc. is a future engine extension. When it lands, the load-bearing security note is: `accept_external` alone gates only the firing event's origin; if a same-UID-spoofed `pane.context_changed` poisons `ContextService` and a *later, internal* event interpolates the poisoned state, `accept_external` won't have caught it. The future extension must enforce origin-aware reads of context fields, not just origin-aware firing.

## Out of scope but designed for revival

The original Phase 22 design covered SSH and tmux-across-hosts. That path needs OSC bytes traveling through the PTY (the only signal that survives `ssh` and tmux pass-through unchanged), plus crypto to gate "emitted at prompt time vs mid-output execution." It was dropped because the implementation cost — VTE OSC capture (Rust bindings don't expose a custom-OSC API today), HMAC plumbing, OSC 133 prompt-boundary state machine, per-session secret distribution over `SSH SendEnv` — outweighed the value for the current user. If SSH coverage returns, this is the shape to bring back:

### Wire format

Custom `OSC <N> ; v1 ; <json> ST` where `<N>` is picked from a shortlist of three candidates: `OSC 6500` (custom high-range, no collisions, recommended), `OSC 1337` (iTerm2 conv with a `copad-context=<base64>` key), or `OSC 133 ; P` extension. JSON payload identical to the current `PaneContext` plus a top-level `hmac` field.

### Trust boundary

OSC bytes are part of the PTY byte stream, so `cat malicious_file.txt` could otherwise inject a fake context. Three layers, applied together:

1. **OSC 133 prompt-boundary state machine** (primary): per-pane state initialized `idle`; `OSC 133 ; A`/`P` → `at-prompt`; `OSC 133 ; B`/`C` → `idle`. OSC 6500 honored only in `at-prompt`. Shell init emits OSC 133 alongside the context payload from the same precmd hook so the marks are state-machine-ordered.
2. **HMAC-SHA256** (defense-in-depth): per-session secret `$COPAD_CONTEXT_SECRET` injected on shell spawn. Catches malicious data files that emit OSC bytes without access to the secret. Does NOT replace mitigation 1, because env-var inheritance allows child processes to compute valid MACs from mid-output.
3. **Timestamp window** (replay protection): reject payloads more than 5 minutes off the local clock.

Opt-outs (`require_prompt_boundary = false`, `require_hmac = false`) are documented as security-relaxing; both off is a startup warning.

### copad-side wiring

Linux: custom OSC dispatcher via VTE FFI (the Rust bindings don't expose this today — would need a shim crate). macOS: `alacritty_terminal`'s OSC stream match arm (already exposed in `copad-term`).

### Secret distribution over SSH

For HMAC validation on payloads emitted from an SSH session, `$COPAD_CONTEXT_SECRET` must reach the remote shell. Use `SendEnv COPAD_CONTEXT_SECRET` in `~/.ssh/config` + matching `AcceptEnv` in remote `sshd_config` for trusted hosts. Documented opt-out (`require_hmac = false`) for untrusted hosts.

### Why all of this is on the bench

Three things would have to change to justify implementing the above:
- The user actually wants to SSH into a remote box and have copad's dossier panel reflect that remote shell's context.
- VTE 0.8.x (or successor) exposes a custom-OSC subscribe API, OR we're willing to write a FFI shim crate.
- The trust model expands beyond "single user on their own workstation."

None of these are true today. The design lives here so it's not lost.

## Related docs

- [roadmap.md § Phase 22](./roadmap.md#phase-22-context-aware-workstation-hub) — phase framing and dependencies
- [workflow-runtime.md](./workflow-runtime.md) — EventBus / ContextService primitives
- [decisions.md #23](./decisions.md) — `events.publish` + `SO_PEERCRED` source stamping
- [decisions.md #37](./decisions.md) — `Origin` field and `[security] accept_external` opt-in
- [decisions.md #46](./decisions.md) — this design's decision entry
