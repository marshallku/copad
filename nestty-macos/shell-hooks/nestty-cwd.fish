# nestty cwd integration — fish
#
# Same contract as nestty-cwd.zsh / .bash. Fish has a native
# `--on-variable PWD` event; we register one function for it.

if test -n "$NESTTY_PANEL_ID"; and test -n "$NESTTY_SOCKET"; and command -q nestctl
    function __nestty_json_escape
        # Backslash MUST be replaced first or later passes double-
        # escape it. Covers the JSON-mandated controls (`\b \f \n
        # \r \t`) that occur in real paths; rarer 0x01-0x1f bytes
        # would need `\uXXXX` form and aren't handled.
        set -l s (string replace --all '\\' '\\\\' -- $argv[1])
        set s (string replace --all '"' '\\"' -- $s)
        set s (string replace --all \b '\\b' -- $s)
        set s (string replace --all \f '\\f' -- $s)
        set s (string replace --all \n '\\n' -- $s)
        set s (string replace --all \r '\\r' -- $s)
        string replace --all \t '\\t' -- $s
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
