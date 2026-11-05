# copad cwd integration for zsh. Reports each chpwd to the macOS copad GUI via coctl, because
# macOS alacritty under hardened runtime cannot read the cwd itself (Linux/VTE uses OSC 7).
# No-op unless COPAD_PANEL_ID and COPAD_SOCKET are set.

if [[ -n "$COPAD_PANEL_ID" && -n "$COPAD_SOCKET" ]] && command -v coctl >/dev/null 2>&1; then
    # JSON-escape one value: backslash first, then `"`, then \b \f \n \r \t. Other control bytes
    # are not escaped; coctl rejects the request and the hook is a no-op for that cd.
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
        # Background so a slow socket cannot stall the prompt; no_monitor hides the job message.
        setopt local_options no_monitor
        local pid_esc cwd_esc
        pid_esc=$(_copad_json_escape "$COPAD_PANEL_ID")
        cwd_esc=$(_copad_json_escape "$PWD")
        coctl call panel.report_cwd \
            --params "{\"panel_id\":\"$pid_esc\",\"cwd\":\"$cwd_esc\"}" \
            >/dev/null 2>&1 &!
    }
    typeset -gaU chpwd_functions
    chpwd_functions+=(_copad_report_cwd)
    _copad_report_cwd
fi
