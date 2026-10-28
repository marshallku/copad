#!/usr/bin/env bash
# Run the review demo host on a throwaway VM (App Store review only; stop it afterwards).
#
#   DEMO_PASSWORD=... deploy/review-demo/run.sh [port]     # default port 22
#
# Limits: one container, 512 MiB, one CPU, 256 processes; sshd caps logins (sshd_config) and
# comux-only caps open connections. Outbound traffic from the container is dropped (the demo
# needs none: pushes are off), so a reviewer's shell cannot reach anything else. Run as root.
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${DEMO_PASSWORD:?set DEMO_PASSWORD (it goes in the App Review notes)}"
PORT="${1:-22}"
docker build -q -f deploy/review-demo/Dockerfile -t copad-review-demo . >/dev/null
docker network inspect copad-review >/dev/null 2>&1 \
    || docker network create --subnet 172.30.99.0/24 copad-review >/dev/null
# Replies to inbound SSH pass; anything the container starts itself is dropped — forwarded
# (DOCKER-USER: the internet) and to the VM itself (INPUT: the bridge gateway and its services).
for chain in DOCKER-USER INPUT; do
    iptables -C "$chain" -s 172.30.99.0/24 -m conntrack ! --ctstate ESTABLISHED,RELATED -j DROP 2>/dev/null \
        || iptables -I "$chain" -s 172.30.99.0/24 -m conntrack ! --ctstate ESTABLISHED,RELATED -j DROP
done
docker rm -f copad-review >/dev/null 2>&1 || true
docker run -d --name copad-review --hostname copad-demo --network copad-review \
    -p "$PORT:22" -e DEMO_PASSWORD -v copad-review-keys:/keys \
    --memory 512m --cpus 1 --pids-limit 256 --restart unless-stopped \
    copad-review-demo >/dev/null
for _ in $(seq 1 50); do docker logs copad-review 2>&1 | grep -q "Server listening" && break; sleep 0.2; done
echo "up on port $PORT; host key:"
docker exec copad-review ssh-keygen -lf /keys/ssh_host_ed25519_key.pub
# Nightly reset (a fresh comux and home; the host key stays): add to root's crontab
#   0 4 * * * docker restart copad-review
