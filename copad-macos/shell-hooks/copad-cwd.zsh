# copad cwd integration — zsh
#
# Reports every chpwd to the macOS copad GUI so its alacritty-backend
# panels know the live cwd for session restore. Linux/VTE captures
# this natively via OSC 7; macOS alacritty + hardened-runtime
# proc_pidinfo can't, so we route through the registered
# `panel.report_cwd` action via coctl.
#
# Activated only when a copad PTY child sees COPAD_PANEL_ID +
# COPAD_SOCKET (both injected by copad-term::copad_term_create),
# so sourcing this from .zshrc in non-copad shells is a silent no-op.
#
# Idempotent: chpwd_functions dedupes on hook name; sourcing twice
# is fine.

if [[ -n "$COPAD_PANEL_ID" && -n "$COPAD_SOCKET" ]] && command -v coctl >/dev/null 2>&1; then
    # JSON-escape a single string value: backslash first (otherwise
    # later replacements would double-escape), then `"`, then the
    # control bytes that are technically legal in POSIX filenames
    # (`\b \f \n \r \t`). Other control bytes (0x01-0x1f) are
    # extremely rare in real paths and would need `\uXXXX` form —
    # punt on them: coctl rejects the request with a JSON parse
    # error and the hook stays a no-op for that one cd.
    _copad_json_escape() {
        local s=${1//\\/\\\\}
        s=${s//\"/\\\"}
        s=${s//$'\b'/\\b}
        s=${s//$'\f'/\\f}
        s=${s//$'\n'/\\n}
        s=${s//$'\r'/\\r}
        printf '%s' "${s//$'\t'/\\t}"
    }
    _copad_report_cwd() {
        # Background + redirect so a slow socket can't stall the prompt.
        # `setopt local_options no_monitor` keeps the backgrounded job
        # from spamming a job-control message.
        setopt local_options no_monitor
        local pid_esc cwd_esc
        pid_esc=$(_copad_json_escape "$COPAD_PANEL_ID")
        cwd_esc=$(_copad_json_escape "$PWD")
        coctl call panel.report_cwd \
            --params "{\"panel_id\":\"$pid_esc\",\"cwd\":\"$cwd_esc\"}" \
            >/dev/null 2>&1 &!
    }
    # chpwd_functions is zsh's standard hook array; append once.
    typeset -gaU chpwd_functions
    chpwd_functions+=(_copad_report_cwd)
    # Initial report on first source — covers the cwd the shell started
    # in (which may differ from the spawn-time initialCwd if the user
    # cd'd in their .zshrc).
    _copad_report_cwd
fi
