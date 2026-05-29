# KB Panel — Phase 22.3

> Status: shipped. Phase 22.3 lives at `plugins/docs/` (panel.html + Cargo scaffold) + `copad-core::context::ActiveDoc` + `kb.list` action. Orthogonal to the project-orchestration track ([22.2 / 22.4–22.7](./project-orchestration.md)) — depends only on Phase 22.1 (`pane_context.git_remote`) and the new `kb.list` extension to `copad-plugin-kb` plus the user's `dn`-maintained indices.

> Codex-plan deviations from the original design (all required to ship; rationale captured here so future readers see the gap between v1 design and v1 reality):
> - **§ 4 active-doc detection switched precmd → preexec** — `precmd` fires *after* foreground nvim exits, so the original `pgrep` design would have detected the editor only when it was no longer running. `preexec` sees the literal command line before execution and parses `nvim <arg>` directly.
> - **§ 7 "no kb protocol change" relaxed** — `kb.search` rejects empty queries (so it can't enumerate a folder) and exposes no mtime (so it can't power recent-edits). A new `kb.list` action was the smallest extension that powers both. See [kb-protocol.md § kb.list](./kb-protocol.md).
> - **shell-quoting `terminal.exec` paths** — the design's `JSON.stringify(path)` doesn't survive `$()`/backticks. The panel now passes paths through a POSIX single-quote escape before concatenating into the nvim command.
> - **`terminal.exec id field` corrected** — design said `panel_id`; the socket dispatcher accepts `id`.
> - **macOS parity not skippable** — `ContextService` lives in `copad-core` (Linux) AND `CopadCore.framework` (Swift mirror); the `ActiveDoc` data path needs both to keep `context.snapshot` shape symmetric.

## 1. Goal

A read+navigate UI for `~/docs` that gives the user the **awareness** features of Obsidian (backlinks, tag pane, related-notes, daily/folder nav) **without** trying to be an editor. Editing stays in nvim because the user's existing flow is `dn <verb>` → `$EDITOR` → write — that's where muscle memory lives. The panel surfaces what the CLI can't render interactively (graph-shaped views, faceted browsing) and routes every "open" through `terminal.exec "nvim <path>"`.

The user tried Obsidian's desktop client and abandoned it — three stale vaults exist (`~/posts/.obsidian/`, `~/dev/blog/ssdocs/content/.obsidian/`, `~/.config/obsidian/`). Signal: fixed sidebar + heavy UI + plugin sprawl didn't stick. A copad panel that is contextual (project-scoped), modal (toggle on/off), and read-only avoids those failure modes.

## 2. Slot layout — v1 ships 6 of 8 candidate slots

The full slot inventory considered:

| # | Slot | v1? | Reason |
|---|---|---|---|
| 1 | **Active note context** — when the focused pane has nvim open on a `~/docs` file, show its title + frontmatter summary | ✅ | Anchors the rest of the panel; no point rendering anything else without the active note |
| 2 | **Backlinks pane** — inbound links to the active note, from `.backlinks/index.tsv` | ✅ | Killer feature; `dn backlinks` already exposes this CLI-side, panel just renders interactively |
| 3 | **Tag tree** — 3-axis stack/domain/activity, hierarchical from `.tags/index.tsv` | ✅ | Tags are enforced 3-axis in `~/docs/CLAUDE.md`; tree view makes the policy visible |
| 4 | **Related notes** — `dn related` algorithm result (idf×2 + bidirectional×6 + same_repo×3 + cited_by_sources×2 + same_folder×1) | ✅ | `dn related` is one of the most-used `dn` subcommands; in-panel surfacing removes CLI roundtrip |
| 5 | **Per-folder browser** — sources/<type>/, topics/<category>/, daily/ tree | ✅ | Replaces `dn topics` + `find` for casual browse |
| 6 | **Recent edits + stale notes** — `dn stale` + `dn resurface` output | ✅ | Doubles as ambient awareness ("notes I should revisit") |
| 7 | Timeline navigator (daily/weekly/monthly calendar) | ❌ | Deferred to v1.1 — useful but not critical; `dn daily` covers the action path well |
| 8 | Quick switcher (fuzzy file picker, Cmd+P style) | ❌ | Deferred to v1.1 — `fzf` in terminal already does this; in-panel adds value mostly when typing-heavy. Revisit after dogfood. |

v1 is intentionally read+browse-shaped. Quick switcher gets revisited if v1 dogfooding shows the panel becoming the primary nav surface (in which case a keystroke-fast picker matters).

## 3. Data sources

The panel does not duplicate `dn`'s indexing — it consumes what `dn` already maintains. Three input categories:

### 3.1 `dn`-maintained incremental indices

- **`~/docs/.backlinks/index.tsv`** — one row per inbound link: `<target_path>\t<source_path>\t<link_text>`. dn rebuilds incrementally on file save (via `dn link-check` invocations and dn's own write paths). Panel reads the file directly; gitignored (machine-local).
- **`~/docs/.tags/index.tsv`** — one row per tag occurrence: `<tag>\t<file_path>`. Single-pass awk-built, also gitignored. Panel groups by tag prefix (`stack/*`, `domain/*`, `activity/*`) to render the 3-axis tree.

Both files are TSV with stable schemas (no header line — order is the contract). Panel uses a small TSV reader (~30 LOC JS) and re-reads on file mtime change.

### 3.2 `copad-plugin-kb` actions

The existing plugin (`plugins/kb/`) already provides:
- `kb.search { query, folder?, limit?, offset? }` → weighted ripgrep results
- `kb.read { id }` → file content by path-like id
- `kb.append { id, content, ensure?, default_template? }` → atomic append (single-syscall `O_APPEND`)
- `kb.ensure { id, default_template? }` → atomic create-if-missing (`renameat2(RENAME_NOREPLACE)`)

Panel reuses them verbatim. No new RPC. `kb.search` powers the search box (also used by related-notes recomputation when `.backlinks/index.tsv` doesn't have an entry yet). `kb.read` powers the active-note preview. `kb.append` is **not** wired in v1 — append is an editing action, and editing belongs in nvim.

### 3.3 SourceItem frontmatter

`~/docs/CLAUDE.md` defines the immutable SourceItem envelope: `source_type`, `source_id`, `canonical_url`, `captured_at`, `extractor`, `content_hash`, `dedupe_key`, plus `summary` / `tags` / `repo` / `created_at` / `updated_at` / `supersedes` fields. The panel parses frontmatter on-demand (lightweight YAML reader in JS, ~50 LOC, supports the subset of YAML the standard actually uses — strings, arrays-of-strings, ISO dates, no nested objects) to surface:

- `summary` — one-line preview under the title in any list view
- `tags` — chip rendering, click → filter to that tag
- `repo` — project scope match (see § 6)
- `supersedes` — chain badge ("supersedes <prev>"); v1 just shows the badge, deeper chain navigation deferred

## 4. Active doc detection

When the user's focused copad pane has nvim open on a `~/docs` file, the panel should surface that file as "active note" and bias the rest of the slots toward it. Two candidate detection paths considered:

| Approach | Cost | Coupling |
|---|---|---|
| **Shell precmd extension** (recommended) — extend the Phase 22.1 zsh hook to also check `pgrep -af "nvim.*<docs_root>"` and publish `doc.opened {path, panel_id}` when a match is found | ~10 LOC zsh, zero changes to nvim config | Lives in `examples/shell/copad-context.zsh` next to existing precmd publisher |
| nvim autocmd via `~/.config/nvim/` | ~10 LOC lua + Plugin install | Requires every user to update nvim config; tighter precision but adds an install step |
| coctl polling daemon | always-on resource cost | Rejected — over-engineered for the use case |

v1 picks **shell precmd extension**. The zsh hook gains a `__copad_doc_publish` function appended to `precmd_functions` alongside `__copad_context_publish`. Same env-gated emission (silent no-op outside copad-spawned shells). Same detached background subshell (must not block prompt). Same `coctl event publish` transport:

```sh
__copad_doc_publish() {
    setopt local_options no_monitor
    (
        local docs_root="${COPAD_DOCS_ROOT:-$HOME/docs}"
        local active
        active=$(pgrep -af "nvim ${docs_root}" 2>/dev/null \
            | awk '{ for(i=2;i<=NF;i++) if ($i ~ /'"$docs_root"'/) { print $i; exit } }' \
            | head -n1)
        if [[ -n "$active" ]]; then
            local panel_esc path_esc payload
            panel_esc=$(_copad_ctx_json_escape "$COPAD_PANEL_ID")
            path_esc=$(_copad_ctx_json_escape "$active")
            payload=$(printf '{"panel_id":"%s","path":"%s","v":1}' "$panel_esc" "$path_esc")
            "$__COPAD_CTX_COCTL" event publish doc.opened "$payload" --quiet >/dev/null 2>&1
        fi
    ) &!
    return 0
}
```

Event kind: `doc.opened`. Daemon-side: `copad-core::context::ContextService` gains an `active_doc_by_panel` map (mirror of `pane_context_by_panel`) plus an `active_doc()` accessor (returns `Option<ActiveDoc>` for the active panel). Panel subscribes via `copad.on("doc.opened")`.

Edge cases:
- **Multiple nvim instances**: pgrep returns the first match; v1 accepts that. Tighter would require pane-PID inheritance tracking which copad already does for `claude.start` (Phase 18.1) but is overkill for a passive viewer.
- **nvim closes**: no `doc.closed` event in v1; panel keeps showing the last active note until a new one publishes. Stale fine for a viewer.
- **Editing outside nvim** (e.g., `dn add-todo` writes to daily): not detected. The user invokes `dn` from terminal; if that's the focus, the panel just doesn't update — acceptable.

## 5. Editing constraint — terminal.exec → nvim only

Every action in the panel that needs to "edit" a note routes through:

```js
copad.action("terminal.exec", {
  panel_id: activePanelId,
  command: `nvim ${JSON.stringify(path)}`,
});
```

Active pane runs the command. If the active pane is a non-terminal panel (kb panel itself, projects panel), the call escalates to:

```js
copad.action("tabs.new", { ... }).then(({ tab_id, panel_id }) =>
  copad.action("terminal.exec", { panel_id, command: `nvim ${JSON.stringify(path)}` })
);
```

This is intentional — the panel does not implement text editing. The user has nvim for that, and trying to do better is exactly what makes Obsidian-style editors feel bloated. The constraint also keeps the v1 implementation simple (no rich text widgets, no syntax highlighting, no autosave).

Exceptions to "no writes":
- **None in v1.** `kb.append` is wired in the protocol but the panel does not surface an append UI. If a future "daily log scratch box" feature lands (deferred from the old Phase 22.2 dossier-panel plan), it ships as v1.1.

## 6. Project scoping

When the active pane's `pane_context.git_remote` is set, the panel default-filters to notes related to that project. Match logic, in order:

1. **Frontmatter `repo:` field** equals `git_remote` exactly — strongest signal, comes from the SourceItem envelope.
2. **Frontmatter `tags:` array** contains `repo/<git_remote>` — secondary signal.
3. **File path contains `<git_remote>` slug** — weakest, fallback for un-frontmatter'd notes (rare).

Filter is toggleable: a "Show all" pill at the top disables the project scope for the current panel session. State doesn't persist across panel reloads (intentional — switching panes should re-scope).

Slots affected by project scoping (when enabled):

- **Backlinks pane** — only inbound links from notes that also match the project filter
- **Tag tree** — counts only tags appearing in project-matching notes
- **Related notes** — `dn related` algorithm's `same_repo×3` boost essentially does this already; panel just amplifies by hard-filtering
- **Per-folder browser** — folders prune to those containing project-matching notes
- **Recent edits + stale notes** — only project-matching files

Slots **not** affected (always all):
- **Active note context** — by definition the active note is whatever's open
- Search box (when implemented) — global ripgrep, project filter is opt-in via folder param

## 7. Interaction with `copad-plugin-kb`

The KB panel is a separate plugin (`copad-plugin-docs`, HTML/JS) that calls into `copad-plugin-kb`'s existing action surface. No protocol change to kb. `kb-protocol.md` is **not** modified.

Plugin layout:

```
plugins/docs/                    # new in 22.3
├── Cargo.toml                   # service plugin host (panel-only — registers panel, no actions of its own)
├── src/main.rs                  # minimal host: panel registration + lifecycle
├── plugin.toml                  # panel descriptor
└── panel.html                   # the UI
```

The plugin's Rust side is essentially empty — it exists to register the panel and own its lifecycle. All data flow is JS-side via `copad.action()` calls into `kb.*` and `copad.on()` subscriptions to `doc.opened` + `pane.context_changed`.

## 8. Deferred (v1.1+ candidates, surfaced for traceability)

- **Wikilinks (`[[note-id]]` style)** — `~/docs/CLAUDE.md` mandates standard markdown relative paths and explicitly forbids wikilinks. v1 honors that. If the user ever switches the convention, the panel renderer would gain a `[[id]]` → resolved-path pass; not on the path today.
- **Interactive 2D graph view** — Obsidian's graph view is the most-cited "killer feature" but the user already tried and abandoned it. Tag tree + backlinks pane cover ~70% of the awareness value at much lower implementation cost. Reopen if dogfood shows specific gaps.
- **Daily log scratch box** — appending one-line entries to today's `daily/<YYYY-MM-DD>.md` via `kb.append` with timestamp prefix. Cut from v1 because editing-in-nvim is the constraint; revisit if append is universal-enough to be a special-case exception (signal: user reaches for `dn add-todo` from inside copad >5 times/day).
- **Search box with frontmatter facets** — `kb.search` integrated with tag/repo/date filters from the panel. Cut from v1 because `kb.search` action exists and panel can be added later without protocol change.
- **Quick switcher (Cmd+P style fuzzy picker)** — see § 2. Defer until dogfood signal.
- **Cross-note bulk operations** (e.g., "add `tag/foo` to all selected") — write-path expansion. Stays out by editing constraint.

## Related docs

- [roadmap.md § Phase 22.3](./roadmap.md#phase-22-context-aware-workstation-hub) — slice checklist
- [project-orchestration.md](./project-orchestration.md) — the parallel Phase 22.2 / 22.4–22.7 track (independent — they share only `pane_context.git_remote` as the project signal)
- [kb-protocol.md](./kb-protocol.md) — existing `kb.*` action contract (unchanged by 22.3)
- [context-bridge.md](./context-bridge.md) — Phase 22.1 (`pane_context.git_remote` source, same shell hook that gets the `doc.opened` extension)
- [decisions.md #48](./decisions.md#48-copad-native-port-of-project-orchestration-spine) — Phase 22 commitment (KB Panel is mentioned as the orthogonal 22.3 slice)
- `~/docs/CLAUDE.md` (in the user's docs repo, not this one) — SourceItem standard, 3-axis tag policy, immutability rules, link conventions
- `~/docs/scripts/dn` — the 2364-LOC CLI whose indices this panel renders
