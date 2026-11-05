#!/usr/bin/env bash
# install-claude-hooks.sh — wire Claude Code hook scripts to the copad bus.
#
# Sentinel-based, idempotent. Users mark the patch point in their own
# hook scripts with:
#
#     # COPAD_HOOK_PUBLISH: <kind> [<json-payload>]
#
# and this script inserts a bus-publish line right after, bracketed by:
#
#     # COPAD_HOOK_PUBLISH_END
#
# so re-runs are no-ops and `--uninstall` is a clean removal. The patch
# point stays under the user's control (you put the sentinel where the
# event semantically fires); the patcher just writes the mechanical
# `coctl event publish ...` line.
#
# Usage:
#   install-claude-hooks.sh                # install across default dirs
#   install-claude-hooks.sh --dry-run      # show what would change
#   install-claude-hooks.sh --uninstall    # remove inserted blocks
#   install-claude-hooks.sh --hooks-dir P  # override scan path (repeatable)
#   install-claude-hooks.sh --self-test    # run unit tests, no fs changes
#
# Default scan dirs: ~/.claude/hooks and ~/.claude/scripts.
#
# See docs/harness-hooks.md for sentinel placement examples per hook.

set -euo pipefail

SCRIPT_NAME="${0##*/}"

# Sentinel grammar:
#   # COPAD_HOOK_PUBLISH: <kind> [<json-payload>]
# `<json-payload>` is optional (`{}` when omitted) and passed verbatim to `coctl event
# publish`, so `${VAR}` in it expands in the hook script at run time (`${` is not escaped).
SENTINEL_START='# COPAD_HOOK_PUBLISH:'
SENTINEL_END='# COPAD_HOOK_PUBLISH_END'

DEFAULT_HOOKS_DIRS=("$HOME/.claude/hooks" "$HOME/.claude/scripts")

usage() {
    grep -E '^#( |$)' "$0" | sed -E 's/^# ?//' | head -n 28
    exit "${1:-0}"
}

log() { printf '[%s] %s\n' "$SCRIPT_NAME" "$*" >&2; }

file_has_sentinel() {
    grep -qF -- "$SENTINEL_START" "$1"
}

# uninstall keeps the sentinel line so a later install re-inserts the pair; dry-run writes
# to stdout instead of the file.
process_file() {
    local mode="$1"
    local file="$2"

    if ! file_has_sentinel "$file"; then
        return 0
    fi

    local tmp
    tmp=$(mktemp "${file}.copad-patch.XXXXXX")
    # AWK state machine: buffer the sentinel line on hit, defer
    # emission until we know whether an END marker follows (within
    # `LOOKAHEAD` lines of body). Any lines between sentinel and END
    # are treated as the previously-inserted publish body and
    # rebuilt on install / dropped on uninstall.
    awk -v mode="$mode" \
        -v start_marker="$SENTINEL_START" \
        -v end_marker="$SENTINEL_END" '
        function emit_publish(sentinel_line,    rest, kind, payload, idx, escaped_payload, last_char, marker_pos, indent) {
            # Sentinel can be at any column (e.g. inside an indented
            # case branch). `index()` finds where the marker starts;
            # we slice from there + length(marker) to capture the
            # body.
            marker_pos = index(sentinel_line, start_marker)
            if (marker_pos == 0) {
                print "internal: emit_publish called on non-sentinel line: " sentinel_line > "/dev/stderr"
                exit 3
            }
            # Preserve the sentinel leading whitespace so the
            # emitted publish line + END marker match the surrounding
            # block indentation (cosmetic, but bash hooks usually
            # live inside case branches and an un-indented publish
            # reads as broken at a glance).
            indent = substr(sentinel_line, 1, marker_pos - 1)
            rest = substr(sentinel_line, marker_pos + length(start_marker))
            sub(/^[[:space:]]+/, "", rest)
            if (length(rest) == 0) {
                print "error: empty sentinel in " FILENAME > "/dev/stderr"
                exit 2
            }
            idx = index(rest, " ")
            if (idx == 0) {
                kind = rest
                payload = "{}"
            } else {
                kind = substr(rest, 1, idx - 1)
                payload = substr(rest, idx + 1)
                sub(/^[[:space:]]+/, "", payload)
                if (length(payload) == 0) payload = "{}"
            }
            # Quote handling: JSON-literal mode vs substitution mode.
            # In JSON mode (payload is a {...} blob) we escape every
            # double quote so the wrapping bash double-quote context
            # parses correctly while ${VAR} still expands at hook
            # fire time. In substitution mode (payload starts with
            # $( and ends with )) we pass through verbatim; bash
            # treats double quotes inside $(...) as the body local
            # syntax, so escaping them would break the body. See
            # docs/harness-hooks.md "Payload safety" + tests 9, 11.
            last_char = substr(payload, length(payload), 1)
            if (substr(payload, 1, 2) == "$(" && last_char == ")") {
                escaped_payload = payload
            } else {
                # Order matters: escape `\` FIRST (so `\` → `\\`),
                # THEN escape `"` (so `"` → `\"`). If we escaped `"`
                # first the inserted `\` would get doubled by the
                # second pass. Bash inside `"..."` collapses `\\` →
                # `\` and `\"` → `"`, so the emitted text round-
                # trips to the literal user payload. Without the
                # `\` escape, `\\tmp` in JSON
                # arrived at coctl as `\tmp`.
                escaped_payload = payload
                gsub(/\\/, "\\\\\\\\", escaped_payload)
                gsub(/"/, "\\\"", escaped_payload)
            }
            # Print BOTH the publish line and the END marker with
            # matching indent so callers do not need to echo
            # end_marker themselves with the right whitespace.
            printf "%scommand -v coctl >/dev/null && coctl event publish %s --quiet \"%s\" &\n", indent, kind, escaped_payload
            printf "%s%s\n", indent, end_marker
        }
        function flush_pending(    i) {
            # Sentinel without matching END within LOOKAHEAD lines —
            # treat as unpaired. Print the sentinel and any buffered
            # lines verbatim, then (install only) insert a fresh
            # publish + END BEFORE the buffered lines. We emit publish
            # + END right after the sentinel, then dump the buffer.
            if (!have_sentinel) return
            print sentinel_line
            if (mode != "uninstall") {
                emit_publish(sentinel_line)
            }
            # Buffered lines were content following the sentinel that
            # turned out NOT to be a publish block (no END within
            # window). Re-emit them in order.
            for (i = 0; i < buf_n; i++) print buf[i]
            have_sentinel = 0
            buf_n = 0
        }

        BEGIN {
            have_sentinel = 0
            sentinel_line = ""
            buf_n = 0
            LOOKAHEAD = 8
        }
        {
            if ($0 ~ start_marker) {
                # If we already had a pending sentinel and we hit a
                # new one without an intervening END, flush the old
                # one as unpaired before starting the new one.
                if (have_sentinel) flush_pending()
                have_sentinel = 1
                sentinel_line = $0
                buf_n = 0
                next
            }
            if (have_sentinel) {
                if ($0 ~ end_marker) {
                    # Paired sentinel: emit sentinel + (publish + END)
                    # on install, sentinel only on uninstall. Buffered
                    # lines were the old publish body — discarded.
                    print sentinel_line
                    if (mode != "uninstall") {
                        emit_publish(sentinel_line)
                    }
                    have_sentinel = 0
                    buf_n = 0
                    next
                }
                # Buffer the line; if no END within LOOKAHEAD we flush
                # as unpaired and treat buffered lines as plain content.
                buf[buf_n++] = $0
                if (buf_n >= LOOKAHEAD) {
                    flush_pending()
                }
                next
            }
            # Plain line, no pending sentinel.
            print
        }
        END {
            if (have_sentinel) {
                # Sentinel without END at EOF — install pairs it.
                print sentinel_line
                if (mode != "uninstall") {
                    emit_publish(sentinel_line)
                }
                for (i = 0; i < buf_n; i++) print buf[i]
            }
        }
    ' "$file" > "$tmp"

    if [[ "$mode" == "dry-run" ]]; then
        if ! diff -u "$file" "$tmp" > /dev/null 2>&1; then
            printf '\n=== %s (would change) ===\n' "$file"
            diff -u "$file" "$tmp" || true
        fi
        rm -f "$tmp"
        return 0
    fi

    # Preserve the file mode: mktemp creates 0600, so a patched hook would lose its
    # executable bit on macOS. GNU `chmod --reference` is Linux-only, hence both stat styles.
    if ! diff -q "$file" "$tmp" > /dev/null 2>&1; then
        # `stat -L`: without it a symlink reports its own mode (0777) and the temp file
        # would be chmodded world-writable before replacing the real target.
        local mode
        mode=$(stat -L -c '%a' "$file" 2>/dev/null || stat -L -f '%OLp' "$file" 2>/dev/null || true)
        if [[ -n "$mode" ]]; then
            chmod "$mode" "$tmp"
        fi
        # Write through symlinks (stow-style dotfiles): a plain `mv` would replace the link
        # with a regular file. Resolve to an ABSOLUTE path (realpath, else `readlink -f`); a
        # relative symlink target would otherwise resolve against the cwd.
        local target="$file"
        if [[ -L "$file" ]]; then
            if command -v realpath >/dev/null 2>&1; then
                target=$(realpath -- "$file" 2>/dev/null || true)
            elif readlink -f -- "$file" >/dev/null 2>&1; then
                target=$(readlink -f -- "$file" 2>/dev/null || true)
            else
                target=""
            fi
            if [[ -z "$target" ]]; then
                log "skip: cannot canonicalize symlink $file (need realpath or readlink -f)"
                rm -f "$tmp"
                return 0
            fi
        fi
        mv "$tmp" "$target"
        if [[ "$file" != "$target" ]]; then
            log "patched: $file -> $target"
        else
            log "patched: $file"
        fi
    else
        rm -f "$tmp"
    fi
}

# `find -L`: users link ~/.claude/{hooks,scripts} from their dotfiles, and without it find
# returns nothing against a symlinked dir. A missing dir is skipped silently.
scan_dir() {
    local mode="$1"
    local dir="$2"
    [[ -d "$dir" ]] || return 0
    while IFS= read -r -d '' file; do
        process_file "$mode" "$file"
    done < <(find -L "$dir" -maxdepth 1 -type f -name '*.sh' -print0)
}

# --self-test runs in a tmpdir and never touches the real ~/.claude.
run_self_test() {
    local tmp
    tmp=$(mktemp -d /tmp/install-claude-hooks-test.XXXXXX)
    # `${tmp:-}` keeps the trap safe under `set -u` once `tmp` is out of scope.
    trap 'rm -rf "${tmp:-}"' EXIT

    local failed=0
    # Not `local`: bash cannot declare a function local.
    _selftest_pass() { printf '  ok %s\n' "$1"; }
    _selftest_fail() { printf '  FAIL %s: %s\n' "$1" "$2" >&2; failed=1; }

    cat > "$tmp/test1.sh" <<'EOF'
#!/bin/sh
echo "before"
# COPAD_HOOK_PUBLISH: claude.commit_blocked {"reason":"$R"}
echo "after"
EOF
    process_file install "$tmp/test1.sh"
    if grep -qF 'coctl event publish claude.commit_blocked --quiet' "$tmp/test1.sh" \
        && grep -qF 'COPAD_HOOK_PUBLISH_END' "$tmp/test1.sh"; then
        _selftest_pass "install adds publish line + END"
    else
        _selftest_fail "test1" "missing publish/end after install: $(cat "$tmp/test1.sh")"
    fi

    cp "$tmp/test1.sh" "$tmp/test1.before"
    process_file install "$tmp/test1.sh"
    if cmp -s "$tmp/test1.before" "$tmp/test1.sh"; then
        _selftest_pass "second install is no-op"
    else
        _selftest_fail "test2" "second install changed file (see diff -u $tmp/test1.before $tmp/test1.sh)"
    fi

    process_file uninstall "$tmp/test1.sh"
    if grep -qF "$SENTINEL_START" "$tmp/test1.sh" \
        && ! grep -qF 'coctl event publish claude.commit_blocked' "$tmp/test1.sh" \
        && ! grep -qF "$SENTINEL_END" "$tmp/test1.sh"; then
        _selftest_pass "uninstall removes publish + END, keeps sentinel"
    else
        _selftest_fail "test3" "uninstall left bad state: $(cat "$tmp/test1.sh")"
    fi

    cat > "$tmp/test4.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.simple_event
EOF
    process_file install "$tmp/test4.sh"
    if grep -qF 'coctl event publish claude.simple_event --quiet "{}"' "$tmp/test4.sh"; then
        _selftest_pass "empty payload defaults to {}"
    else
        _selftest_fail "test4" "payload default wrong: $(cat "$tmp/test4.sh")"
    fi

    cat > "$tmp/test5.sh" <<'EOF'
#!/bin/sh
echo "no markers here"
exit 0
EOF
    cp "$tmp/test5.sh" "$tmp/test5.before"
    process_file install "$tmp/test5.sh"
    if cmp -s "$tmp/test5.before" "$tmp/test5.sh"; then
        _selftest_pass "no sentinel → unchanged"
    else
        _selftest_fail "test5" "file changed without sentinel"
    fi

    cat > "$tmp/test6.sh" <<'EOF'
#!/bin/sh
if cond1; then
# COPAD_HOOK_PUBLISH: claude.commit_blocked {"reason":"cond1"}
echo "denied 1"
fi
if cond2; then
# COPAD_HOOK_PUBLISH: claude.commit_blocked {"reason":"cond2"}
echo "denied 2"
fi
EOF
    process_file install "$tmp/test6.sh"
    # Count by the `coctl event publish` prefix; the sentinel comment also contains `reason`.
    local publish_count
    publish_count=$(grep -cF 'coctl event publish' "$tmp/test6.sh" || true)
    local end_count
    end_count=$(grep -cF "$SENTINEL_END" "$tmp/test6.sh" || true)
    local cond1_pub
    cond1_pub=$(grep -cE 'coctl event publish.*reason.*cond1' "$tmp/test6.sh" || true)
    local cond2_pub
    cond2_pub=$(grep -cE 'coctl event publish.*reason.*cond2' "$tmp/test6.sh" || true)
    if [[ "$publish_count" -eq 2 && "$end_count" -eq 2 && "$cond1_pub" -eq 1 && "$cond2_pub" -eq 1 ]]; then
        _selftest_pass "multiple sentinels patched independently"
    else
        _selftest_fail "test6" "expected 2 publish + 2 END + 1-each-reason, got publish=$publish_count end=$end_count cond1=$cond1_pub cond2=$cond2_pub"
    fi

    cat > "$tmp/test7.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.dryrun
EOF
    cp "$tmp/test7.sh" "$tmp/test7.before"
    process_file dry-run "$tmp/test7.sh" > /dev/null
    if cmp -s "$tmp/test7.before" "$tmp/test7.sh"; then
        _selftest_pass "dry-run does not write"
    else
        _selftest_fail "test7" "dry-run modified file"
    fi

    # Regression: embedded `"` in natural JSON must be escaped so the emitted double-quoted
    # payload parses.
    cat > "$tmp/test9.sh" <<'EOF'
#!/bin/sh
R=hello
# COPAD_HOOK_PUBLISH: claude.commit_blocked {"reason":"$R"}
EOF
    process_file install "$tmp/test9.sh"
    if grep -qE 'coctl event publish claude.commit_blocked --quiet "\{\\"reason\\":\\"\$R\\"\}"' "$tmp/test9.sh"; then
        _selftest_pass "double-quote payload is escaped for bash"
    else
        _selftest_fail "test9" "payload not escaped: $(grep coctl "$tmp/test9.sh")"
    fi
    # Run the patched script with `coctl` shimmed to echo its payload, proving the bash-level
    # quoting end to end.
    local payload_observed
    payload_observed=$(
        coctl() {
            printf '%s\n' "$5"
        }
        export -f coctl 2>/dev/null || true
        R=hello bash <<EOS
coctl() { printf '%s\n' "\$5"; }
$(cat "$tmp/test9.sh")
EOS
    )
    if [[ "$payload_observed" == '{"reason":"hello"}' ]]; then
        _selftest_pass "patched line expands \$VAR and ships valid JSON"
    else
        _selftest_fail "test9-exec" "expected '{\"reason\":\"hello\"}', got '$payload_observed'"
    fi

    # Test 11 (docs path): `$(...)` command
    # substitution in the sentinel survives quote-escape so the
    # docs-recommended `jq -n` recipe works. The patcher escapes `"`
    # uniformly, which inside bash double-quoted context becomes
    # literal `"` — exactly what jq expects in its argv.
    cat > "$tmp/test11.sh" <<'EOF'
#!/bin/sh
R='unsafe "value'
# COPAD_HOOK_PUBLISH: claude.commit_blocked $(printf '{"reason":"%s"}' "$R")
EOF
    process_file install "$tmp/test11.sh"
    # Run with coctl shimmed; expect the printf substitution to
    # have produced a literal JSON payload string (printf is not a
    # JSON-escaping tool, so this test just verifies the `$(...)`
    # round-trips, not that printf produces valid JSON — that's the
    # caller's responsibility per the docs).
    local payload_observed
    payload_observed=$(
        R='unsafe "value' bash <<EOS
coctl() { printf '%s\n' "\$5"; }
$(cat "$tmp/test11.sh")
EOS
    )
    if [[ "$payload_observed" == *'unsafe'* && "$payload_observed" == \{* ]]; then
        _selftest_pass "command substitution \$(...) survives patcher escape"
    else
        _selftest_fail "test11" "command substitution broke: '$payload_observed'"
    fi

    # Regression: a literal backslash in the JSON must survive; without the `\` escape bash
    # collapses `\\` to `\`.
    cat > "$tmp/test13.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.path_event {"path":"C:\\tmp\\file"}
EOF
    process_file install "$tmp/test13.sh"
    local payload_observed
    payload_observed=$(
        bash <<EOS
coctl() { printf '%s\n' "\$5"; }
$(cat "$tmp/test13.sh")
EOS
    )
    # JSON `\\` represents one literal backslash; the patcher must
    # preserve the byte stream so the action handler sees the same
    # JSON. Bash single quotes are literal, so `'{"path":"C:\\tmp\\file"}'`
    # is the 24-byte string with two-backslash pairs.
    if [[ "$payload_observed" == '{"path":"C:\\tmp\\file"}' ]]; then
        _selftest_pass "literal backslash in JSON round-trips"
    else
        _selftest_fail "test13" "expected double-backslash preserved, got: $payload_observed"
    fi

    # Test 14 (regression): file-level symlinks
    # are preserved — the patch lands on the target file, not on
    # the symlink itself. Dotfiles tools (stow) commonly symlink
    # individual hook files, and a naked `mv` would replace the
    # link with a regular file.
    mkdir -p "$tmp/real-hooks" "$tmp/link-hooks"
    cat > "$tmp/real-hooks/hook.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.symlink_test
EOF
    ln -s "$tmp/real-hooks/hook.sh" "$tmp/link-hooks/hook.sh"
    process_file install "$tmp/link-hooks/hook.sh"
    # The symlink itself must still exist and still point at the
    # real file (not be replaced with a regular file).
    if [[ -L "$tmp/link-hooks/hook.sh" ]] \
        && grep -qF 'coctl event publish claude.symlink_test' "$tmp/real-hooks/hook.sh"; then
        _selftest_pass "file-level symlink survives, target updated"
    else
        _selftest_fail "test14" "symlink replaced or target unmodified: link=$(stat -c '%F' "$tmp/link-hooks/hook.sh" 2>/dev/null) real-content=$(grep -c coctl "$tmp/real-hooks/hook.sh")"
    fi

    # Test 15 (regression): RELATIVE symlink with
    # the patcher run from an unrelated cwd. Plain `readlink` returns
    # the relative target unchanged; if `mv` interprets that
    # relative to its own cwd, it writes to the wrong place. The
    # canonicalization step must produce an absolute path.
    mkdir -p "$tmp/relreal" "$tmp/rellink"
    cat > "$tmp/relreal/hook.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.relative_symlink
EOF
    # Symlink with relative target.
    (cd "$tmp/rellink" && ln -s ../relreal/hook.sh hook.sh)
    # Run from a cwd that does NOT resolve the relative path.
    (cd / && process_file install "$tmp/rellink/hook.sh")
    if [[ -L "$tmp/rellink/hook.sh" ]] \
        && grep -qF 'coctl event publish claude.relative_symlink' "$tmp/relreal/hook.sh"; then
        _selftest_pass "relative symlink canonicalized to absolute target"
    else
        _selftest_fail "test15" "relative symlink mishandled (link=$(stat -c '%F' "$tmp/rellink/hook.sh" 2>/dev/null) real-content=$(grep -c coctl "$tmp/relreal/hook.sh"))"
    fi

    # Test 16 (regression): mode is read from the
    # symlink TARGET, not the symlink itself. A symlink is typically
    # 0777; reading the symlink mode and copying it to the temp
    # file would chmod the underlying target world-writable.
    cat > "$tmp/relreal/mode.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.mode_via_symlink
EOF
    chmod 0640 "$tmp/relreal/mode.sh"
    (cd "$tmp/rellink" && ln -sf ../relreal/mode.sh mode.sh)
    process_file install "$tmp/rellink/mode.sh"
    local final_mode
    final_mode=$(stat -L -c '%a' "$tmp/rellink/mode.sh" 2>/dev/null || stat -L -f '%OLp' "$tmp/rellink/mode.sh" 2>/dev/null || echo "")
    if [[ "$final_mode" == "640" ]]; then
        _selftest_pass "symlink target mode preserved (not 0777)"
    else
        _selftest_fail "test16" "expected mode 640, got $final_mode"
    fi

    # Test 12 (regression): sentinel inside an
    # indented case branch (the documented placement in
    # docs/harness-hooks.md § codex-review.sh). The parser must use
    # the index of `# COPAD_HOOK_PUBLISH:` within the line, not the
    # column-1 assumption that broke this.
    cat > "$tmp/test12.sh" <<'EOF'
#!/bin/sh
case "$VERDICT" in
    "APPROVED")
        # COPAD_HOOK_PUBLISH: claude.review_approved {"session":"$SESSION"}
        exit 0
        ;;
esac
EOF
    process_file install "$tmp/test12.sh"
    # The emitted publish must carry `claude.review_approved` as the
    # kind — NOT something derived from the slice misalignment.
    if grep -qE 'coctl event publish claude.review_approved --quiet' "$tmp/test12.sh"; then
        _selftest_pass "indented sentinel parses correctly"
    else
        _selftest_fail "test12" "indented sentinel slice misaligned: $(grep coctl "$tmp/test12.sh" || echo NONE)"
    fi

    # Test 10 (regression): patched file keeps its
    # original mode. GNU `chmod --reference` is Linux-only; on macOS
    # the mktemp 0600 mode would otherwise leak into the patched
    # hook and drop the executable bit. We read the original mode
    # via `stat` with both flag styles and reapply before the mv.
    cat > "$tmp/test10.sh" <<'EOF'
#!/bin/sh
# COPAD_HOOK_PUBLISH: claude.mode_test
EOF
    chmod 0755 "$tmp/test10.sh"
    process_file install "$tmp/test10.sh"
    if [[ -x "$tmp/test10.sh" ]]; then
        _selftest_pass "patched file retains executable bit"
    else
        _selftest_fail "test10" "patched file lost executable bit (mode=$(stat -c '%a' "$tmp/test10.sh" 2>/dev/null || stat -f '%OLp' "$tmp/test10.sh"))"
    fi

    # Test 8: sentinel at EOF with no trailing content.
    cat > "$tmp/test8.sh" <<'EOF'
#!/bin/sh
echo "before"
# COPAD_HOOK_PUBLISH: claude.eof
EOF
    process_file install "$tmp/test8.sh"
    if grep -qF 'coctl event publish claude.eof' "$tmp/test8.sh" \
        && grep -qF "$SENTINEL_END" "$tmp/test8.sh"; then
        _selftest_pass "sentinel at EOF gets paired"
    else
        _selftest_fail "test8" "EOF sentinel not paired: $(cat "$tmp/test8.sh")"
    fi

    if [[ "$failed" -eq 0 ]]; then
        printf '\nself-test: all passed\n'
        return 0
    else
        printf '\nself-test: FAILED\n' >&2
        return 1
    fi
}

# --- main ---

MODE="install"
HOOKS_DIRS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run" ;;
        --uninstall) MODE="uninstall" ;;
        --hooks-dir)
            shift
            [[ -z "${1:-}" ]] && { log "--hooks-dir requires a path"; exit 2; }
            HOOKS_DIRS+=("$1")
            ;;
        --self-test)
            run_self_test
            exit $?
            ;;
        -h|--help) usage 0 ;;
        *)
            log "unknown arg: $1"
            usage 2
            ;;
    esac
    shift
done

if [[ ${#HOOKS_DIRS[@]} -eq 0 ]]; then
    HOOKS_DIRS=("${DEFAULT_HOOKS_DIRS[@]}")
fi

if [[ "$MODE" == "install" ]] && ! command -v coctl >/dev/null 2>&1; then
    log "warning: coctl not on PATH — patches will be inert until you install copad"
fi

for dir in "${HOOKS_DIRS[@]}"; do
    scan_dir "$MODE" "$dir"
done

if [[ "$MODE" == "dry-run" ]]; then
    log "dry-run complete"
fi
