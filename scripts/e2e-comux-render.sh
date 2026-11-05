#!/usr/bin/env bash
# Does the screen show what the pane holds? A REAL comux server and client inside a REAL outer
# terminal, driven through the output that broke it: `git status` (tab-indented, coloured), tabs
# in the middle of a line, CJK and decomposed Korean filenames, then `clear`. The outer screen is
# read back and compared with `comux capture-pane`, cell position by cell position: right after
# the output, after the periodic silent repaint, and after `clear` (no residue may remain).
#
# The outer terminal is tmux by default (both platforms). scripts/e2e-comux-render-gui-*.sh run it
# inside a GUI copad tab instead, through these (each an eval'd command):
#   OUTER_RUN   starts the comux client (`$COMUX`, with the test env) in the outer terminal
#   OUTER_READ  prints the outer terminal's screen
#   OUTER_COLS  prints its width in cells
#   OUTER_STOP  ends the client
#   OUTER_SHOT  screenshots it, given the moment's name in $SHOT (optional)
#   E2E_WORK    the work dir: the app has to be started with COPAD_MUX_SOCK=$E2E_WORK/s
# Own socket + state: never touches a running comux.
#
#   scripts/e2e-comux-render.sh
#   COMUX=/path/to/comux scripts/e2e-comux-render.sh     # an already built binary
#   COMUX_SERVER=/path/to/older/comux …                  # a client meeting an older server
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMUX="${COMUX:-}"
if [[ -z "$COMUX" ]]; then
    cargo build -q --manifest-path "$REPO/Cargo.toml" -p copad-mux
    COMUX="$REPO/target/debug/comux"
fi
CMP=(python3 "$REPO/scripts/lib/screencmp.py")
W="${E2E_WORK:-$(mktemp -d /tmp/cr.XXXX)}"
T="cr$$" # the tmux socket name
FAILS=0
tm() { tmux -L "$T" -f /dev/null "$@"; }
cleanup() {
    tm kill-server >/dev/null 2>&1 || true
    "$COMUX" kill-server >/dev/null 2>&1 || true
    [[ "${KEEP:-}" == 1 ]] && { echo "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT
export COPAD_MUX_SOCK="$W/s" COPAD_MUX_STATE="$W/st.json" COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 \
    COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME="$W/cfg"
# A UTF-8 locale whatever the caller has: tmux and the pane's bash both measure by it.
LOCALE="$(locale -a 2>/dev/null | grep -iE '^(en_US|C)\.utf-?8$' | head -1)"
export LANG="$LOCALE" LC_ALL="$LOCALE"
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID COPAD_MUX_HOST_TAG TMUX
mkdir -p "$W/cfg/copad" "$W/home"

# A shell with no rc files, so a prompt is one known line and a login zsh cannot take 40 s.
printf "PS1='\$ '\n" >"$W/rc"
printf '#!/bin/sh\nexec bash --noprofile --rcfile %s -i\n' "$W/rc" >"$W/sh"
chmod +x "$W/sh"

# The repo `git status` runs in: tracked files changed at several depths, so a tail left behind
# lands in different columns (what the owner's ~/docs looked like), plus Korean names in both
# composed and decomposed form.
R="$W/repo"
FILES=(metadata.json a/session.bak a/b/reference.json a/b/c/INDEX.md plan.md a/b/deep-plan.md
    a/b/c/design.md 한글노트.md)
NFD="$(python3 -c "import unicodedata; print(unicodedata.normalize('NFD', '분해된이름.md'))")"
mkdir -p "$R/a/b/c"
cd "$R"
git init -q
git config user.email e2e@example.invalid
git config user.name e2e
git config core.quotepath false
git config color.status always
for f in "${FILES[@]}" "$NFD"; do echo one >"$f"; done
git add -A
git -c commit.gpgsign=false commit -qm init
for f in "${FILES[@]}" "$NFD"; do echo two >>"$f"; done
echo new >untracked.txt
cd "$REPO"

# ---- one scenario: a mux.toml, a fresh server, a fresh client -----------------------------------
scenario() {
    local name="$1" toml="$2"
    printf '%s\n' "$toml" >"$W/cfg/copad/mux.toml"
    export COPAD_MUX_STATE="$W/st-$name.json" # or the next scenario restores this one's layout
    HOME="$W/home" SHELL="$W/sh" "${COMUX_SERVER:-$COMUX}" server >"$W/server-$name.log" 2>&1 &
    for _ in $(seq 1 100); do "$COMUX" list-sessions >/dev/null 2>&1 && break; sleep 0.1; done

    if [[ -n "${OUTER_RUN:-}" ]]; then
        eval "$OUTER_RUN"
    else
        tm new-session -d -s o -x 140 -y 40 \
            "exec env COPAD_MUX_SOCK='$COPAD_MUX_SOCK' COPAD_MUX_STATE='$COPAD_MUX_STATE' XDG_CONFIG_HOME='$XDG_CONFIG_HOME' COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 '$COMUX'"
    fi
    sleep 2

    screen() {
        if [[ -n "${OUTER_READ:-}" ]]; then eval "$OUTER_READ"; else tm capture-pane -p -t o; fi
    }
    outer_cols() {
        if [[ -n "${OUTER_COLS:-}" ]]; then eval "$OUTER_COLS"; else tm display -p -t o '#{window_width}'; fi
    }
    # Fills PANES with "token cols rows x" per pane, left to right: side by side, right of the
    # sidebar, divided by one column.
    layout() {
        local total
        total="$(outer_cols)"
        PANES=()
        while IFS= read -r l; do PANES+=("$l"); done < <("$COMUX" list --json | python3 -c '
import json, sys
total = int(sys.argv[1])
ps = json.load(sys.stdin)["panes"]
used = sum(p["cols"] for p in ps) + len(ps) - 1
x = total - used
for p in ps:
    print(p["token"], p["cols"], p["rows"], x)
    x += p["cols"] + 1' "$total")
    }
    layout
    local tok pw ph px
    read -r tok pw ph px <<<"${PANES[0]}"
    echo "[$name] pane ${pw}x${ph} at column $px"

    check() {
        local what="$1" entry i=0
        layout # the window may have changed size (a browser attaching to Broadway does that)
        screen >"$W/outer.txt"
        for entry in "${PANES[@]}"; do
            local t c r x
            read -r t c r x <<<"$entry"
            "$COMUX" capture-pane "$t" -S "$r" >"$W/pane.txt" 2>/dev/null
            if ! "${CMP[@]}" "$W/pane.txt" "$W/outer.txt" "$x" 0 "$c" "$r" --label "$name/$what/pane$i"; then
                FAILS=$((FAILS + 1))
            fi
            i=$((i + 1))
        done
    }
    run() {
        "$COMUX" send "$tok" "$1" >/dev/null
        "$COMUX" send "$tok" $'\r' >/dev/null
    }
    residue() {
        local what="$1" hits
        hits="$(screen | grep -E '\.(json|bak|md|txt)' || true)"
        if [[ -n "$hits" ]]; then
            echo "$name/$what: residue on screen:"
            printf '%s\n' "$hits" | sed 's/^/  /'
            FAILS=$((FAILS + 1))
        fi
    }
    # 4 s is past the default silent repaint (3 s), which re-emits every cell: this is where a row
    # used to shift sideways.
    settle() { sleep 4; }
    shot() {
        SHOT="$1"
        if [[ -n "${OUTER_SHOT:-}" ]]; then eval "$OUTER_SHOT" || true; fi
    }

    run "cd '$R' && clear && git status"
    sleep 1
    check after-output
    settle
    check after-repaint
    shot git-status-after-repaint
    settle
    check after-second-repaint

    run "clear"
    sleep 0.6 # before the next repaint could hide it
    check after-clear
    residue after-clear
    shot after-clear

    run "printf 'a\\tb\\tc\\n\\tx\\t\\ty\\n12345678\\t|\\n한글\\t|\\n\\033[41m\\tred\\033[0m\\tplain\\n'"
    settle
    check tabs-mid-line
    run "clear; git diff --stat; git log --oneline -3"
    settle
    check diff-stat
    run "git status"
    settle
    run "clear"
    sleep 0.6
    check clear-after-repaint
    residue clear-after-repaint

    # Glyphs emulators and tables disagree about: combining, arrow, Powerline, emoji, VS16.
    run "clear; printf 'e\\314\\201 \\342\\206\\222 \\356\\202\\260 \\360\\237\\216\\211 \\342\\235\\244\\357\\270\\217 end\\n'"
    settle
    check glyphs
    # A soft-wrapped line, then a burst long enough to scroll.
    run "clear; printf 'x%.0s' \$(seq 1 400); echo"
    settle
    check soft-wrap
    run "for i in \$(seq 1 300); do printf '\\t%s\\t가나다\\tend\\n' \$i; done"
    settle
    check burst
    # The alternate screen and back.
    run "clear; git status"
    sleep 0.6
    run "less a/b/reference.json"
    settle
    check alt-screen
    "$COMUX" send "$tok" q >/dev/null
    settle
    check alt-screen-left

    # A split: the same output in a pane whose first column is not the terminal's.
    run "clear"
    "$COMUX" split -h --from "$tok" >/dev/null
    sleep 1
    layout
    local right
    read -r right _ <<<"${PANES[1]}"
    "$COMUX" send "$right" "cd '$R' && clear && git status" >/dev/null
    "$COMUX" send "$right" $'\r' >/dev/null
    run "git status"
    sleep 1
    check split-output
    settle
    check split-repaint
    shot split-after-repaint
    "$COMUX" send "$right" "clear" >/dev/null
    "$COMUX" send "$right" $'\r' >/dev/null
    run "clear"
    sleep 0.6
    check split-clear
    residue split-clear
    "$COMUX" close 1 >/dev/null
    sleep 1

    # A resize (only the tmux outer can do this on demand): the reflowed text has to land too.
    if [[ -z "${OUTER_RUN:-}" ]]; then
        layout
        read -r tok pw ph px <<<"${PANES[0]}"
        run "git status"
        sleep 1
        tm resize-window -t o -x 100 -y 30
        sleep 1.5
        layout
        check resized
        settle
        check resized-repaint
        run "clear"
        sleep 0.6
        check resized-clear
        residue resized-clear
    fi

    if [[ -n "${OUTER_STOP:-}" ]]; then eval "$OUTER_STOP"; else tm kill-server >/dev/null 2>&1 || true; fi
    "$COMUX" kill-server >/dev/null 2>&1 || true
    for _ in $(seq 1 50); do "$COMUX" list-sessions >/dev/null 2>&1 || break; sleep 0.1; done
}

# Pane titles are chrome over a pane's top row, which would read as a difference.
scenario no-sidebar $'sidebar = false\npane_titles = false'
# The sidebar's width is not a multiple of 8, so a tab stop lands somewhere else in the row. A GUI
# copad claims the sidebar and draws its own, so this would only repeat the scenario above there.
if [[ -z "${OUTER_RUN:-}" ]]; then
    scenario sidebar $'sidebar = true\npane_titles = false'
fi

if [[ "$FAILS" -gt 0 ]]; then
    echo "FAIL: $FAILS check(s) differ"
    exit 1
fi
echo "PASS: the outer screen matched the pane in every check"
