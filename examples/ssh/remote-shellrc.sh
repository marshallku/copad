# copad-over-SSH: remote shell rc snippet.
#
# Paste into the REMOTE host's shell rc (~/.zshrc / ~/.bashrc). Zero root needed —
# this is the low-friction alternative to `AcceptEnv COPAD_SOCKET` (which requires
# editing the remote sshd_config as root).
#
# It exports COPAD_SOCKET to the forwarded socket path IFF that socket is present,
# and clears a stale inherited value otherwise. The path must match the LEFT side
# of the RemoteForward line in examples/ssh/copad-remote.sshconfig.
#
# Note: `[ -S ]` proves the path is a socket, not that the forward is LIVE — a
# crashed SSH master can leave a stale socket. That's benign: `coctl … --quiet`
# just exits 0 when the socket is dead, so no hook ever breaks. To confirm the
# link is actually live: `coctl call system.ping`.

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
