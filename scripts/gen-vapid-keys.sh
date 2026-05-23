#!/usr/bin/env bash
# Generate a VAPID key pair for plugins/web-bridge push notifications.
# Prints two `export` lines to stdout — suitable for piping into a
# systemd user EnvironmentFile or a .profile.
#
# The DER-extraction offsets match SEC1 ECPrivateKey: 7-byte header
# (30 77 02 01 01 04 20) then 32 bytes of raw private key, then the
# OID + BIT STRING. The public key is the last 65 bytes (uncompressed
# 0x04 prefix + 64 bytes x|y). Both are URL-safe base64 without
# padding, the format web-push + the SPA both consume.
set -euo pipefail

TMP=$(mktemp -d)
trap "rm -rf $TMP" EXIT

openssl ecparam -name prime256v1 -genkey -noout -out "$TMP/private.pem" 2>/dev/null

PRIV=$(openssl ec -in "$TMP/private.pem" -outform DER 2>/dev/null \
    | tail -c +8 | head -c 32 \
    | base64 | tr '+/' '-_' | tr -d '=\n')

PUB=$(openssl ec -in "$TMP/private.pem" -pubout -outform DER 2>/dev/null \
    | tail -c 65 \
    | base64 | tr '+/' '-_' | tr -d '=\n')

cat <<EOF
# VAPID key pair for nestty plugins/web-bridge.
# Add to whatever env source your nesttyd systemd unit reads (e.g.
# ~/.config/environment.d/nestty.conf, your .zprofile, or a per-unit
# drop-in via systemctl --user edit nestty-daemon).
NESTTY_WEB_BRIDGE_VAPID_PRIVATE=$PRIV
NESTTY_WEB_BRIDGE_VAPID_PUBLIC=$PUB
# Optional — used as the JWT 'sub' claim. mailto: or https URL.
# NESTTY_WEB_BRIDGE_VAPID_SUBJECT=mailto:you@example.com
EOF
