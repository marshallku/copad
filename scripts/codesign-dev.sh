#!/usr/bin/env bash
# Sign a Copad.app bundle with a stable self-signed code-signing identity so macOS TCC keeps
# granted permissions across rebuilds.
#
# `swift build` emits an ad-hoc binary whose cdhash changes every build, so TCC re-prompts
# for every grant. A stable identity makes TCC store grants against the designated
# requirement, which survives rebuilds.
#
# First run creates a "Copad Dev" self-signed cert in the login keychain (openssl +
# `security import`); then the bundle is signed with it.
#
# Usage:
#   ./scripts/codesign-dev.sh <path-to-Copad.app>
#
# Idempotent. The first build prompts Keychain Access once ("codesign wants to use key Copad
# Dev"); click "Always Allow" and later builds run unattended.
#
# Self-signed rather than Developer ID: this is for local development, and TCC only needs a
# stable designated requirement, not a system-trusted cert.

set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "$0 is macOS-only" >&2
    exit 2
fi

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <path-to-Copad.app>" >&2
    exit 2
fi

APP_PATH="$1"
IDENTITY="Copad Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if [[ ! -d "$APP_PATH" ]]; then
    echo "error: $APP_PATH is not a directory (expected an .app bundle)" >&2
    exit 1
fi

# Match the quoted CN so a substring does not false-positive.
identity_exists() {
    security find-identity -p codesigning -v "$KEYCHAIN" 2>/dev/null \
        | grep -q "\"$IDENTITY\""
}

create_identity() {
    echo "==> creating self-signed code signing identity '$IDENTITY' in login keychain"

    local tmpdir
    tmpdir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" RETURN

    # `basicConstraints=CA:false` keeps Keychain from offering this as a CA cert.
    openssl req -x509 -newkey rsa:2048 \
        -keyout "$tmpdir/key.pem" \
        -out "$tmpdir/cert.pem" \
        -days 3650 -nodes \
        -subj "/CN=$IDENTITY" \
        -addext "basicConstraints=critical,CA:false" \
        -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=critical,codeSigning" \
        2>/dev/null

    # PKCS#12 needs both of these or the import fails with "MAC verification failed":
    # `-legacy` (OpenSSL 3 defaults to AES-256/SHA-256, Security.framework wants 3DES/SHA-1)
    # and a non-empty password (OpenSSL and macOS disagree on the empty-password MAC
    # encoding). The password is throwaway; the .p12 lives only in tmpdir.
    local p12pass="copad-dev-transient"
    openssl pkcs12 -export -legacy \
        -inkey "$tmpdir/key.pem" \
        -in "$tmpdir/cert.pem" \
        -out "$tmpdir/identity.p12" \
        -name "$IDENTITY" \
        -passout "pass:$p12pass" \
        2>/dev/null

    # `-T /usr/bin/codesign` avoids a recurring Keychain Access "allow access" prompt.
    security import "$tmpdir/identity.p12" \
        -k "$KEYCHAIN" \
        -P "$p12pass" \
        -T /usr/bin/codesign \
        -T /usr/bin/security \
        > /dev/null

    # Self-signed certs are untrusted by default, and `codesign --sign <name>` then fails
    # with "no identity found". Trust the cert for code signing; this prompts once for the
    # login password and later runs skip it.
    echo "    (one-time: macOS will prompt for your login password to trust the cert for code signing)"
    security add-trusted-cert -r trustRoot -p codeSign \
        -k "$KEYCHAIN" \
        "$tmpdir/cert.pem"
}

sign_app() {
    # --deep covers nested bundles (none yet; cheap insurance). --timestamp=none skips the
    # Apple timestamp server: dev cert, no notarisation.
    echo "==> codesign --sign '$IDENTITY' $APP_PATH"
    codesign --force --deep --sign "$IDENTITY" --timestamp=none "$APP_PATH"
}

# Hard-fail unless the signed bundle verifies strictly AND carries our identity: an invalid
# or ad-hoc signature makes macOS deny TCC prompts silently ("Operation not permitted"),
# the symptom this script exists to prevent. `codesign --verify` alone passes on an ad-hoc
# seal, hence the Authority check.
verify_signature() {
    local verr
    if ! verr=$(codesign --verify --strict --deep "$APP_PATH" 2>&1); then
        echo "error: signature verification failed for $APP_PATH:" >&2
        printf '%s\n' "$verr" | sed 's/^/    /' >&2
        exit 1
    fi
    # Grab the leaf (first) Authority line. NB: no `awk exit` here — an
    # early exit closes the pipe while `codesign -dvv` is still writing,
    # which under `set -o pipefail` surfaces as SIGPIPE (141) and aborts
    # the whole script. `!seen` keeps only the first match while still
    # draining all of codesign's output.
    local authority
    authority=$(codesign -dvv "$APP_PATH" 2>&1 | awk -F= '/^Authority=/ && !seen {print $2; seen=1}')
    if [[ "$authority" != "$IDENTITY" ]]; then
        echo "error: $APP_PATH is signed by '${authority:-none/ad-hoc}', expected '$IDENTITY'" >&2
        echo "       (TCC would silently deny permission prompts for this bundle)" >&2
        exit 1
    fi
}

if ! identity_exists; then
    create_identity
fi

sign_app
verify_signature

codesign -dvv "$APP_PATH" 2>&1 | grep -E "^(Identifier|Authority|TeamIdentifier|Sealed)" || true
echo "ok    signature verified: --strict --deep + Authority=$IDENTITY"
