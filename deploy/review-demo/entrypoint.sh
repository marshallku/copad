#!/bin/sh
set -eu
: "${DEMO_PASSWORD:?run with -e DEMO_PASSWORD (see run.sh)}"
printf 'review:%s\n' "$DEMO_PASSWORD" | chpasswd
unset DEMO_PASSWORD
[ -f /keys/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N '' -f /keys/ssh_host_ed25519_key
su review -s /bin/sh -c /usr/local/lib/review-demo/seed.sh
exec /usr/sbin/sshd -D -e
