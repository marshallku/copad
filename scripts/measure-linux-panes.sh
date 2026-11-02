#!/usr/bin/env bash
# Memory cost of panes in a headless copad-linux (gtk4-broadwayd, isolated config/state; nothing on
# the live desktop), `[terminal] images` on and off: PSS with one tab, then after N more tabs that
# each print 300 lines. Run on the Linux host after `cargo build --release -p copad-linux -p copad-cli`.
#
#   N=8 MODES="true false" scripts/measure-linux-panes.sh
set -euo pipefail
R="$(cd "$(dirname "$0")/.." && pwd)"
N=${N:-8}
W=""; APP=""; C=""; B=""
cleanup() {
  # Every step may find its process already gone: never let that stop the run (set -e).
  if [[ -n "$APP" ]]; then kill $APP 2>/dev/null || true; sleep 1; fi
  if [[ -n "$C" ]]; then kill $C 2>/dev/null || true; fi
  if [[ -n "$B" ]]; then kill $B 2>/dev/null || true; fi
  if [[ -n "$W" ]]; then
    for f in /proc/[0-9]*/fd/*; do case "$(readlink "$f" 2>/dev/null)" in "$W"/*) p=${f#/proc/}; echo "${p%%/*}";; esac; done 2>/dev/null | sort -u | xargs -r kill 2>/dev/null || true
    rm -rf $W
  fi
  W=""; APP=""; C=""; B=""
}
trap cleanup EXIT
for mode in ${MODES:-true false}; do
  W=$(mktemp -d /tmp/cmem.XXXX); mkdir -p $W/cfg/copad
  printf '[terminal]\nimages = %s\nshell = "/bin/sh"\n' $mode >$W/cfg/copad/config.toml
  gtk4-broadwayd :21 >$W/b.log 2>&1 & B=$!
  sleep 1
  XDG_STATE_HOME=$W/state XDG_CONFIG_HOME=$W/cfg COPAD_E2E_TAG=$W GDK_BACKEND=broadway BROADWAY_DISPLAY=:21 dbus-run-session -- $R/target/release/copad >$W/c.log 2>&1 & C=$!
  GUI=""; APP=""
  for _ in $(seq 1 100); do for p in $(pgrep -x copad); do tr '\0' '\n' </proc/$p/environ 2>/dev/null | grep -qx "COPAD_E2E_TAG=$W" && [ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ] && { GUI=$XDG_RUNTIME_DIR/copad/gui-$p.sock; APP=$p; break 2; }; done; sleep 0.2; done
  CO=$R/target/release/coctl
  fill() { $CO --socket $GUI terminal exec "for i in \$(seq 1 300); do echo \$i abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnop; done" >/dev/null; }
  pss() { awk '/^Pss:/ {print int($2/1024)}' /proc/$APP/smaps_rollup; }
  sleep 3; fill; sleep 2
  one=$(pss)
  for _ in $(seq 1 $N); do $CO --socket $GUI tab new >/dev/null; sleep 1; fill; sleep 0.6; done
  sleep 3
  many=$(pss)
  echo "images=$mode: 1 pane ${one} MB, $((N+1)) panes ${many} MB -> $(( (many-one)*10/N ))e-1 MB per pane"
  cleanup
done
