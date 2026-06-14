# macOS GPU (Metal) Renderer Plan

**Status:** Slice 1 shipped 2026-06-12 (flag default off). Slice 2/3 pending.

## Slice 1 results (2026-06-12)

- Code: `MetalGridRenderer.swift` (pipeline + shaders + `GlyphAtlas`), `IMEPreeditOverlayView.swift`, GPU mode in `AlacrittyRenderView` (CAMetalLayer backing, `requestRepaint()` funnel, `gpuFrameDirty` latch), `CopadCore/GPURenderLogic.swift` (`CellQuadResolver` / `AtlasShelfPacker` / `GridQuadGeometry`, 38 unit tests).
- E2E (real app, isolated `XDG_CONFIG_HOME`, captures visually inspected + CPU-painter side-by-side): ANSI 16/256/truecolor, bold/italic/bold-italic, underline/strike/inverse/dim, 한글·漢字 wide cells, color emoji, box drawing (cell-aligned, lines connect), powerline glyphs, vim TUI (syntax + inverse status line), find-bar highlight quad, Cmd+= zoom (atlas reflow), theme hot-reload, wallpaper transparency (default cells transparent, explicit-bg opaque, cursor outline), hollow non-key cursor, 200k-line stream.
- Perf snapshot (not the slice-2 harness): `time seq 1 200000` wall 0.334s GPU vs 0.441s CPU painter; process CPU ~7% vs ~15% while streaming; idle parity (~1.5%).
- Known characteristics: fully-occluded window keeps its last presented frame until the display link resumes (dirty latch repaints on first visible tick) — observed once during e2e when another window covered the pane; correct-by-design but listed for slice-2 awareness.

### Slice 1 follow-up e2e (same day, user's real instance + config)

- Restart-to-GPU verified: both restored session tabs render via Metal (IOAccelerator graphics grew by one ~16MB atlas per pane — note: the atlas is **per-pane**; sharing it across panes is a slice-2 candidate for split-heavy use).
- Real config (JetBrainsMono Nerd Font 12pt + wallpaper + `[window] opacity 0.8`): pattern renders correctly; Nerd Font powerline/icon glyphs resolve via the cascade (the `?` boxes in the default-font run were missing-glyph parity, as suspected).
- **Korean IME composition on GPU — verified live**: raw-keycode ㅎㅏㄴㄱㅡㄹ with the 2-Set input source → 한 committed to the grid, 글 painted by `IMEPreeditOverlayView` (opaque bg + underline) mid-composition; Return commits, overlay clears, no residue.
- **Mouse drag-selection render — verified**: synthetic CGEvent drag paints the surface2 tint over the dragged span.
- **Drag → Cmd+C copy — verified by user (2026-06-14)**: real human drag + Cmd+C copies on the GPU path. (Synthetic System Events/CGEvent input had returned an empty clipboard on *both* painters during automation, so the automation gap was an input-injection artifact, not a GPU regression — confirmed.)

**Slice 1 manual-dogfood items are all closed.** Remaining work is slice 2 (perf harness, per-pane atlas sharing, ProMotion/resize polish) and slice 3 (default flip).
**Predecessor:** [`macos-renderer-migration-plan.md`](./macos-renderer-migration-plan.md) Phase 9 (deferred Metal path). The measurement gate was waived by explicit user decision — GPU rendering work starts now, ahead of a demonstrated CoreText bottleneck, to own the pipeline before scrollback/perf demands force a rushed port.

## Goal

Add a Metal-backed render path to `AlacrittyRenderView` behind a config flag, reaching **visual parity** (same content, same z-order, no gross drift — subpixel raster/baseline/decoration differences vs CoreText are accepted and documented) for everyday content, without touching input/IME/selection/damage logic (all of which stay exactly where they are).

## Current state (what we build on)

- `AlacrittyRenderView` (in `AlacrittyTerminalViewController.swift`) is a flipped, layer-backed NSView. CADisplayLink fires per refresh; `takeDamageRows` gates work; `snapshotCache` holds the latest `CopadTermFFI.Snapshot`; `draw(_:)` paints via CoreText (`drawRow` per-run: bg fill → CTLine glyphs; then cursor → selection tint → search tint → IME preedit overlay).
- Snapshot ABI is row-contiguous utf8 + run arrays (`CopadRun`: start/end col, fg/bg rgba, flags bold/italic/inverse/dim/strike, underline style+color). Colors resolve through `resolveColor` (sentinel 0 = default, palette indices, truecolor).
- CTLine cache keyed (text, font, fg, decorations); fallback-font cache for non-ASCII.

## Architecture decisions

### D1 — One view, two painters

`AlacrittyRenderView` keeps all input/IME/mouse/damage logic. GPU mode changes only the output stage:

- `wantsLayer = true` + `makeBackingLayer() → CAMetalLayer` when GPU mode is on. GPU mode is decided **before view init**: the controller resolves `config.rendererGPU && MTLCreateSystemDefaultDevice() != nil` and passes the resolved mode (plus the device) into the view initializer, so `makeBackingLayer()` never has to fall back after the layer class is already committed (codex R1).
- **Repaint requests unify through one helper** (codex R1 breaker #1): every current `needsDisplay = true` / `invalidateRows` site (PTY damage, theme/opacity hot-reload, background set/clear, search refresh, focus change, selection change, select-all, IME preedit updates, blink ticks) calls `requestRepaint()` instead. CPU mode: forwards to `setNeedsDisplay`. GPU mode: sets a `gpuFrameDirty` flag consumed by `displayLinkFired`. The flag clears **only after a frame is successfully encoded+presented** — if `nextDrawable` returns nil (occluded window, drawable starvation) the flag stays set and the next tick retries, so a damage drain consumed by `takeDamageRows` can never be lost (codex R1 breaker #2).
- `displayLinkFired` in GPU mode: drain damage → update `snapshotCache` → if damaged-or-dirty, `metalRenderer.render(snapshot:)`. Per-row dirty rects are irrelevant on GPU — every frame is a full redraw. Instance count at extreme grids (300×80 ≈ 24k cells) stays well within instanced-quad budgets, but the "trivially cheap" assumption is validated by the slice-2 perf harness before any default flip.
- `draw(_:)` early-returns in GPU mode (AppKit won't call it for a CAMetalLayer-backed view anyway, but belt-and-braces).

### D2 — IME preedit via CG overlay child view (slice 1)

The marked-text overlay (Korean/Japanese/Chinese composition) keeps the existing CoreText drawing logic, hosted in a dedicated transparent layer-backed child NSView (`IMEPreeditOverlayView`) sitting above the metal layer. This is **real porting scope, not a verbatim move** (codex R1 breaker #3): `paintMarkedText` currently depends on `draw(_:)`'s flipped-view + textMatrix scope and reads `snapshotCache.cursor` in place. The overlay view gets: its own flipped `draw(_:)` that re-establishes the textMatrix, cursor/cell-metrics/theme state pushed in by the parent on every change, explicit show/hide keyed off `markedText != nil`, and its own invalidation triggered from `setMarkedText`/`unmarkText` AND from each GPU frame while composing (cursor may move under the preedit). Rationale for CG-not-Metal: preedit is the highest-risk visual surface for this user (Korean daily driver); the drawing code itself is proven. Porting preedit into the Metal pass is slice 2+ (or never — compositing cost is one tiny transparent layer only while composing).

The cursor, selection, search highlights all move INTO the Metal pass (they're plain quads — easier on GPU than text).

### D3 — Config flag

`[renderer] gpu = true` (bool, default `false`). Parsed into `CopadConfig.rendererGPU`. Per-pane at creation time; config hot-reload affects newly created panes only (documented, matches how `transparent_default_bg` init-time flag already behaves). Metal availability is resolved **before** the render view is constructed (`MTLCreateSystemDefaultDevice() == nil` → log + construct the pane in CPU mode); the layer class is committed once and never swapped (codex R1).

### D4 — Glyph pipeline: CoreText shapes, Metal blits

Per run (same walk as `drawRow`):

1. **Shaping cache** keyed `(text, fontId)` → `[ShapedGlyph(glyphID, position, ctFont, isColor)]`, extracted via `CTLineCreateWithAttributedString` → `CTLineGetGlyphRuns` → `CTRunGetGlyphs/Positions/Attributes`. Color drops out of the key entirely (vs the CTLine cache which keys fg) — tint is per-instance data in the shader, so theme changes don't re-shape and don't flush the atlas.
2. **Atlas** — single `MTLTexture` RGBA8, 2048×2048 @ backing-scale pixels, shelf (row) packer. Entry keyed `(ctFont identity, glyphID)`. Rasterize via `CTFontDrawGlyphs` into a CGBitmapContext: monochrome glyphs drawn in white (shader multiplies by per-instance fg color); color glyphs (emoji — `kCTFontTraitColorGlyphs` on the run font) drawn as-is and flagged `colored` so the shader samples without tint. Glyph bounds from `CTFontGetBoundingRectsForGlyphs` + 1px padding. **Bearings are first-class** (codex R1): the bounding rect's origin (negative left bearing on italics, below-baseline descent, combining marks overhanging the cell) is stored per entry as a bearing offset; the per-instance quad position = `(penX + position.x + bearing.x, baselineY - bearing.maxY)` in pixels, NOT the cell origin — otherwise italic/emoji/combining glyphs clip or shift.
3. **Overflow policy (slice 1):** atlas full → flush-all + rebuild from the current frame's working set; log once. A terminal working set (couple hundred glyphs × 4 font styles) fits trivially in 2048²; overflow is a pathological case. Page growth / LRU eviction is slice 2.
4. **Flush triggers:** `setFont` / zoom (cell metrics change), backing scale change. NOT `setTheme` (atlas is colorless).

### D5 — Frame composition (single render pass, ordered draw calls)

Mirrors the CPU painter's z-order exactly:

1. Clear: `theme.background` (opaque mode) or `alpha 0` (transparent modes — `isTransparentBgActive` reused as-is; CAMetalLayer `isOpaque = false` so the wallpaper layer / window alpha shows through).
2. Cell-bg instanced quads — same skip logic as `drawRow` (default-sentinel + not-inverse skips in transparent mode; resolved == theme.bg skips in opaque mode). Resolve/inverse/dim logic extracted to a pure function shared in spirit with drawRow (see Testing).
3. Glyph instanced quads (textured, per-instance fg tint + colored flag). Underline/strike as untextured quads emitted alongside (single-fold of underline_style ≠ 0, same semantic as the CPU path) — geometry from font metrics (`CTFontGetUnderlinePosition` / `CTFontGetUnderlineThickness`, strike at ~mid-x-height) so it lands close to CoreText's attribute rendering, but **exact pixel match with NSUnderlineStyle output is explicitly not claimed** (codex R1 breaker #4); e2e captures gate on visual-parity (no gross drift), not pixel-diff zero.
4. Cursor: block (filled quad + cursor-cell glyphs re-emitted tinted `theme.background`), beam/underline (thin quads), hollow outline (4 thin quads) for non-key window; honors blink phase + 1px outline heuristic when `isTransparentBgActive` (NOT just `imageBackgroundActive` — the window-opacity mode needs the outline too; codex R1 breaker #5).
5. Selection tint quads (`surface2` @ 0.4 alpha; block vs row-wrapped span — port of `paintSelection` geometry).
6. Search-match tint quads (`accent` @ 0.45 — port of `paintSearchMatch` geometry).

Alpha blending on; one shader pair for untextured quads, one for textured glyph quads. Shaders compiled at runtime from a Swift string constant (`device.makeLibrary(source:)`) — avoids SwiftPM metallib bundling issues with the hand-rolled .app layout in `install-macos.sh`.

Vertex/instance buffers rebuilt per frame on CPU (≤ a few thousand instances for 80×24..300×80 grids; well under a millisecond) — no incremental buffer management in slice 1.

### D6 — What stays out of slice 1

- Performance benchmark harness + numbers vs CoreText (slice 2 — the flag defaults off, so no user regression risk while unmeasured).
- Atlas LRU / multi-page (slice 2, gated on real overflow).
- ProMotion 120Hz tuning, `presentsWithTransaction` resize-smoothness work (slice 2).
- Default flip `gpu = true` (slice 3, after a dogfood window — mirrors the 10a/10b pattern).

## Testing

### Unit (CopadCore — pure Swift, `swift test`)

New pure-logic types land in `CopadCore` (the executable target isn't test-importable):

- `AtlasShelfPacker` — placement, row growth, padding, overflow signaling.
- `CellQuadResolver` — port of drawRow's color-resolve decision table (default sentinel, inverse swap, dim alpha, transparent-mode skip, opaque-mode equal-bg skip) as a pure function over `(run flags, fg/bg rgba, mode)` → `(fgRGBA, bgRGBA?, skip)`. Table-driven tests covering every branch, including the inverse+default-bg-must-paint case.
- `GridGeometry` — cell rect / baseline / NDC transform math, cursor quad geometry per style, selection/search span → quad list (incl. block selection rectangle vs row-wrapped span).

### E2E (the thorough part — on this Mac, real app)

1. `./scripts/install-macos.sh` (release build, both staticlibs + swift), launch with `[renderer] gpu = true`.
2. Content correctness: `coctl call terminal.feed` a test pattern — 16/256/truecolor swatches, bold/italic/bold-italic, underline+strike, inverse, dim, Korean (한글 조합), CJK wide chars, emoji, box-drawing — then `screencapture -l <windowid>` and **visually inspect the PNG** (Read tool). Repeat identical pattern with `gpu = false`, capture, compare side-by-side for parity drift.
3. `terminal.read` / `terminal.state` shape checks via coctl (proves the socket surface is renderer-agnostic).
4. Interaction smoke: type into the pane (`terminal.output` event still fires), scroll a `seq 1 5000` buffer, Cmd+F search highlight visible in capture, cursor blink/styles via `printf` DECSCUSR, IME overlay — type 한글 mid-composition and capture.
5. Stability: resize the window (drawable resize), zoom Cmd+/-, theme hot-reload, wallpaper on/off (transparent bg modes), pane split with one GPU pane + one CPU pane coexisting.
6. Fallback: `gpu = true` with Metal unavailable can't be simulated here — covered by the nil-device guard + unit-testable decision, noted as untested-on-hardware.

### Quality gates

swiftformat + zero new build warnings (Swift quality gate), `cargo fmt`/`clippy` if Rust touched (not expected in slice 1), cross-review via codex before `~/save.sh`.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| G1 | CAMetalLayer + flipped NSView coordinate confusion (upside-down frame) | All geometry computed in our own cell-space → NDC transform; e2e screenshot catches inversion immediately. |
| G2 | Glyph raster mismatch vs CTLineDraw (weight/antialiasing differs subtly) | Same CTFontDrawGlyphs rasterizer CoreText uses; e2e side-by-side captures; font smoothing left to default. |
| G3 | Emoji / color glyph atlas entries render tinted or blank | `colored` flag per entry + dedicated e2e emoji line in the test pattern. |
| G4 | nextDrawable() blocking stalls main thread on occluded windows | Render only on damage (existing gate); on nil drawable the `gpuFrameDirty` flag stays set so the repaint retries next tick — never silently dropped. |
| G7 | Stale frames from non-damage invalidations (theme, search, focus, selection) | All invalidation sites route through `requestRepaint()`; grep-audit every `needsDisplay` assignment in the view during implementation. |
| G5 | Transparent-bg/wallpaper compositing breaks (image behind metal layer) | `isOpaque=false` + clear-alpha path mirrors CPU skip logic; e2e step 5 covers both modes. |
| G6 | Slice 1 scope too big for one pass | Feature checklist ordered so bg+glyphs+cursor land first; selection/search/IME-overlay are small follow-on commits within the slice. |

## Codex pressure-test results

**Round 1 (5 breakers + 4 risks, all accepted — no round 2 needed):**

- **B1** Damage-only rendering misses non-grid invalidations (theme/opacity/search/focus/selection/select-all/IME all use `needsDisplay` without PTY damage) → `requestRepaint()` unification + `gpuFrameDirty` flag in D1.
- **B2** nil `nextDrawable` after `takeDamageRows` consumed damage = permanently lost repaint → flag clears only after successful encode+present.
- **B3** "`paintMarkedText` verbatim reuse" was false — overlay view needs own coordinate scope, state propagation, show/hide, invalidation → D2 rewritten as explicit port scope.
- **B4** GPU underline/strike quads won't pixel-match CoreText `NSUnderlineStyle` rendering → font-metric-based quads + parity claim weakened to visual parity.
- **B5** Cursor outline condition stale (`imageBackgroundActive` vs the correct `isTransparentBgActive`) → fixed in D5.
- **R1** Glyph bearings (negative left bearing, descent, combining overhang) must be first-class in the atlas entry → D4 step 2.
- **R2** 300×80 = 24k instances, not "a few thousand" → assumption softened; slice-2 perf harness gates the default flip.
- **R3** "Pixel-parity" stronger than the visual-inspection test gate → claim weakened, gate documented as visual-parity.
- **R4** nil-device fallback must resolve before the layer class commits → D3 reordered (resolve at controller init, pass mode+device into view).

## Slices

- **Slice 1 (now):** flag + Metal painter at feature parity (list above) + unit tests + e2e protocol. Default **off**.
- **Slice 2:** perf harness (gpu vs coretext frame times on `yes`/`seq`/vim-scroll workloads), atlas LRU/multi-page, ProMotion/resize polish, preedit-in-Metal evaluation.
- **Slice 3:** default flip after dogfood window; decide whether the CoreText painter stays as fallback or follows SwiftTerm out (10b pattern).
