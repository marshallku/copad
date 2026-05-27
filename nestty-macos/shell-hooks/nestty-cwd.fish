# nestty cwd integration — fish
#
# Same contract as nestty-cwd.zsh / .bash. Fish has a native
# `--on-variable PWD` event; we register one function for it.

if test -n "$NESTTY_PANEL_ID"; and test -n "$NESTTY_SOCKET"; and command -q nestctl
    function __nestty_json_escape
        # Backslash MUST be replaced first or the next pass will
        # double-escape it. Newline → literal `\n`.
        set -l s (string replace --all '\\' '\\\\' -- $argv[1])
        set s (string replace --all '"' '\\"' -- $s)
        string replace --all \n '\\n' -- $s
    end
    function __nestty_report_cwd --on-variable PWD
        set -l pid_esc (__nestty_json_escape "$NESTTY_PANEL_ID")
        set -l cwd_esc (__nestty_json_escape "$PWD")
        # `&` background runs as fish job; `disown` so the job table
        # doesn't fill up across many cds. stderr/stdout suppressed —
        # a transient socket error mid-prompt shouldn't surface.
        nestctl call panel.report_cwd \
            --params "{\"panel_id\":\"$pid_esc\",\"cwd\":\"$cwd_esc\"}" \
            >/dev/null 2>&1 &
        disown 2>/dev/null
    end
    # Initial report covers the cwd at shell start.
    __nestty_report_cwd
end
