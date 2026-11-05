# copad context bridge — zsh
#
# Publishes a `pane.context_changed` event at every prompt redraw (host, cwd, git remote, branch,
# tmux session, shell). See `docs/context-bridge.md`.
#
# A silent no-op unless COPAD_PANEL_ID and COPAD_SOCKET are set (copad injects both into its PTY
# children). Source from your `.zshrc`:
#     source /path/to/copad/examples/shell/copad-context.zsh
#
# Trust note (decision #46): `coctl event publish` stamps events as Origin::External, so any
# same-UID process can publish with an arbitrary panel_id. Triggers consuming `pane.*` need an
# explicit accept_external.

# Resolve coctl from PATH, falling back to common install dirs: systemd-user units and desktop
# entries often have a PATH without ~/.local/bin and ~/.cargo/bin.
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
    # Resolved once at source time, not on every prompt.
    typeset -g __COPAD_CTX_COCTL="$(__copad_ctx_coctl_path)"
    # Integer epoch seconds (`$EPOCHREALTIME` is a float, `date +%s%N` is GNU-only).
    zmodload -F zsh/datetime b:strftime p:EPOCHSECONDS 2>/dev/null

    _copad_ctx_json_escape() {
        # Same escape set as copad-macos/shell-hooks/copad-cwd.zsh. Other control bytes are not
        # escaped: coctl rejects the payload and that prompt's emission is skipped.
        local s=${1//\\/\\\\}
        s=${s//\"/\\\"}
        s=${s//$'\b'/\\b}
        s=${s//$'\f'/\\f}
        s=${s//$'\n'/\\n}
        s=${s//$'\r'/\\r}
        printf '%s' "${s//$'\t'/\\t}"
    }

    __copad_context_publish() {
        # no_monitor suppresses the job-control message when the backgrounded call is disowned.
        setopt local_options no_monitor
        # Everything runs in the background subshell, including the git/tmux probes that can
        # stall on slow mounts; gathering before `&!` would block the prompt.
        (
            local host cwd_esc git_remote branch tmux_session pane_cmd ts_ms panel_esc payload
            host=${HOSTNAME:-$(hostname 2>/dev/null)}
            cwd_esc=$(_copad_ctx_json_escape "$PWD")
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

    # Every prompt, not just cd: branch and tmux session change without a directory change.
    typeset -gaU precmd_functions
    precmd_functions+=(__copad_context_publish)

    # Active-doc signal. precmd runs after a foreground nvim exits, so only preexec can see the
    # command line. Misses: `nvim` without a path argument, other writers, `:e` mid-session.
    __copad_doc_publish() {
        local cmd="$1"
        [[ "$cmd" == nvim* ]] || return 0
        setopt local_options no_monitor
        (
            local kb_root="${COPAD_KB_ROOT:-${COPAD_DOCS_ROOT:-$HOME/docs}}"
            local cwd_abs="$PWD"
            local -a words
            words=(${(z)cmd})
            local active=""
            local w
            for w in "${words[@]:1}"; do
                w=${(Q)w}
                [[ -z "$w" ]] && continue
                [[ "$w" == -* ]] && continue
                local cand
                if [[ "$w" == /* ]]; then
                    cand="$w"
                else
                    cand="$cwd_abs/$w"
                fi
                # Normalize without requiring the file to exist (`nvim` on a new file is normal).
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
