# nestty cwd integration — bash
#
# Same contract as nestty-cwd.zsh. Bash has no native chpwd; we hook
# PROMPT_COMMAND instead, which fires before each prompt redraw. That
# also catches cwd changes done programmatically (not just `cd`).

if [[ -n "$NESTTY_PANEL_ID" && -n "$NESTTY_SOCKET" ]] && command -v nestctl >/dev/null 2>&1; then
    _nestty_json_escape() {
        local s=${1//\\/\\\\}
        s=${s//\"/\\\"}
        s=${s//$'\b'/\\b}
        s=${s//$'\f'/\\f}
        s=${s//$'\n'/\\n}
        s=${s//$'\r'/\\r}
        printf '%s' "${s//$'\t'/\\t}"
    }
    _nestty_report_cwd() {
        if [[ "$PWD" != "$_NESTTY_LAST_REPORTED_CWD" ]]; then
            _NESTTY_LAST_REPORTED_CWD=$PWD
            local pid_esc cwd_esc
            pid_esc=$(_nestty_json_escape "$NESTTY_PANEL_ID")
            cwd_esc=$(_nestty_json_escape "$PWD")
            nestctl call panel.report_cwd \
                --params "{\"panel_id\":\"$pid_esc\",\"cwd\":\"$cwd_esc\"}" \
                >/dev/null 2>&1 &
            disown 2>/dev/null
        fi
    }
    # Only prepend if not already present — sourcing twice mustn't
    # double-fire on every prompt.
    case ";$PROMPT_COMMAND;" in
        *";_nestty_report_cwd;"*) ;;
        *) PROMPT_COMMAND="_nestty_report_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
    _nestty_report_cwd
fi
