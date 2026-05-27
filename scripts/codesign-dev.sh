#!/usr/bin/env bash
# scripts/codesign-dev.sh — Sign a Copad.app bundle with a stable,
# self-signed code-signing identity so macOS TCC remembers granted
# permissions across rebuilds.
#
# Problem this solves:
#   `swift build` emits an ad-hoc, linker-signed binary. Every rebuild
#   changes the cdhash, so TCC treats it as a brand new app and
#   re-prompts for every permission grant (Accessibility, Input
#   Monitoring, Local Network, …). Signing with a stable identity makes
#   TCC store grants against the *designated requirement* (cert
#   identity), and rebuilds with the same cert keep those grants.
#
# What it does:
#   1. Ensures a "Copad Dev" self-signed code-signing cert exists in the
#      login keychain. Creates one via `openssl` + `security import` on
#      first run.
#   2. codesigns the given .app bundle with that identity.
#
# Usage:
#   ./scripts/codesign-dev.sh <path-to-Copad.app>
#
# Idempotent — safe to call on every build. The first build prompts the
# Keychain Access dialog once ("codesign wants to use key Copad Dev");
# click "Always Allow" and subsequent builds run unattended.
#
# Why a self-signed cert (not Apple Developer ID):
#   This is for local development, where shipping signed builds to other
#   machines is a non-goal. Apple Developer ID requires a paid account
#   and is overkill for solving the TCC re-prompt loop. Self-signed certs
#   in the login keychain satisfy TCC's "stable designated requirement"
#   contract — TCC does not require the signing cert to be trusted by
#   the system, only that the signature validates against itself.

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

# `security find-identity -p codesigning` lists every cert with the
# codeSigning EKU and a matching private key. Match on the quoted CN so
# we don't false-positive on a substring.
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

    # Self-signed x509 with the EKUs codesign + TCC actually check.
    # `basicConstraints=CA:false` keeps Keychain from offering this as a
    # CA cert in unrelated trust UIs.
    openssl req -x509 -newkey rsa:2048 \
        -keyout "$tmpdir/key.pem" \
        -out "$tmpdir/cert.pem" \
        -days 3650 -nodes \
        -subj "/CN=$IDENTITY" \
        -addext "basicConstraints=critical,CA:false" \
        -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=critical,codeSigning" \
        2>/dev/null

    # Bundle into PKCS#12 with a transient password. Two gotchas here:
    #
    # 1. `-legacy` is required. OpenSSL 3 defaults to AES-256 + SHA-256
    #    for the PKCS12 MAC/encryption, but macOS Security.framework's
    #    SecKeychainItemImport still wants the legacy 3DES + SHA-1
    #    profile and fails with "MAC verification failed" otherwise.
    # 2. A *non-empty* password is required. OpenSSL and macOS disagree
    #    on the unicode encoding for the empty-password MAC computation,
    #    so an empty `pass:` import also fails with "MAC verification
    #    failed". The password is throwaway — the .p12 lives only in
    #    tempdir, gets imported, then deleted.
    local p12pass="copad-dev-transient"
    openssl pkcs12 -export -legacy \
        -inkey "$tmpdir/key.pem" \
        -in "$tmpdir/cert.pem" \
        -out "$tmpdir/identity.p12" \
        -name "$IDENTITY" \
        -passout "pass:$p12pass" \
        2>/dev/null

    # `-T /usr/bin/codesign` whitelists codesign for the imported key's
    # access ACL, so codesign doesn't get a recurring "allow access"
    # Keychain Access prompt.
    security import "$tmpdir/identity.p12" \
        -k "$KEYCHAIN" \
        -P "$p12pass" \
        -T /usr/bin/codesign \
        -T /usr/bin/security \
        > /dev/null

    # Self-signed certs are untrusted by default — `codesign --sign
    # <name>` then fails with "no identity found" (the trust evaluator
    # filters out untrusted identities even though the private key is
    # available). Add the cert as a trusted code-signing root in the
    # user's trust settings. This prompts once for the user's macOS
    # login password via the GUI authorization dialog; subsequent runs
    # find the trust setting and skip the prompt entirely.
    echo "    (one-time: macOS will prompt for your login password to trust the cert for code signing)"
    security add-trusted-cert -r trustRoot -p codeSign \
        -k "$KEYCHAIN" \
        "$tmpdir/cert.pem"
}

sign_app() {
    # `--force` overwrites any prior signature (e.g. swift's ad-hoc one).
    # `--deep` walks nested bundles. copad-macos currently has no nested
    # frameworks, but plugin .dylibs could appear later — cheap insurance.
    # `--timestamp=none` skips the network round-trip to Apple's
    # timestamp server; this is a dev cert, no notarisation, no need.
    echo "==> codesign --sign '$IDENTITY' $APP_PATH"
    codesign --force --deep --sign "$IDENTITY" --timestamp=none "$APP_PATH"
}

if ! identity_exists; then
    create_identity
fi

sign_app

# Sanity check — print the cdhash + Authority so a build log shows the
# signature is bound to the right identity.
codesign -dv "$APP_PATH" 2>&1 | grep -E "^(Identifier|Authority|Signature|TeamIdentifier)" || true
