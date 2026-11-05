
if [[ -n "$COPAD_PANEL_ID" && -n "$COPAD_SOCKET" ]] && command -v coctl >/dev/null 2>&1; then
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
        if [[ "$PWD" != "$_COPAD_LAST_REPORTED_CWD" ]]; then
            _COPAD_LAST_REPORTED_CWD=$PWD
            local pid_esc cwd_esc
            pid_esc=$(_copad_json_escape "$COPAD_PANEL_ID")
            cwd_esc=$(_copad_json_escape "$PWD")
            coctl call panel.report_cwd \
                --params "{\"panel_id\":\"$pid_esc\",\"cwd\":\"$cwd_esc\"}" \
                >/dev/null 2>&1 &
            disown 2>/dev/null
        fi
    }
    case ";$PROMPT_COMMAND;" in
        *";_copad_report_cwd;"*) ;;
        *) PROMPT_COMMAND="_copad_report_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
    _copad_report_cwd
fi
