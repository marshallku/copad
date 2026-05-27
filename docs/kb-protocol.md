# Knowledge Base Action Protocol

> Status: protocol contract. Implementations live in service plugins (e.g.
> `copad-plugin-kb` Phase 9.3). `copad-core` ships the contract only — no KB
> code in core. This doc is what every KB-backed integration (triggers,
> meeting prep, derived markdown ingestion, future LLM context-building)
> reads against.

The KB protocol gives copad a stable, backend-agnostic vocabulary for "find,
read, write, ensure" over the user's notes. The first implementation is
grep + filename search over `~/docs` ([service-plugins.md](./service-plugins.md)
Phase 9.3). Future implementations may swap in SQLite FTS5 (Phase 13),
vector search, Notion, or Obsidian — without touching this protocol.

## Wire conventions

Three rules apply to every action in this document:

1. **The "Always present" column means "the field key MUST appear in
   this object."** Fields marked `yes` are part of the v1 wire
   contract: a compliant implementation always emits the key, even when
   the value is `null` (for `T|null` types).
2. **Nullable fields use `null`, not omission.** Any field typed
   `T|null` and marked Always present = yes appears in the JSON object
   with the key set to `null` when the value is unavailable. This gives
   callers a single shape to match against.
3. **Forward-compat additions use omission.** Brand-new fields not
   present in this document — added in a later revision without a
   protocol version bump — MAY be omitted entirely by older
   implementations. Callers MUST treat unknown / missing-but-undocumented
   fields as optional and ignore them.

Together: every key documented here is always present in compliant
output; future-revision keys may not be.

## Design constraints

The decisions below come from [service-plugins.md](./service-plugins.md)
D6 and D9. Re-stated here so the protocol is self-contained:

1. **Backend swap must not break callers.** Every field exposed here is
   shaped so a richer backend can populate it without a protocol bump.
   Hits carry a `score` whether the backend uses grep ranking, FTS5 BM25,
   or cosine similarity. Hits carry a `snippet` whether produced by
   line-context or by attention-weighted excerpt extraction.
2. **The `id` is a logical path-like key, the same shape across every
   backend.** It is `<folder>/<filename>`-style: forward-slash-separated
   segments, ending in a leaf, e.g. `meetings/2026-04-26-syncup.md`.
   Filesystem backends use it as the path relative to the KB root.
   Non-FS backends (Notion, Obsidian) MUST expose their content under
   the same logical-path shape, mapping to internal UUIDs / vault IDs /
   block IDs as a private translation layer.

   *Tradeoff:* an earlier draft of this protocol allowed non-FS backends
   to expose opaque UUID ids that callers round-trip without parsing.
   That model is genuinely more backend-agnostic in the abstract, but
   it's incompatible with the protocol surfaces that triggers and
   ingestion workflows already need: `kb.search.folder` prefix filter,
   parent-container auto-creation on `kb.ensure`, the `.raw/` search
   exclusion, and caller-constructed ids like `meetings/{event.id}.md`
   in trigger configs ALL key off path semantics. Choosing path-like
   ids forces non-FS backends to do an internal mapping; choosing
   opaque ids would have forced every caller to also exchange a parallel
   logical-path field. Path-like won as the simpler total contract.
3. **`path` is the optional FS-native rendering of `id`.** Filesystem
   backends populate `path` (absolute path) so triggers can chain to
   actions like `webview.open` (once the chained-trigger mechanism
   lands — see Open questions in service-plugins.md). Non-FS backends
   return `null` for `path` even though `id` is well-defined for them.
4. **Append-only writes for v1.** No `kb.delete`, no `kb.replace`. The
   ingestion model (Slack/Discord → derived markdown) is naturally append.
   Edits go through the user's editor of choice. Deletion and update can
   be added later without breaking the protocol.
5. **Path traversal is the plugin's problem.** Every action that takes
   an `id` must reject ids that escape the KB root. The protocol
   specifies the error code (`forbidden`); enforcement is per-plugin.
6. **Parent containers auto-create on write.** `kb.ensure` and
   `kb.append` (with `ensure=true`) MUST create any missing parent
   folders / collections / vault sections implied by the `id`. For
   filesystem backends this is `mkdir -p` semantics on the parent
   directory. For Notion/Obsidian or similar non-FS backends, the
   equivalent operation is creating any missing intermediate
   collection nodes. Without this guarantee, ingestion workflows that
   write `threads/<topic>.md` would have to seed the `threads/`
   directory beforehand on every backend, and the meeting-prep
   trigger would fail the first time `meetings/` doesn't exist.

## Folder conventions

Recommended layout under the KB root (`~/docs` for the file-based plugin):

| Folder        | Purpose                                                                       |
| ------------- | ----------------------------------------------------------------------------- |
| `meetings/`   | Per-event prep notes. Created by the Calendar plugin via `kb.ensure`.         |
| `people/`     | Per-person notes. Manually authored.                                          |
| `threads/`    | LLM-derived markdown summaries of messenger conversations (Phase 11+).        |
| `notes/`      | Free-form user notes.                                                         |
| `.raw/`       | Source-of-truth dumps from external services. Not surfaced to `kb.search`.   |

**`.raw/` is a protocol-level exclusion**, not a plugin convention.
Compliant backends MUST omit any document whose `id` begins with `.raw/`
from `kb.search` results. `kb.read` and `kb.append` still operate on
`.raw/` ids when the caller passes one explicitly — ingestion pipelines
need that to write raw archives. The exclusion is asymmetric on purpose:
write-by-id is allowed; surface-via-search is not. This keeps backend
swaps (file → Notion → Obsidian) from materially changing what shows up
in user-facing search.

## Actions

All actions follow the standard service-plugin RPC shape: copad sends
`action.invoke` with `{name, params}`, the plugin replies on the same
request id with `{ok, result}` or `{ok=false, error}`.

### `kb.search`

Find documents matching a query. Ranked.

**Request:**
```json
{
  "query": "weekly sync",
  "limit": 20,
  "offset": 0,
  "folder": "meetings"
}
```

| Field    | Type   | Required | Notes                                                                       |
| -------- | ------ | -------- | --------------------------------------------------------------------------- |
| `query`  | string | yes      | Free-text. Backends interpret per their capability (literal, FTS, semantic).|
| `limit`  | int    | no       | Default 20. Max enforced per-plugin (suggested cap 100).                    |
| `offset` | int    | no       | Default 0. For pagination.                                                  |
| `folder` | string | no       | Optional folder to scope the search (e.g. `"meetings"`). Trust-boundary inputs split into shape vs traversal: shape problems (empty string, embedded nul bytes) → `invalid_params`; trust-boundary violations (`..` traversal, absolute paths) → `forbidden`. The match is **directory-segment prefix**, not raw string prefix: `"meetings"` matches `meetings/foo.md` and `meetings/2026/foo.md`, but NOT `meetings-archive/foo.md`. Trailing slash on the input is allowed and ignored. |

**Response:**
```json
{
  "hits": [
    {
      "id": "meetings/2026-04-26-syncup.md",
      "title": "Weekly Sync — Eng Team",
      "score": 12.4,
      "snippet": "…discussed the **weekly sync** cadence and rotation…",
      "path": "/home/user/docs/meetings/2026-04-26-syncup.md",
      "match_kind": "fulltext"
    }
  ],
  "total": 1
}
```

Top-level response fields:

| Field   | Type           | Always present | Notes                                                                 |
| ------- | -------------- | -------------- | --------------------------------------------------------------------- |
| `hits`  | KbHit[]        | yes            | Ranked list, length ≤ `limit`. Empty array `[]` (not `null`) when no matches. |
| `total` | int\|null      | yes            | Total matches across the full result set (for pagination). `null` if the backend can't cheaply compute it (key still present). |

Each `KbHit` element:

| Field         | Type           | Always present | Notes                                                                 |
| ------------- | -------------- | -------------- | --------------------------------------------------------------------- |
| `id`          | string         | yes            | Stable handle. Pass back to `kb.read` / `kb.append` / `kb.ensure`.    |
| `title`       | string\|null   | yes            | Best-effort: frontmatter `title`, first H1, or filename without ext. `null` if no reasonable title can be derived. |
| `score`       | float          | yes            | Higher = better. Scale is implementation-defined; relative only.      |
| `snippet`     | string         | yes            | Plain-text excerpt around the match. Backends may inline `**…**` or similar markdown around hits — callers should treat as display text. |
| `path`        | string\|null   | yes            | Absolute filesystem path. `null` for non-FS backends.                 |
| `match_kind`  | string         | yes            | One of `"filename"`, `"fulltext"`, `"semantic"` (more values may be added in future revisions without a protocol bump). Callers that don't recognize the value MUST treat the hit as opaque-but-valid. |

**Errors:** `invalid_params` (missing `query`, malformed `folder`),
`forbidden` (`folder` escapes KB root), `io_error`. Note: when the plugin
is in supervisor-enforced degraded mode (manifest declared this action
but the runtime omitted it from its `provides` reply) the caller sees
`service_degraded` from the supervisor BEFORE the request reaches the
plugin — `not_implemented` here is for plugin-internal "the request
shape is recognized but this code path isn't built yet."

### `kb.read`

Read a document's full content by id.

**Request:**
```json
{ "id": "meetings/2026-04-26-syncup.md" }
```

| Field | Type   | Required | Notes                                                |
| ----- | ------ | -------- | ---------------------------------------------------- |
| `id`  | string | yes      | The id from a `kb.search` hit, or constructed by the caller (e.g. `meetings/{event.id}.md`). |

**Response:**
```json
{
  "id": "meetings/2026-04-26-syncup.md",
  "content": "# Weekly Sync — Eng Team\n\n…",
  "frontmatter": { "title": "Weekly Sync — Eng Team", "tags": ["sync"] },
  "path": "/home/user/docs/meetings/2026-04-26-syncup.md"
}
```

| Field         | Type           | Always present | Notes                                                                 |
| ------------- | -------------- | -------------- | --------------------------------------------------------------------- |
| `id`          | string         | yes            | Echoes the request `id` so async callers can correlate without holding state. |
| `content`     | string         | yes            | Full document text (markdown). Frontmatter, if present, is INCLUDED in the raw content so callers that want byte fidelity have it. |
| `frontmatter` | object\|null   | yes            | Parsed YAML frontmatter if the document begins with `---\n…\n---\n`; `null` otherwise (key always present). |
| `path`        | string\|null   | yes            | Same semantics as in `kb.search`.                                     |

**Errors:** `invalid_params` (missing `id`, wrong type), `invalid_id`
(string is shaped right but malformed — empty, contains nul bytes, etc.),
`not_found` (id doesn't resolve), `forbidden` (id escapes KB root),
`io_error`.

**Concurrent-write visibility:** the protocol provides whole-file
snapshot isolation only against `kb.ensure` (because the temp-file +
no-replace-rename algorithm makes the create transition atomic). It
does NOT promise snapshot isolation against `kb.append` — a reader
running while an append is in flight may observe the document before
or after the append, OR a state with the appended bytes partially
materialized. Callers that need consistency against ongoing ingestion
should poll `kb.search` (which reflects the post-append state once the
backend has indexed it) or call `kb.read` AFTER the writer has
acknowledged completion. This weaker guarantee matches what regular
filesystems actually provide; a stronger one would require explicit
file locking that the file-based plugin won't take.

### `kb.append`

Append text to a document. Creates the document if `ensure=true` (default
false — distinct call from `kb.ensure` if the caller wants only "create
if missing"). When `ensure=true` triggers creation, missing parent
containers are also created (same `mkdir -p` semantics as `kb.ensure`).

**Request:**
```json
{
  "id": "threads/q2-roadmap-discussion.md",
  "content": "\n## 2026-04-26 update\n\nSynced with @alice — …\n",
  "ensure": true
}
```

| Field     | Type   | Required | Notes                                                                 |
| --------- | ------ | -------- | --------------------------------------------------------------------- |
| `id`      | string | yes      | Target document.                                                      |
| `content` | string | yes      | Text appended verbatim. Caller is responsible for leading newline if separation matters. |
| `ensure`  | bool   | no       | Default `false`. If `true`, create the file (with empty initial content) before appending if missing. If `false` and missing → `not_found`. |

**Response:**
```json
{
  "id": "threads/q2-roadmap-discussion.md",
  "bytes_written": 87,
  "created": false,
  "path": "/home/user/docs/threads/q2-roadmap-discussion.md"
}
```

| Field           | Type           | Always present | Notes                                                                 |
| --------------- | -------------- | -------------- | --------------------------------------------------------------------- |
| `id`            | string         | yes            | Echoes the request `id`.                                              |
| `bytes_written` | int            | yes            | Bytes appended (UTF-8 byte count). Useful for ingestion observability.|
| `created`       | bool           | yes            | True if `ensure=true` triggered file creation in this call.            |
| `path`          | string\|null   | yes            | Same semantics as elsewhere.                                          |

**Concurrency:** filesystem backends MUST issue the entire `content`
payload as a single `write(2)` syscall on a file opened with `O_APPEND`,
so concurrent `kb.append` calls from different services can't interleave
mid-payload AGAINST EACH OTHER.

**Concurrent ensure-creation race:** when two `kb.append` calls with
`ensure=true` race on a missing `id`, the same exactly-one-creator rule
that `kb.ensure` defines applies: exactly one call returns
`created=true`, all others return `created=false`. Both calls' `content`
is appended to the resulting file (one before the other; ordering
between them is unspecified beyond "neither is lost"). The implementation
path is the same — exclusive create via temp-file + `RENAME_NOREPLACE`
— with the appended payload written either as part of the temp file
(for the winner) or via the normal `O_APPEND` path on the now-existing
file (for the loser). POSIX guarantees `O_APPEND` writes are
atomic relative to the file offset only per syscall; splitting a payload
across multiple write calls would break that guarantee, so callers who
need atomic multi-line appends MUST pass the whole block in one
`kb.append` call rather than one call per line.

This single-syscall rule does NOT give concurrent `kb.read` a snapshot
view — see the `kb.read` "Concurrent-write visibility" note above.
Append-vs-append is interleave-safe; append-vs-read is not.

**Errors:** `invalid_params` (missing `id` or `content`, wrong type),
`invalid_id`, `not_found` (id missing and `ensure=false`), `forbidden`,
`io_error`.

### `kb.ensure`

Create-or-return a document. Idempotent: calling repeatedly with the same
id returns the same path with `created=false` after the first call.
Missing parent containers in the `id` (`mkdir -p` for FS backends,
intermediate collection creation for Notion/Obsidian-style backends)
are created implicitly per the wire convention rule (5) above.

**Request:**
```json
{
  "id": "meetings/abc123.md",
  "default_template": "# Meeting prep\n\n## Goals\n\n- \n\n## Notes\n\n"
}
```

| Field              | Type   | Required | Notes                                                                 |
| ------------------ | ------ | -------- | --------------------------------------------------------------------- |
| `id`               | string | yes      | Target document.                                                      |
| `default_template` | string | no       | Initial content if the file is created. Treated as a literal string — interpolation of `{event.*}` etc. happens at the trigger layer BEFORE the call, so the plugin sees the final string. |

**Response:**
```json
{
  "id": "meetings/abc123.md",
  "created": true,
  "path": "/home/user/docs/meetings/abc123.md"
}
```

| Field     | Type           | Always present | Notes                                                                 |
| --------- | -------------- | -------------- | --------------------------------------------------------------------- |
| `id`      | string         | yes            | Echoes the request `id`.                                              |
| `created` | bool           | yes            | True iff this call created the file.                                  |
| `path`    | string\|null   | yes            | Same semantics as elsewhere.                                          |

**Atomicity and race safety:** filesystem backends MUST satisfy BOTH
properties below in a single algorithm:

1. **Exactly-one-creator under concurrent ensure.** If two calls race
   to create the same `id`, exactly one returns `created=true` (with
   the template it provided) and every other concurrent caller returns
   `created=false`. Callers MUST NOT see `created=true` from more than
   one call for the same `id`.
2. **No torn reads.** A concurrent `kb.read` MUST observe either the
   pre-creation state (`not_found`) or the post-creation state (full
   `content`). It MUST NOT observe a partially written template.

The required algorithm is **temp-file + atomic rename with no-replace
semantics**: write the template to a sibling temp file, then move it
into place with `renameat2(..., RENAME_NOREPLACE)` (Linux) or the
equivalent on the target platform. The no-replace flag enforces (1)
because losing the race surfaces as `EEXIST` from the rename; the
rename's atomicity enforces (2). `open(O_CREAT|O_EXCL)` followed by
in-place writes satisfies (1) but NOT (2) and is therefore
INSUFFICIENT.

**Errors:** `invalid_params` (missing `id`, wrong type), `invalid_id`,
`forbidden`, `io_error`.

## Error codes (shared)

KB plugins SHOULD use these codes; they keep callers' error handling
backend-independent. Unknown codes propagate verbatim.

| Code              | Origin            | Meaning                                                                  |
| ----------------- | ----------------- | ------------------------------------------------------------------------ |
| `not_found`       | plugin            | The id does not resolve to a document.                                   |
| `forbidden`       | plugin            | The id escapes the KB root (e.g. contains `..` traversal).               |
| `invalid_id`      | plugin            | The id is malformed (empty, contains nul bytes, etc).                    |
| `invalid_params`  | plugin            | A required field is missing or malformed (e.g. `query` empty in search). |
| `not_implemented` | plugin            | The plugin recognized the request shape but the code path isn't built (e.g. an action declared in both manifest AND init reply that the developer hasn't filled in yet — should normally be caught earlier by the runtime narrowing its `provides`). |
| `io_error`        | plugin            | Filesystem or backend storage failure. Message carries detail.           |
| `service_degraded`| **supervisor**    | The runtime omitted this manifest-approved action from its `initialize` reply. Surfaced by the supervisor before the request reaches the plugin (Phase 9.1 asymmetric subset rule). Callers MUST handle this in addition to the plugin-origin codes above. |
| `service_unavailable` | **supervisor** | Service isn't running and this action can't trigger spawn (e.g. `onStartup`/`onEvent` service in `Stopped`/`Failed`, or `onAction:<glob>` mismatch). Phase 9.1 lifecycle. |

## Forward compatibility notes

- **`kb.search`** is the action most likely to gain fields. New optional
  response fields (e.g. `vector_distance`, `highlights: [[start, end], …]`,
  `match_kind` extensions) MUST be added with default-omit semantics so
  existing callers ignore them silently.
- **`kb.read`** may grow a `format` request field (`"text"` | `"html"`)
  if rich rendering becomes necessary, but the default stays markdown-text
  for backward compatibility.
- **`kb.append`** may grow `mode` (`"append"` | `"prepend"` | `"replace"`)
  in a future revision, with `"append"` remaining the default. Callers
  that don't pass `mode` get the v1 behavior.
- **`kb.ensure`** is intentionally minimal — adding a `frontmatter` field
  that the plugin merges into the template is the most likely extension.

A protocol version bump is reserved for breaking changes (renaming a
required field, changing a required type, removing an action). Anything
that can be added with default-omit goes in without a version bump.

## Cross-references

- [service-plugins.md](./service-plugins.md) — D6 (KB protocol in core, impl in plugin), D9 (defer indexing upgrades)
- [workflow-runtime.md](./workflow-runtime.md) — Context Service consumes KB hits as part of `workflow.context`
- [roadmap.md](./roadmap.md) — Phase 9.3 (first-party `copad-plugin-kb`), Phase 13 (FTS/embedding upgrade)
