# copad context bridge — zsh
#
# Publishes a `pane.context_changed` event every prompt redraw so copad's
# ContextService (and the future dossier panel) knows the live host /
# cwd / git remote / branch / tmux session / shell name of the active
# pane. See `docs/context-bridge.md` for the full design.
#
# Activated only when a copad PTY child sees COPAD_PANEL_ID + COPAD_SOCKET
# (both injected by copad-linux/src/terminal.rs and the macOS counterpart),
# so sourcing this from .zshrc in non-copad shells is a silent no-op.
#
# Source from your `.zshrc`:
#     source /path/to/copad/examples/shell/copad-context.zsh
#
# Idempotent: precmd_functions dedupes on hook name; sourcing twice is fine.
#
# Trust note (decision #46): `coctl event publish` stamps events as
# Origin::External — any same-UID process can publish with an arbitrary
# panel_id. ContextService treats this as best-effort display data;
# downstream triggers consuming `pane.*` need explicit accept_external.

# Resolve coctl from PATH, falling back to common install dirs.
# systemd-user units and desktop entries often have a stripped PATH
# that omits ~/.local/bin and ~/.cargo/bin even though both are
# canonical install locations (see CLAUDE.md install notes); a bare
# `command -v coctl` would silently skip the hook in those shells
# even though the binary exists. Same fallback chain web-bridge uses
# for tmx lookup.
__copad_ctx_coctl_path() {
    if command -v coctl >/dev/null 2>&1; then
        command -v coctl
    elif [[ -x "$HOME/.local/bin/coctl" ]]; then
        printf '%s\n' "$HOME/.local/bin/coctl"
    elif [[ -x "$HOME/.cargo/bin/coctl" ]]; then
        printf '%s\n' "$HOME/.cargo/bin/coctl"
    fi
}

if [[ -n "$COPAD_PANEL_ID" && -n "$COPAD_SOCKET" ]] && [[ -n "$(__copad_ctx_coctl_path)" ]]; then
    # Cache the resolved coctl binary at source-time so the precmd
    # hook doesn't re-walk the fallback chain every prompt.
    typeset -g __COPAD_CTX_COCTL="$(__copad_ctx_coctl_path)"
    # zsh's integer epoch — produces an `int`, unlike `$EPOCHREALTIME`
    # (float) and unlike `date +%s%N` (GNU-only, breaks on BSD/macOS).
    # Second precision is adequate for per-prompt context emission.
    zmodload -F zsh/datetime b:strftime p:EPOCHSECONDS 2>/dev/null

    _copad_ctx_json_escape() {
        # Same escape set as copad-macos/shell-hooks/copad-cwd.zsh —
        # backslash first, then `"`, then \b \f \n \r \t. Rare control
        # bytes (0x01-0x1f outside these) would need \uXXXX; if a path
        # has them, coctl rejects with parse error and we silently
        # skip that prompt's emission.
        local s=${1//\\/\\\\}
        s=${s//\"/\\\"}
        s=${s//$'\b'/\\b}
        s=${s//$'\f'/\\f}
        s=${s//$'\n'/\\n}
        s=${s//$'\r'/\\r}
        printf '%s' "${s//$'\t'/\\t}"
    }

    __copad_context_publish() {
        # `local_options no_monitor` suppresses the job-control message
        # when we disown the backgrounded coctl call.
        setopt local_options no_monitor
        # Detach EVERYTHING — including the git/tmux probes that can
        # stall on slow mounts or unresponsive tmux servers — into a
        # background subshell. Synchronous data gathering before `&!`
        # would defeat the "must never block the prompt" guarantee.
        (
            local host cwd_esc git_remote branch tmux_session pane_cmd ts_ms panel_esc payload
            host=${HOSTNAME:-$(hostname 2>/dev/null)}
            cwd_esc=$(_copad_ctx_json_escape "$PWD")
            # `git remote get-url origin | sed` to extract owner/repo
            # from ssh- or https-form URLs. Empty if no origin / not a
            # git repo (acceptable — daemon treats empty as "no repo").
            git_remote=$(git -C "$PWD" remote get-url origin 2>/dev/null \
                | sed -nE 's#.*[:/]([^/]+/[^/.]+)(\.git)?$#\1#p')
            branch=$(git -C "$PWD" symbolic-ref --short -q HEAD 2>/dev/null \
                || git -C "$PWD" rev-parse --short HEAD 2>/dev/null)
            if [[ -n "$TMUX_PANE" ]]; then
                tmux_session=$(tmux display-message -p '#S' 2>/dev/null)
            fi
            pane_cmd=${ZSH_NAME:-zsh}
            ts_ms=$(( EPOCHSECONDS * 1000 ))
            panel_esc=$(_copad_ctx_json_escape "$COPAD_PANEL_ID")
            payload=$(printf '{"panel_id":"%s","host":"%s","cwd":"%s","git_remote":"%s","branch":"%s","tmux_session":"%s","pane_cmd":"%s","timestamp_ms":%d,"v":1}' \
                "$panel_esc" \
                "$(_copad_ctx_json_escape "$host")" \
                "$cwd_esc" \
                "$(_copad_ctx_json_escape "$git_remote")" \
                "$(_copad_ctx_json_escape "$branch")" \
                "$(_copad_ctx_json_escape "$tmux_session")" \
                "$(_copad_ctx_json_escape "$pane_cmd")" \
                "$ts_ms")
            "$__COPAD_CTX_COCTL" event publish pane.context_changed "$payload" --quiet \
                >/dev/null 2>&1
        ) &!
        return 0
    }

    # precmd_functions runs at every prompt redraw — not just on cd,
    # because branch / tmux_session can change without a directory
    # change. Append once via unique-array.
    typeset -gaU precmd_functions
    precmd_functions+=(__copad_context_publish)

    # Phase 22.3 — active-doc signal. precmd fires AFTER foreground
    # nvim exits, so a pgrep-on-precmd approach would never see the
    # editor that's actually open. preexec fires BEFORE the command
    # runs, so when the user types `nvim ~/docs/foo.md` the hook sees
    # the cmd line and can publish a `doc.opened` event with the
    # path-relative-to-KB-root for the panel to consume.
    #
    # Misses (acceptable for v1):
    # - `nvim` with no path arg (fzf-driven file picker, `:e`)
    # - editing via `dn add-todo` style writers (not nvim)
    # - foreground `nvim` swap to another file via `:e` mid-session
    __copad_doc_publish() {
        # `preexec` arg 1 is the literal command line.
        local cmd="$1"
        # Cheap guard before we fork.
        [[ "$cmd" == nvim* ]] || return 0
        setopt local_options no_monitor
        (
            local kb_root="${COPAD_KB_ROOT:-${COPAD_DOCS_ROOT:-$HOME/docs}}"
            # Canonicalize so relative-arg lookups (`nvim foo.md` from
            # inside ~/docs) still match the kb_root prefix.
            local cwd_abs="$PWD"
            # Extract first arg under kb_root. zsh word-split with =().
            local -a words
            words=(${(z)cmd})
            local active=""
            local w
            for w in "${words[@]:1}"; do
                # Strip surrounding quotes if any.
                w=${(Q)w}
                [[ -z "$w" ]] && continue
                [[ "$w" == -* ]] && continue
                # Resolve relative paths against PWD.
                local cand
                if [[ "$w" == /* ]]; then
                    cand="$w"
                else
                    cand="$cwd_abs/$w"
                fi
                # Normalize without requiring the file to exist
                # (`nvim` on a new file is a real workflow).
                # Drop `./` and double-slashes; leave the rest.
                cand="${cand//\/.\//\/}"
                while [[ "$cand" == *//* ]]; do
                    cand=${cand//\/\//\/}
                done
                if [[ "$cand" == "$kb_root"/* ]]; then
                    active="${cand#$kb_root/}"
                    break
                fi
            done
            [[ -z "$active" ]] && return 0
            local panel_esc path_esc ts_ms payload
            panel_esc=$(_copad_ctx_json_escape "$COPAD_PANEL_ID")
            path_esc=$(_copad_ctx_json_escape "$active")
            ts_ms=$(( EPOCHSECONDS * 1000 ))
            payload=$(printf '{"panel_id":"%s","path":"%s","timestamp_ms":%d,"v":1}' \
                "$panel_esc" "$path_esc" "$ts_ms")
            "$__COPAD_CTX_COCTL" event publish doc.opened "$payload" --quiet \
                >/dev/null 2>&1
        ) &!
        return 0
    }

    typeset -gaU preexec_functions
    preexec_functions+=(__copad_doc_publish)
fi
