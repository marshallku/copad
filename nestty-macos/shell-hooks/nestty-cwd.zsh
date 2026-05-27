# nestty cwd integration — zsh
#
# Reports every chpwd to the macOS nestty GUI so its alacritty-backend
# panels know the live cwd for session restore. Linux/VTE captures
# this natively via OSC 7; macOS alacritty + hardened-runtime
# proc_pidinfo can't, so we route through the registered
# `panel.report_cwd` action via nestctl.
#
# Activated only when a nestty PTY child sees NESTTY_PANEL_ID +
# NESTTY_SOCKET (both injected by nestty-term::nestty_term_create),
# so sourcing this from .zshrc in non-nestty shells is a silent no-op.
#
# Idempotent: chpwd_functions dedupes on hook name; sourcing twice
# is fine.

if [[ -n "$NESTTY_PANEL_ID" && -n "$NESTTY_SOCKET" ]] && command -v nestctl >/dev/null 2>&1; then
    _nestty_report_cwd() {
        # Background + redirect so a slow socket can't stall the prompt.
        # `setopt local_options no_monitor` keeps the backgrounded job
        # from spamming a job-control message.
        setopt local_options no_monitor
        nestctl call panel.report_cwd \
            --params "{\"panel_id\":\"$NESTTY_PANEL_ID\",\"cwd\":\"$PWD\"}" \
            >/dev/null 2>&1 &!
    }
    # chpwd_functions is zsh's standard hook array; append once.
    typeset -gaU chpwd_functions
    chpwd_functions+=(_nestty_report_cwd)
    # Initial report on first source — covers the cwd the shell started
    # in (which may differ from the spawn-time initialCwd if the user
    # cd'd in their .zshrc).
    _nestty_report_cwd
fi
