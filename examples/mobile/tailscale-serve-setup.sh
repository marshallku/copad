#!/usr/bin/env bash
# tailscale-serve-setup.sh — get copad's web-bridge PWA reachable from your phone.
#
# Full guide: docs/mobile-access.md. This helper generates a bearer token, prints
# the platform-correct env-injection steps (systemd on Linux / launchd on macOS),
# and shows the `tailscale serve` command. It PRINTS by default and only mutates
# state when you pass explicit flags.
#
#   ./tailscale-serve-setup.sh              # print token + setup guidance
#   ./tailscale-serve-setup.sh --serve      # also run `tailscale serve` (HTTPS)
#
# Why tailscale serve (not a raw bind): it gives HTTPS + a *.ts.net host = the
# secure context iOS/Android require before a PWA may use a service worker or Web
# Push, and it injects the Tailscale-User-Login identity header so the phone skips
# the token page. See docs/mobile-access.md § tailscale serve.

set -euo pipefail

PORT="${COPAD_WEB_BRIDGE_PORT:-7575}"
DO_SERVE=0
[ "${1:-}" = "--serve" ] && DO_SERVE=1

command -v openssl >/dev/null || { echo "need openssl to generate a token" >&2; exit 1; }
TOKEN="$(openssl rand -hex 32)"   # 64 hex chars, ≥32 required by web-bridge

echo "== copad mobile access setup =="
echo
echo "1) Bearer token (put in the DAEMON env, not your shell rc):"
echo "     COPAD_WEB_BRIDGE_TOKEN=$TOKEN"
echo

case "$(uname)" in
Linux)
    # Paste-safe: `printf > file` instead of a nested heredoc (an indented
    # heredoc terminator wouldn't terminate when the user pastes it).
    cat <<EOF
2) Inject on Linux (systemd user drop-in), then reload — paste these lines:
     D=~/.config/systemd/user/copad-daemon.service.d
     mkdir -p "\$D"
     printf '[Service]\nEnvironment=COPAD_WEB_BRIDGE_TOKEN=%s\n' "$TOKEN" > "\$D/web-bridge-env.conf"
     chmod 600 "\$D/web-bridge-env.conf"
     systemctl --user daemon-reload && systemctl --user restart copad-daemon
   (Add COPAD_WEB_BRIDGE_VAPID_* Environment= lines from scripts/gen-vapid-keys.sh
    for Web Push.)
EOF
    ;;
Darwin)
    cat <<'EOF'
2) Inject on macOS: add to the <key>EnvironmentVariables</key> dict of
   ~/Library/LaunchAgents/com.marshall.copad.daemon.plist (chmod 600 it — it now
   holds a secret), using the token printed above, then:
     launchctl unload ~/Library/LaunchAgents/com.marshall.copad.daemon.plist
     launchctl load   ~/Library/LaunchAgents/com.marshall.copad.daemon.plist
   (Add the VAPID_* keys from scripts/gen-vapid-keys.sh for Web Push.)
EOF
    ;;
esac

echo
echo "3) Front it with HTTPS over your tailnet (keeps the bind on loopback):"
echo "     sudo tailscale serve --bg --https=443 http://127.0.0.1:$PORT"
echo "     tailscale serve status      # your https://<host>.<tailnet>.ts.net URL"
echo "   Open that URL on the phone → PWA installs, push works, no token page."
echo "   NOTE: writing a serve config needs root — without it you get"
echo "         'Access denied: serve config denied'. Either prefix with sudo, or"
echo "         grant your user the operator role once:"
echo "           sudo tailscale set --operator=\$USER"
echo "         (then plain 'tailscale serve' works, and so does --serve below)."
echo

if [ "$DO_SERVE" = 1 ]; then
    command -v tailscale >/dev/null || { echo "tailscale not installed" >&2; exit 1; }
    echo "Running: tailscale serve --bg --https=443 http://127.0.0.1:$PORT"
    # Writing a serve config is root-only unless the user holds the operator
    # role. Say so instead of letting tailscale's bare "Access denied" land.
    if ! tailscale serve --bg --https=443 "http://127.0.0.1:$PORT"; then
        echo >&2
        echo "serve failed (tailscale's own error is above)." >&2
        echo "If it says 'Access denied: serve config denied', it needs root — retry as:" >&2
        echo "    sudo tailscale serve --bg --https=443 http://127.0.0.1:$PORT" >&2
        echo "or grant the operator role once, then re-run this script:" >&2
        echo "    sudo tailscale set --operator=\$USER" >&2
        exit 1
    fi
    tailscale serve status
else
    echo "(re-run with --serve to run the tailscale serve command)"
fi
