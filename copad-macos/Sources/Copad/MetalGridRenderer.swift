import AppKit
import CCopadTerm
import CopadCore
import CoreText
import Metal
import QuartzCore

/// Metal painter for `AlacrittyRenderView`'s GPU mode
/// (docs/macos-gpu-renderer-plan.md slice 1, `[renderer] gpu = true`).
///
/// Division of labor: the render view keeps ALL input / IME / damage /
/// snapshot logic and decides *when* to render; this class owns *how*
/// — CoreText shapes text, glyphs land in a texture atlas, and every
/// visual element (cell bg, glyph, decoration, cursor, selection,
/// search highlight) becomes an instanced quad in a single ordered
/// draw call. Instance order in the buffer IS the z-order: Metal
/// rasterizes instances in order, so the buffer is laid out exactly
/// like the CPU painter's paint sequence.
///
/// Coordinates: quads are computed in device pixels with origin
/// top-left / y-down (the flipped convention the view already uses);
/// the vertex shader maps pixels → NDC, so none of the grid math
/// changes between the two painters.
@MainActor
final class MetalGridRenderer {
    // MARK: - Shaders

    /// Compiled at runtime via `makeLibrary(source:)` — the hand-rolled
    /// .app bundle from install-macos.sh has no SwiftPM resource
    /// processing, so a default.metallib is not guaranteed to exist.
    /// The ~ms compile happens once per renderer (per pane).
    private static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;

    struct QuadInstance {
        float2 origin;
        float2 size;
        float2 uvOrigin;
        float2 uvSize;
        float4 color;   // non-premultiplied
        uint4  kindPad; // x: 0 = solid, 1 = mono glyph, 2 = color glyph
    };

    struct VSOut {
        float4 pos [[position]];
        float2 uv;
        float4 color;
        uint kind [[flat]];
    };

    vertex VSOut quad_vs(
        uint vid [[vertex_id]],
        uint iid [[instance_id]],
        const device QuadInstance *instances [[buffer(0)]],
        constant float2 &viewportPx [[buffer(1)]]
    ) {
        QuadInstance inst = instances[iid];
        float2 corner = float2(vid & 1, vid >> 1);
        float2 px = inst.origin + corner * inst.size;
        VSOut out;
        out.pos = float4(px.x / viewportPx.x * 2.0 - 1.0,
                         1.0 - px.y / viewportPx.y * 2.0,
                         0.0, 1.0);
        out.uv = inst.uvOrigin + corner * inst.uvSize;
        out.color = inst.color;
        out.kind = inst.kindPad.x;
        return out;
    }

    fragment float4 quad_fs(
        VSOut in [[stage_in]],
        texture2d<float> atlas [[texture(0)]]
    ) {
        constexpr sampler s(mag_filter::nearest, min_filter::nearest,
                            address::clamp_to_edge);
        // Output is premultiplied (pipeline blends one /
        // one-minus-source-alpha).
        switch (in.kind) {
        case 0:
            return float4(in.color.rgb * in.color.a, in.color.a);
        case 1: {
            float cov = atlas.sample(s, in.uv).a;
            return float4(in.color.rgb * in.color.a * cov, in.color.a * cov);
        }
        default: {
            // Color glyph (emoji): atlas texels are already
            // premultiplied by the CGBitmapContext; tint is ignored,
            // only the instance alpha scales.
            return atlas.sample(s, in.uv) * in.color.a;
        }
        }
    }
    """

    /// Memory layout mirror of the MSL `QuadInstance` (both 64-byte
    /// stride: float2×4 @ 0/8/16/24, float4 @ 32, uint4 @ 48).
    private struct QuadInstance {
        var origin: SIMD2<Float>
        var size: SIMD2<Float>
        var uvOrigin: SIMD2<Float> = .zero
        var uvSize: SIMD2<Float> = .zero
        var color: SIMD4<Float>
        var kindPad: SIMD4<UInt32> = .zero
    }

    private enum QuadKind: UInt32 {
        case solid = 0
        case monoGlyph = 1
        case colorGlyph = 2
    }

    // MARK: - Inputs

    struct Fonts {
        let regular: NSFont
        let bold: NSFont
        let italic: NSFont
        let boldItalic: NSFont

        func pick(bold isBold: Bool, italic isItalic: Bool) -> NSFont {
            switch (isBold, isItalic) {
            case (true, true): boldItalic
            case (true, false): bold
            case (false, true): italic
            case (false, false): regular
            }
        }
    }

    /// Cell metrics in points + the backing scale that converts them
    /// to device pixels. Changing any of these flushes the atlas.
    struct Metrics: Equatable {
        let cellWidth: CGFloat
        let cellHeight: CGFloat
        let ascent: CGFloat
        let scale: CGFloat
    }

    /// Theme-derived colors packed RGBA32 (the CopadCore convention).
    struct ThemeColors {
        let defaultFg: UInt32
        let defaultBg: UInt32
        let accent: UInt32
        let surface2: UInt32
        let palette: [UInt32]
    }

    /// Everything about *this* frame that isn't grid content.
    struct FrameState {
        let viewSizePoints: CGSize
        let transparentMode: Bool
        let blinkVisible: Bool
        let isKeyWindow: Bool
    }

    // MARK: - State

    let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private weak var layer: CAMetalLayer?

    private var fonts: Fonts
    private var metrics: Metrics
    private var atlas: GlyphAtlas

    /// (text, base font) → shaped glyph list. Tint lives per-instance,
    /// so unlike the CPU painter's CTLine cache this survives theme
    /// changes untouched. Flush-all when the (rare) cap is hit —
    /// terminal working sets stay far below it.
    private var shapeCache: [ShapeKey: [PositionedGlyph]] = [:]
    private static let shapeCacheMax = 2048

    /// Codepoint → cascade-resolved font, mirroring the CPU painter's
    /// `resolveRunFont` cache. Keyed by the first non-ASCII scalar.
    private var fallbackCache: [UInt32: NSFont] = [:]

    private struct ShapeKey: Hashable {
        let text: String
        let fontId: ObjectIdentifier
    }

    private struct PositionedGlyph {
        let glyph: CGGlyph
        let font: CTFont
        /// Typographic position relative to the run origin, points.
        let position: CGPoint
        let isColor: Bool
    }

    init?(layer: CAMetalLayer, device: MTLDevice, fonts: Fonts, metrics: Metrics) {
        guard let queue = device.makeCommandQueue() else { return nil }
        let library: MTLLibrary
        do {
            library = try device.makeLibrary(source: Self.shaderSource, options: nil)
        } catch {
            FileHandle.standardError.write(Data("[copad] Metal shader compile failed: \(error)\n".utf8))
            return nil
        }
        guard let vs = library.makeFunction(name: "quad_vs"),
              let fs = library.makeFunction(name: "quad_fs")
        else { return nil }

        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = vs
        desc.fragmentFunction = fs
        let attachment = desc.colorAttachments[0]!
        attachment.pixelFormat = .bgra8Unorm
        attachment.isBlendingEnabled = true
        // Premultiplied-alpha over: the fragment shader premultiplies.
        attachment.sourceRGBBlendFactor = .one
        attachment.sourceAlphaBlendFactor = .one
        attachment.destinationRGBBlendFactor = .oneMinusSourceAlpha
        attachment.destinationAlphaBlendFactor = .oneMinusSourceAlpha

        do {
            pipeline = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            FileHandle.standardError.write(Data("[copad] Metal pipeline create failed: \(error)\n".utf8))
            return nil
        }

        guard let atlas = GlyphAtlas(device: device, scale: metrics.scale) else { return nil }

        self.device = device
        self.queue = queue
        self.layer = layer
        self.fonts = fonts
        self.metrics = metrics
        self.atlas = atlas
    }

    /// Font / zoom / backing-scale change: shaped positions and atlas
    /// rasterizations are stale — rebuild lazily from the next frame.
    func setFonts(_ newFonts: Fonts, metrics newMetrics: Metrics) {
        fonts = newFonts
        metrics = newMetrics
        atlas.flush(scale: newMetrics.scale)
        shapeCache.removeAll(keepingCapacity: true)
        fallbackCache.removeAll(keepingCapacity: true)
    }

    // MARK: - Frame

    /// Build + encode + present one frame. Returns false when no
    /// drawable is available (occluded window, drawable starvation) —
    /// the caller MUST keep its dirty flag set and retry next tick,
    /// because the damage that triggered this call is already drained.
    func render(
        snap: CopadTermFFI.Snapshot,
        theme: ThemeColors,
        state: FrameState,
    ) -> Bool {
        guard let layer else { return true }
        let scale = metrics.scale
        let drawableSize = CGSize(
            width: max(1, state.viewSizePoints.width * scale),
            height: max(1, state.viewSizePoints.height * scale),
        )
        if layer.drawableSize != drawableSize {
            layer.drawableSize = drawableSize
        }

        // Two attempts: an atlas overflow mid-build invalidates every
        // uv already emitted this frame, so flush and rebuild once
        // from the current working set (plan D4 overflow policy).
        var instances: [QuadInstance] = []
        for attempt in 0 ..< 2 {
            instances.removeAll(keepingCapacity: true)
            if buildFrame(snap: snap, theme: theme, state: state, into: &instances) {
                break
            }
            atlas.flush(scale: scale)
            if attempt == 1 {
                FileHandle.standardError.write(
                    Data("[copad] glyph atlas overflow persists after rebuild — frame may drop glyphs\n".utf8),
                )
            }
        }

        guard let drawable = layer.nextDrawable() else { return false }

        let pass = MTLRenderPassDescriptor()
        let attachment = pass.colorAttachments[0]!
        attachment.texture = drawable.texture
        attachment.loadAction = .clear
        attachment.storeAction = .store
        attachment.clearColor = clearColor(theme: theme, state: state)

        guard let cmd = queue.makeCommandBuffer(),
              let encoder = cmd.makeRenderCommandEncoder(descriptor: pass)
        else { return false }

        if !instances.isEmpty,
           let buffer = device.makeBuffer(
               bytes: instances,
               length: MemoryLayout<QuadInstance>.stride * instances.count,
               options: .storageModeShared,
           )
        {
            var viewport = SIMD2<Float>(Float(drawableSize.width), Float(drawableSize.height))
            encoder.setRenderPipelineState(pipeline)
            encoder.setVertexBuffer(buffer, offset: 0, index: 0)
            encoder.setVertexBytes(&viewport, length: MemoryLayout<SIMD2<Float>>.size, index: 1)
            encoder.setFragmentTexture(atlas.texture, index: 0)
            encoder.drawPrimitives(
                type: .triangleStrip,
                vertexStart: 0,
                vertexCount: 4,
                instanceCount: instances.count,
            )
        }
        encoder.endEncoding()
        cmd.present(drawable)
        cmd.commit()
        return true
    }

    /// Transparent modes clear to alpha-0 so the layer background /
    /// wallpaper image view behind shows through — the GPU equivalent
    /// of the CPU painter skipping its opaque bounds fill.
    private func clearColor(theme: ThemeColors, state: FrameState) -> MTLClearColor {
        if state.transparentMode {
            return MTLClearColor(red: 0, green: 0, blue: 0, alpha: 0)
        }
        let bg = rgbaToFloat4(theme.defaultBg)
        return MTLClearColor(red: Double(bg.x), green: Double(bg.y), blue: Double(bg.z), alpha: 1)
    }

    // MARK: - Frame assembly

    /// Append the full frame's instances in z-order. Returns false on
    /// atlas overflow (caller flushes + retries the whole frame).
    private func buildFrame(
        snap: CopadTermFFI.Snapshot,
        theme: ThemeColors,
        state: FrameState,
        into instances: inout [QuadInstance],
    ) -> Bool {
        let rows = snap.rows
        let cellW = metrics.cellWidth
        let cellH = metrics.cellHeight

        // 1. Cell backgrounds. (The bounds clear already painted the
        //    default bg, or alpha-0 in transparent mode.)
        for row in 0 ..< rows {
            let runs = snap.rowRuns(row)
            for i in 0 ..< runs.count {
                let run = runs[i]
                let resolved = CellQuadResolver.resolve(
                    fgPacked: run.fg_rgba,
                    bgPacked: run.bg_rgba,
                    flags: run.flags,
                    transparentMode: state.transparentMode,
                    palette: theme.palette,
                    defaultFg: theme.defaultFg,
                    defaultBg: theme.defaultBg,
                )
                guard resolved.paintBg else { continue }
                appendSolid(
                    &instances,
                    quad: GPUQuad(
                        x: Double(run.start_col) * cellW,
                        y: Double(row) * cellH,
                        width: Double(run.end_col - run.start_col) * cellW,
                        height: cellH,
                    ),
                    rgba: resolved.bg,
                )
            }
        }

        // 2. Glyphs + per-run decorations.
        for row in 0 ..< rows {
            let runs = snap.rowRuns(row)
            let utf8 = snap.rowUtf8(row)
            for i in 0 ..< runs.count {
                let run = runs[i]
                let resolved = CellQuadResolver.resolve(
                    fgPacked: run.fg_rgba,
                    bgPacked: run.bg_rgba,
                    flags: run.flags,
                    transparentMode: state.transparentMode,
                    palette: theme.palette,
                    defaultFg: theme.defaultFg,
                    defaultBg: theme.defaultBg,
                )
                guard appendRunGlyphs(
                    &instances,
                    run: run,
                    utf8: utf8,
                    row: row,
                    fg: resolved.fg,
                    theme: theme,
                ) else { return false }
            }
        }

        // 3. Cursor (on top of glyphs, same as the CPU painter).
        guard appendCursor(&instances, snap: snap, theme: theme, state: state) else { return false }

        // 4. Selection tint.
        let sel = snap.selection
        if sel.present == 1 {
            appendSpanQuads(
                &instances,
                startRow: Int(sel.start_row), startCol: Int(sel.start_col),
                endRow: Int(sel.end_row), endCol: Int(sel.end_col),
                isBlock: sel.is_block == 1,
                viewWidth: state.viewSizePoints.width,
                rgba: theme.surface2,
                alpha: 0.45,
            )
        }

        // 5. Search-match tint.
        let match = snap.searchMatch
        if match.present == 1 {
            appendSpanQuads(
                &instances,
                startRow: Int(match.start_row), startCol: Int(match.start_col),
                endRow: Int(match.end_row), endCol: Int(match.end_col),
                isBlock: false,
                viewWidth: state.viewSizePoints.width,
                rgba: theme.accent,
                alpha: 0.45,
            )
        }
        return true
    }

    /// Glyph + underline/strike instances for one run. Returns false
    /// on atlas overflow.
    private func appendRunGlyphs(
        _ instances: inout [QuadInstance],
        run: CopadRun,
        utf8: UnsafeBufferPointer<UInt8>,
        row: UInt16,
        fg: UInt32,
        theme: ThemeColors,
    ) -> Bool {
        let cellW = metrics.cellWidth
        let cellH = metrics.cellHeight
        let len = Int(run.utf8_len)
        let offset = Int(run.utf8_offset)
        guard offset + len <= utf8.count else { return true }
        guard
            let str = String(bytes: UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len]), encoding: .utf8),
            !str.isEmpty
        else { return true }

        let isBold = run.flags & CopadCellFlags.bold != 0
        let isItalic = run.flags & CopadCellFlags.italic != 0
        let runFont = resolveRunFont(str, base: fonts.pick(bold: isBold, italic: isItalic))
        let baselineY = Double(row) * cellH + metrics.ascent
        let runX = Double(run.start_col) * cellW

        // Aggregated uniform-ASCII run: one shaped glyph stamped per
        // cell at exact cell intervals (walk_row only aggregates
        // same-byte single-width cells, so per-cell stamping is
        // correct AND keeps wide grids grid-aligned).
        let span = Int(run.end_col - run.start_col)
        let isWide = run.flags & CopadCellFlags.wideLeading != 0
        if span > 1, !isWide {
            let glyphChar = String(str.prefix(1))
            if glyphChar != " " {
                let shaped = shape(text: glyphChar, font: runFont)
                for cell in 0 ..< span {
                    guard appendShapedGlyphs(
                        &instances,
                        shaped: shaped,
                        originX: runX + Double(cell) * cellW,
                        baselineY: baselineY,
                        tint: fg,
                    ) else { return false }
                }
            }
        } else {
            let shaped = shape(text: str, font: runFont)
            guard appendShapedGlyphs(
                &instances,
                shaped: shaped,
                originX: runX,
                baselineY: baselineY,
                tint: fg,
            ) else { return false }
        }

        // Decorations: single-fold underline + strike as quads from
        // font metrics. Spans the whole run, including spaces — same
        // as the CPU painter's CTLine attribute rendering.
        let runWidth = Double(span) * cellW
        if run.underline_style != 0 {
            let ulColor = run.underline_color_rgba == 0
                ? fg
                : GPUCellColor.resolve(run.underline_color_rgba, palette: theme.palette, defaultColor: fg)
            let thickness = max(1, Double(runFont.underlineThickness))
            appendSolid(
                &instances,
                quad: GPUQuad(
                    x: runX,
                    y: baselineY - Double(runFont.underlinePosition) - thickness / 2,
                    width: runWidth,
                    height: thickness,
                ),
                rgba: ulColor,
            )
        }
        if run.flags & CopadCellFlags.strike != 0 {
            let thickness = max(1, Double(runFont.underlineThickness))
            appendSolid(
                &instances,
                quad: GPUQuad(
                    x: runX,
                    y: baselineY - Double(runFont.xHeight) / 2 - thickness / 2,
                    width: runWidth,
                    height: thickness,
                ),
                rgba: fg,
            )
        }
        return true
    }

    /// Emit the textured quads for one shaped string at a pen
    /// position. Returns false on atlas overflow.
    private func appendShapedGlyphs(
        _ instances: inout [QuadInstance],
        shaped: [PositionedGlyph],
        originX: Double,
        baselineY: Double,
        tint: UInt32,
    ) -> Bool {
        let scale = metrics.scale
        for pg in shaped {
            guard let entry = atlas.entry(glyph: pg.glyph, font: pg.font, isColor: pg.isColor) else {
                return false
            }
            guard !entry.isEmpty else { continue }
            let x = originX + Double(pg.position.x) + Double(entry.bearingX)
            let y = baselineY - Double(pg.position.y) - Double(entry.bearingMaxY)
            instances.append(QuadInstance(
                origin: SIMD2(Float(x * Double(scale)), Float(y * Double(scale))),
                size: SIMD2(Float(entry.pixelSize.width), Float(entry.pixelSize.height)),
                uvOrigin: entry.uvOrigin,
                uvSize: entry.uvSize,
                color: rgbaToFloat4(tint),
                kindPad: SIMD4(pg.isColor ? QuadKind.colorGlyph.rawValue : QuadKind.monoGlyph.rawValue, 0, 0, 0),
            ))
        }
        return true
    }

    /// Cursor quads + inverted cursor-cell glyph (block style).
    /// Returns false on atlas overflow.
    private func appendCursor(
        _ instances: inout [QuadInstance],
        snap: CopadTermFFI.Snapshot,
        theme: ThemeColors,
        state: FrameState,
    ) -> Bool {
        let cursor = snap.cursor
        let quads = GridQuadGeometry.cursorQuads(
            style: cursor.style,
            blink: cursor.blink,
            blinkVisible: state.blinkVisible,
            isKeyWindow: state.isKeyWindow,
            needsOutline: state.transparentMode,
            col: Int(cursor.col),
            row: Int(cursor.row),
            cellWidth: metrics.cellWidth,
            cellHeight: metrics.cellHeight,
        )
        for q in quads.accent {
            appendSolid(&instances, quad: q, rgba: theme.accent)
        }
        for q in quads.outline {
            appendSolid(&instances, quad: q, rgba: theme.defaultBg)
        }
        guard quads.redrawGlyphInverted else { return true }
        return appendInvertedCursorGlyph(&instances, snap: snap, theme: theme)
    }

    /// Port of the CPU painter's `redrawCursorGlyph`: re-render the
    /// cell glyph under a filled block cursor tinted theme.background
    /// so the character stays legible.
    private func appendInvertedCursorGlyph(
        _ instances: inout [QuadInstance],
        snap: CopadTermFFI.Snapshot,
        theme: ThemeColors,
    ) -> Bool {
        let cursor = snap.cursor
        let runs = snap.rowRuns(cursor.row)
        let utf8 = snap.rowUtf8(cursor.row)

        var hit: CopadRun?
        for i in 0 ..< runs.count {
            let r = runs[i]
            if r.start_col <= cursor.col, cursor.col < r.end_col {
                hit = r
                break
            }
        }
        guard let run = hit else { return true }

        let len = Int(run.utf8_len)
        let offset = Int(run.utf8_offset)
        guard offset + len <= utf8.count else { return true }

        let runSpan = run.end_col - run.start_col
        let isWide = run.flags & CopadCellFlags.wideLeading != 0
        let isAggregatedUniform = !isWide && runSpan > 1

        let drawBytes: UnsafeBufferPointer<UInt8>
        let drawX: Double
        if isAggregatedUniform {
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + 1])
            drawX = Double(cursor.col) * metrics.cellWidth
        } else {
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len])
            drawX = Double(run.start_col) * metrics.cellWidth
        }
        guard
            let str = String(bytes: drawBytes, encoding: .utf8),
            !str.isEmpty,
            str != " "
        else { return true }

        let isBold = run.flags & CopadCellFlags.bold != 0
        let isItalic = run.flags & CopadCellFlags.italic != 0
        let runFont = resolveRunFont(str, base: fonts.pick(bold: isBold, italic: isItalic))
        let baselineY = Double(cursor.row) * metrics.cellHeight + metrics.ascent
        return appendShapedGlyphs(
            &instances,
            shaped: shape(text: str, font: runFont),
            originX: drawX,
            baselineY: baselineY,
            tint: theme.defaultBg,
        )
    }

    /// Selection / search-match tint quads via the shared span
    /// geometry.
    private func appendSpanQuads(
        _ instances: inout [QuadInstance],
        startRow: Int,
        startCol: Int,
        endRow: Int,
        endCol: Int,
        isBlock: Bool,
        viewWidth: CGFloat,
        rgba: UInt32,
        alpha: Float,
    ) {
        let cellW = metrics.cellWidth
        let cellH = metrics.cellHeight
        guard cellW > 0, cellH > 0 else { return }
        let lastCol = max(1, Int(viewWidth / cellW)) - 1
        var color = rgbaToFloat4(rgba)
        color.w = alpha
        for span in GridQuadGeometry.spanRows(
            startRow: startRow, startCol: startCol,
            endRow: endRow, endCol: endCol,
            lastCol: lastCol, isBlock: isBlock,
        ) {
            appendSolid(
                &instances,
                quad: GPUQuad(
                    x: Double(span.firstCol) * cellW,
                    y: Double(span.row) * cellH,
                    width: Double(span.finalCol - span.firstCol + 1) * cellW,
                    height: cellH,
                ),
                color: color,
            )
        }
    }

    private func appendSolid(_ instances: inout [QuadInstance], quad: GPUQuad, rgba: UInt32) {
        appendSolid(&instances, quad: quad, color: rgbaToFloat4(rgba))
    }

    private func appendSolid(_ instances: inout [QuadInstance], quad: GPUQuad, color: SIMD4<Float>) {
        let s = Float(metrics.scale)
        instances.append(QuadInstance(
            origin: SIMD2(Float(quad.x) * s, Float(quad.y) * s),
            size: SIMD2(Float(quad.width) * s, Float(quad.height) * s),
            color: color,
            kindPad: SIMD4(QuadKind.solid.rawValue, 0, 0, 0),
        ))
    }

    // MARK: - Shaping

    /// Mirror of the CPU painter's `resolveRunFont` (cascade fallback
    /// for CJK / Nerd Font / emoji), with its own cache — the two
    /// painters never share a pane.
    private func resolveRunFont(_ str: String, base: NSFont) -> NSFont {
        var key: UInt32?
        for scalar in str.unicodeScalars where scalar.value >= 0x80 {
            key = scalar.value
            break
        }
        guard let cp = key else { return base }
        if let cached = fallbackCache[cp] { return cached }
        let resolved = CTFontCreateForString(
            base as CTFont,
            str as CFString,
            CFRange(location: 0, length: (str as NSString).length),
        ) as NSFont
        fallbackCache[cp] = resolved
        return resolved
    }

    private func shape(text: String, font: NSFont) -> [PositionedGlyph] {
        let key = ShapeKey(text: text, fontId: ObjectIdentifier(font))
        if let cached = shapeCache[key] { return cached }

        let attr = NSAttributedString(string: text, attributes: [.font: font])
        let line = CTLineCreateWithAttributedString(attr)
        var out: [PositionedGlyph] = []
        for runAny in CTLineGetGlyphRuns(line) as NSArray {
            let run = runAny as! CTRun
            let count = CTRunGetGlyphCount(run)
            guard count > 0 else { continue }
            let attrs = CTRunGetAttributes(run) as NSDictionary
            // CoreText may substitute a cascade font mid-line; the
            // run's effective font is what the glyph IDs index into.
            let runFont = attrs[kCTFontAttributeName] as! CTFont
            let isColor = CTFontGetSymbolicTraits(runFont).contains(.traitColorGlyphs)
            var glyphs = [CGGlyph](repeating: 0, count: count)
            var positions = [CGPoint](repeating: .zero, count: count)
            CTRunGetGlyphs(run, CFRange(location: 0, length: 0), &glyphs)
            CTRunGetPositions(run, CFRange(location: 0, length: 0), &positions)
            for i in 0 ..< count {
                out.append(PositionedGlyph(
                    glyph: glyphs[i],
                    font: runFont,
                    position: positions[i],
                    isColor: isColor,
                ))
            }
        }

        if shapeCache.count >= Self.shapeCacheMax {
            shapeCache.removeAll(keepingCapacity: true)
        }
        shapeCache[key] = out
        return out
    }

    private func rgbaToFloat4(_ rgba: UInt32) -> SIMD4<Float> {
        SIMD4(
            Float((rgba >> 24) & 0xFF) / 255.0,
            Float((rgba >> 16) & 0xFF) / 255.0,
            Float((rgba >> 8) & 0xFF) / 255.0,
            Float(rgba & 0xFF) / 255.0,
        )
    }
}

// MARK: - Glyph atlas

/// CoreText-rasterized glyph cache in a single RGBA8 texture.
/// Monochrome glyphs rasterize white-on-transparent (the shader
/// multiplies coverage by the per-instance tint); color glyphs (emoji)
/// rasterize as-is and the shader samples them untinted.
@MainActor
private final class GlyphAtlas {
    struct Entry {
        let uvOrigin: SIMD2<Float>
        let uvSize: SIMD2<Float>
        /// Quad extent in device pixels (the bitmap is 1:1).
        let pixelSize: CGSize
        /// Bitmap left edge in glyph space, points (negative for
        /// glyphs with left overhang — italics, combining marks).
        let bearingX: CGFloat
        /// Bitmap top edge above the baseline, points (y-up). The
        /// flipped-view quad top is `baselineY - bearingMaxY`.
        let bearingMaxY: CGFloat
        /// Zero-coverage glyph (space): no quad to emit.
        let isEmpty: Bool
    }

    private struct Key: Hashable {
        let font: FontKey
        let glyph: CGGlyph
    }

    /// CTFont wrapper hashing by CFHash/CFEqual so cascade-resolved
    /// instances of the same font dedupe into one atlas entry.
    private struct FontKey: Hashable {
        let font: CTFont

        static func == (lhs: FontKey, rhs: FontKey) -> Bool {
            CFEqual(lhs.font, rhs.font)
        }

        func hash(into hasher: inout Hasher) {
            hasher.combine(CFHash(font))
        }
    }

    static let size = 2048

    let texture: MTLTexture
    private var packer: AtlasShelfPacker
    private var entries: [Key: Entry] = [:]
    private var scale: CGFloat

    init?(device: MTLDevice, scale: CGFloat) {
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .rgba8Unorm,
            width: Self.size,
            height: Self.size,
            mipmapped: false,
        )
        desc.usage = .shaderRead
        desc.storageMode = .shared
        guard let texture = device.makeTexture(descriptor: desc) else { return nil }
        self.texture = texture
        packer = AtlasShelfPacker(width: Self.size, height: Self.size, padding: 1)
        self.scale = scale
    }

    /// Drop every entry and reset packing. Old texels stay in the
    /// texture (harmless — nothing references them) and get
    /// overwritten as new glyphs land.
    func flush(scale newScale: CGFloat) {
        entries.removeAll(keepingCapacity: true)
        packer.reset()
        scale = newScale
    }

    /// Look up or rasterize. Returns nil on atlas overflow — caller
    /// flushes and rebuilds the frame.
    func entry(glyph: CGGlyph, font: CTFont, isColor: Bool) -> Entry? {
        let key = Key(font: FontKey(font: font), glyph: glyph)
        if let cached = entries[key] { return cached }

        var g = glyph
        var rect = CGRect.zero
        CTFontGetBoundingRectsForGlyphs(font, .horizontal, &g, &rect, 1)
        guard rect.width > 0, rect.height > 0 else {
            let empty = Entry(
                uvOrigin: .zero, uvSize: .zero, pixelSize: .zero,
                bearingX: 0, bearingMaxY: 0, isEmpty: true,
            )
            entries[key] = empty
            return empty
        }

        // 1px guard band on every side for antialiasing bleed.
        let padPx = 1
        let padPts = CGFloat(padPx) / scale
        let widthPx = Int(ceil(rect.width * scale)) + 2 * padPx
        let heightPx = Int(ceil(rect.height * scale)) + 2 * padPx

        guard let placement = packer.place(width: widthPx, height: heightPx) else {
            return nil
        }

        var bytes = [UInt8](repeating: 0, count: widthPx * heightPx * 4)
        let drawn = bytes.withUnsafeMutableBytes { buf -> Bool in
            guard let ctx = CGContext(
                data: buf.baseAddress,
                width: widthPx,
                height: heightPx,
                bitsPerComponent: 8,
                bytesPerRow: widthPx * 4,
                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue,
            ) else { return false }
            ctx.scaleBy(x: scale, y: scale)
            if !isColor {
                ctx.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
            }
            var position = CGPoint(x: -rect.minX + padPts, y: -rect.minY + padPts)
            var glyphCopy = glyph
            CTFontDrawGlyphs(font, &glyphCopy, &position, 1, ctx)
            return true
        }
        guard drawn else { return nil }

        texture.replace(
            region: MTLRegionMake2D(placement.x, placement.y, widthPx, heightPx),
            mipmapLevel: 0,
            withBytes: bytes,
            bytesPerRow: widthPx * 4,
        )

        let atlasSize = Float(Self.size)
        let entry = Entry(
            uvOrigin: SIMD2(Float(placement.x) / atlasSize, Float(placement.y) / atlasSize),
            uvSize: SIMD2(Float(widthPx) / atlasSize, Float(heightPx) / atlasSize),
            pixelSize: CGSize(width: widthPx, height: heightPx),
            bearingX: rect.minX - padPts,
            bearingMaxY: rect.maxY + padPts,
            isEmpty: false,
        )
        entries[key] = entry
        return entry
    }
}
