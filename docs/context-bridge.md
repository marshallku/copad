# Context Bridge — design doc for Phase 22.1

> Status: design. Implementation lands in Phase 22.1 sessions. See [roadmap.md](./roadmap.md) for the broader Phase 22 framing.

## Why this exists

The wedge of [Phase 22](./roadmap.md#phase-22-context-aware-workstation-hub) is "copad is the live context sidebar that follows the active pane wherever it runs, including over SSH." That requires a low-latency, transport-agnostic way for any shell — local terminal, tmux pane, or SSH session — to tell the local copad GUI what context the active prompt is sitting in.

OSC (Operating System Command) escape sequences are the only mechanism that satisfies all four constraints simultaneously:

| Constraint | OSC | `coctl event publish` polling | inotify on remote FS |
|---|---|---|---|
| Works through SSH unchanged | ✅ | ❌ (CLI not on remote host) | ❌ |
| Works through tmux unchanged | ✅ | ❌ | ❌ |
| Per-prompt cost | one PTY write, microseconds | RPC round-trip + JSON encode | inotify watch storm |
| No daemon required on remote | ✅ | ❌ | ❌ |

OSC 7 (CWD reporting) and OSC 133 (prompt boundaries) already prove the transport: both are emitted by user shells, propagate through SSH and tmux, and are decoded by terminal emulators (VTE, WezTerm, Kitty, iTerm2, `alacritty_terminal`). copad's new context payload rides the same path.

## Wire format

### OSC sequence — shortlist

Three candidates, ranked by recommendation:

1. **Custom OSC 6500 with versioned JSON** (recommended). `OSC 6500 ; v1 ; <json> ST`. Future-extensible (`v2` etc.). No collision with any documented OSC range. JSON keeps the payload self-describing and ergonomic for shell-init scripts to construct.
2. **OSC 1337 (iTerm2) with a `copad-context=<base64>` key**. iTerm2's custom OSC is conventional for tool-specific data. Downside: keyspace pollution and base64 is friction for shell scripts.
3. **OSC 133 ; P with a `copad-context=<base64>` parameter**. Extends the FinalTerm shell-integration protocol. Downside: many other tools consume OSC 133 and a malformed extension could break them.

Decision in the implementation session, after a quick `grep -r 'OSC ' ~/dev/copad copad-core copad-linux copad-macos` to confirm no internal collision. Default lean: option 1.

### Payload (`v1`)

```json
{
  "host": "marshall-desktop",         // hostname; "localhost" alias allowed
  "cwd": "/home/marshall/dev/copad",  // absolute path, post-expansion
  "git_remote": "marshallku/copad",   // owner/repo from `origin` if present, else ""
  "branch": "master",                  // current branch, or detached-HEAD short sha
  "tmux_session": "copad-dev",         // $TMUX_PANE → session name, or ""
  "pane_cmd": "zsh",                   // foreground command on the pane PTY, or ""
  "timestamp_ms": 1748419200000,       // emitter clock, milliseconds since epoch
  "hmac": "<hex>"                      // optional — see security
}
```

All fields except `timestamp_ms` may be empty strings when not derivable. Consumers must tolerate missing keys (forward compatibility for `v2`).

### Reserved fields (do not use in `v1`)

- `user`, `session_id`, `pid` — leak risk; not needed for the dossier panel's data sources.
- Free-form notes — anything user-authored belongs in `~/docs`, not the bus.

## tmx integration point

`~/dev/tmx/src/shell_init.rs` already generates per-shell init scripts that tmx injects into spawned shells. Phase 22.1 adds one helper function emitting the OSC at every `precmd` (zsh) / `PROMPT_COMMAND` (bash) / `precmd_functions` (fish) hook. Lands as a separate tmx PR — referenced from this phase but never modified in this repo.

Shape of the addition (zsh example, illustrative):

```zsh
__copad_context() {
  local host="${HOSTNAME:-$(hostname)}"
  local cwd="$PWD"
  local git_remote="$(git -C "$PWD" remote get-url origin 2>/dev/null \
                       | sed -nE 's#.*[:/]([^/]+/[^/.]+)(\.git)?$#\1#p')"
  local branch="$(git -C "$PWD" symbolic-ref --short -q HEAD 2>/dev/null \
                       || git -C "$PWD" rev-parse --short HEAD 2>/dev/null)"
  local tmux_session="${TMUX_PANE:+$(tmux display-message -p '#S' 2>/dev/null)}"
  local pane_cmd="$ZSH_NAME"  # placeholder; pane_cmd usually filled by copad itself
  local ts_ms="$(($(date +%s%N) / 1000000))"
  printf '\033]6500;v1;{"host":"%s","cwd":"%s","git_remote":"%s","branch":"%s","tmux_session":"%s","pane_cmd":"%s","timestamp_ms":%s}\033\\' \
    "$host" "$cwd" "$git_remote" "$branch" "$tmux_session" "$pane_cmd" "$ts_ms"
}
precmd_functions+=(__copad_context)
```

(Real implementation must JSON-escape every field — the snippet above is illustrative only.)

For users not on tmx, copad ships an equivalent `examples/shell/copad-context.zsh` / `.bash` / `.fish` that they can source from their rc file directly.

## copad-side wiring

### OSC handler

Linux: VTE 0.84+ accepts custom OSC dispatchers via `vte::Terminal::connect_setup_context_menu` is *not* it; the actual API path is the OSC-callback registration that `copad-linux/src/terminal.rs` already uses for OSC 7 (`hostCurrentDirectoryUpdate`). Phase 22.1 adds a sibling callback for OSC 6500.

macOS: `copad-term` (the alacritty_terminal wrapper landed in Phase 6 of the macOS app) exposes the OSC stream pre-decode. Add a match arm for OSC 6500 alongside the existing OSC 7 handling. Same code path as Linux at the protocol layer.

### EventBus event kind

New event kind `pane.context_changed`, documented in [workflow-runtime.md](./workflow-runtime.md) once landed:

```rust
Event {
    kind: "pane.context_changed".into(),
    source: panel_id.to_string(),       // emitted by the panel that received the OSC
    timestamp: SystemTime::now(),
    payload: serde_json::json!({
        "host": ..., "cwd": ..., "git_remote": ...,
        "branch": ..., "tmux_session": ..., "pane_cmd": ...,
        "timestamp_ms": ..., "v": 1,
    }),
}
```

Origin tagging: `event_bus::Origin::Internal` (the OSC reached us through our own VTE/term layer, not through external `coctl event publish`). Triggers consuming this event do **not** need `accept_external = true`.

### ContextService extension

`copad-core/src/context.rs` (already exists for the `context.snapshot` socket method) gains a `HashMap<PanelId, PaneContext>` populated by `EventBus::subscribe("pane.context_changed")`. The existing `active_panel` field combined with that map produces a synthesized `active_pane_context` field on every `context.snapshot` reply.

This makes the new signal usable from:
- `coctl context` (Phase 19.2 expansion already merges several sources here — add one row).
- The Phase 22.2 dossier panel (subscribes to bus + reads context on first render).
- Triggers — condition expressions like `{pane.git_remote} == "marshallku/copad"` start working.

## SSH and tmux pass-through

Both layers are transparent to OSC sequences by default:

- **SSH**: OSC bytes are part of the program-output stream the server writes to the client PTY. `ssh` does not filter them.
- **tmux**: tmux is a multiplexer; it forwards OSC sequences to the outer terminal. Some versions strip OSC 52 (clipboard) for security but never touched the broader OSC space. OSC 7 already works through tmux in production.

If a particular SSH server or tmux build is found to strip OSC 6500, the mitigation is **inside** Phase 22.1, not a precondition: ship a config flag `[context_bridge] allow_kinds = [6500, 1337]` that adds OSC 1337 as a fallback transport.

## Security — trust boundary

OSC sequences are part of the PTY byte stream. Any program that writes to the user's terminal can emit one. The threat is `cat malicious_file.txt` where `malicious_file.txt` contains `OSC 6500 ; v1 ; {...lies...} ST` — the malicious payload would otherwise overwrite the active pane's context and (if it lies about `git_remote`) cause the dossier panel to show notes from an unrelated repository.

**Intent (from the Phase 22.1 plan): only honor OSC at prompt-command time, not mid-output.** This must hold even if a child process inside the user's shell tries to inject — `$COPAD_CONTEXT_SECRET` would be inherited by such a child via the standard env copy, so an HMAC-only design does not actually enforce the boundary. The mitigation has to be a state-machine that distinguishes the prompt window from command execution, not just a signature.

Three mitigations, applied together (1 is the primary trust boundary, 2 and 3 are defense-in-depth):

1. **OSC 133 prompt-boundary state machine** *(the actual "prompt-command time only" enforcement)*. Per-pane state in the OSC handler:
   - `idle` (initial) — reject all OSC 6500.
   - On `OSC 133 ; A` (prompt start) or `OSC 133 ; P` (prompt redraw) → enter `at-prompt`.
   - On `OSC 133 ; B` (command typed) or `OSC 133 ; C` (command begin) → return to `idle`.
   - OSC 6500 honored *only* when in `at-prompt`. Any OSC 6500 emitted mid-execution (between `133 ; C` and the next `133 ; A`) is dropped before payload parsing.

   Shell init that copad ships emits OSC 133 alongside `__copad_context`; both go into the same `precmd_functions` hook so they are state-machine-ordered (`133 ; A` then `6500` then the prompt itself, with `133 ; C` arriving when the user submits the command). For users whose shell already emits OSC 133 via the terminal's shell-integration package (iTerm2, WezTerm, Kitty, vscode-shell-integration), copad's `__copad_context` rides those existing marks.

2. **Shell-init HMAC**. Per-session secret `$COPAD_CONTEXT_SECRET` injected into shells copad spawns; `__copad_context` computes `hmac = HMAC-SHA256(secret, host || cwd || git_remote || branch || tmux_session || pane_cmd || timestamp_ms)`. Defense-in-depth — does NOT replace mitigation 1, because child processes inside the shell inherit the env var and could compute valid MACs from mid-execution code paths. HMAC catches a different class of attack: malicious data files (`cat injected.txt`) that emit OSC 6500 without any access to the secret (and would also be dropped by mitigation 1 because they fire during execution, not at prompt time — defense in depth means *both* checks must pass).

3. **Timestamp window**. Reject payloads with `timestamp_ms` more than 5 minutes off the local clock. Prevents replay of captured payloads from another machine.

**Opt-outs (both documented as security-relaxing):**
- `[context_bridge] require_prompt_boundary = false` — accept OSC 6500 outside `at-prompt` state. For environments where the user's shell genuinely cannot emit OSC 133. Re-introduces mid-output injection risk; recommended only with `require_hmac = true` and a single-user trust assumption.
- `[context_bridge] require_hmac = false` — for shells copad didn't spawn (e.g., a manually-started remote SSH session). Mitigation 1 still applies; the spoofing surface is bounded to processes that can race into `at-prompt` state.

Setting both to `false` simultaneously disables the context bridge in safe mode; copad logs a warning at startup if both are off.

## SSH session secret distribution

For copad to validate an HMAC on a payload emitted from an SSH session, `$COPAD_CONTEXT_SECRET` has to reach that remote shell. Two paths:

1. **SSH SendEnv / AcceptEnv** (preferred, no remote daemon). Add `SendEnv COPAD_CONTEXT_SECRET` to the user's local `~/.ssh/config`; remote `sshd_config` needs a matching `AcceptEnv COPAD_CONTEXT_SECRET`. The remote shell init reads `$COPAD_CONTEXT_SECRET` and uses it the same way the local shell does.
2. **Documented opt-out** (the `require_hmac = false` path above) for one-off remote hosts the user doesn't control.

Phase 22.1 doc'd both; defaults to (1).

## macOS / Linux parity

Both platforms hit identical code paths at the protocol layer:

| Concern | Linux | macOS |
|---|---|---|
| OSC handler | VTE OSC-callback (sibling to OSC 7 — existing pattern in `copad-linux/src/terminal.rs`) | `copad-term` OSC stream (sibling to OSC 7 — existing pattern in Phase 6 renderer wrapper) |
| Bus / context wiring | unchanged from existing `copad-core` | unchanged (Swift consumes via `copad-ffi`) |
| Shell init secret env var | injected into shell spawn (existing `spawn_shell` in `copad-linux`) | injected by `LocalProcessTerminalView` spawn path (existing `copad-macos` shell launcher) |
| Plugin panel rendering | webkit6 (Linux) | WKWebView (macOS) — both already host all 10 first-party plugins |

No platform branch in the design. The implementation may differ at the C-ABI level (OSC callback signatures differ between VTE and `copad-term`) but the wire format, security model, and event kind are identical.

## What we explicitly defer

- **Binary wire format**. JSON keeps shell-init scripts trivial; binary would shave microseconds at the cost of debuggability. Revisit only if a measurable bottleneck appears.
- **Encryption of the payload**. HMAC gives authenticity; the payload content (cwd, repo name) is not confidential in any practical threat model. Encrypted variant could be added as `v2` if a user wants to obscure context from `tmux pipe-pane` log readers.
- **Bidirectional protocol**. The bridge is one-way (shell → copad). If a future feature needs copad to push context *to* the remote shell (e.g., "switch to this branch"), that goes through `coctl` over SSH, not through OSC.

## Open questions for implementation

1. **OSC number final pick** — `6500` vs `1337` vs `133;P`. Decided after a grep pass in the implementation session.
2. **Shell init distribution** — ship as `examples/shell/copad-context.{zsh,bash,fish}` plus a doc snippet, or expose `coctl shell-init zsh` similar to `direnv hook zsh`? Both, probably — `coctl` for the install/upgrade path, `examples/` for users who want to inline-customize.
3. **`pane_cmd` derivation** — the shell can only report its own name (`$ZSH_NAME` etc.), not the foreground child during command execution. The current foreground command is something copad already tracks per pane (it owns the PTY). Likely answer: shell reports `pane_cmd: ""` and the OSC handler fills it from copad's own pane state, overriding the shell's value. To revisit during implementation.

## Related docs

- [roadmap.md § Phase 22](./roadmap.md#phase-22-context-aware-workstation-hub) — phase framing and dependencies on subsequent slices
- [workflow-runtime.md](./workflow-runtime.md) — EventBus / ActionRegistry / ContextService primitives this design plugs into
- [harness-integration.md](./harness-integration.md) — daemon-first migration, including the existing `event.history` ring buffer and trust-boundary `Origin` field
- [decisions.md](./decisions.md) — the OSC-over-`coctl` decision is captured there alongside the life-assistant absorption stance
