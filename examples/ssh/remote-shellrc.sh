# copad-over-SSH: remote shell rc snippet.
#
# Paste into the REMOTE host's shell rc (~/.zshrc / ~/.bashrc). Needs no root, unlike
# `AcceptEnv COPAD_SOCKET` (remote sshd_config).
#
# Exports COPAD_SOCKET to the forwarded socket path IFF that socket is present, and clears a stale
# inherited value otherwise. The path must match the LEFT side of the RemoteForward line in
# examples/ssh/copad-remote.sshconfig.
#
# `[ -S ]` proves a socket exists, not that the forward is live (a crashed SSH master can leave a
# stale one); that is benign, since `coctl … --quiet` exits 0 on a dead socket. To confirm the
# link: `coctl call system.ping`.

# --- zsh / bash ---
if [ -S /tmp/copad-ws.sock ]; then
    export COPAD_SOCKET=/tmp/copad-ws.sock
elif [ "${COPAD_SOCKET:-}" = /tmp/copad-ws.sock ]; then
    unset COPAD_SOCKET
fi

# --- fish (put this in ~/.config/fish/config.fish instead) ---
#   if test -S /tmp/copad-ws.sock
#       set -gx COPAD_SOCKET /tmp/copad-ws.sock
#   else if test "$COPAD_SOCKET" = /tmp/copad-ws.sock
#       set -e COPAD_SOCKET
#   end
