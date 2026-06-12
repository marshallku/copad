@testable import CopadCore
import XCTest

final class GPUCellColorTests: XCTestCase {
    private let palette: [UInt32] = (0 ..< 16).map { UInt32($0) << 24 | 0xFF }

    func testDefaultSentinelResolvesToDefault() {
        XCTAssertEqual(GPUCellColor.resolve(0x0000_0000, palette: palette, defaultColor: 0xAABB_CCFF), 0xAABB_CCFF)
    }

    func testIndexedResolvesFromPalette() {
        XCTAssertEqual(GPUCellColor.resolve(0x0100_0005, palette: palette, defaultColor: 0), palette[5])
    }

    func testIndexedOutOfRangeFallsBackToDefault() {
        XCTAssertEqual(GPUCellColor.resolve(0x0100_00FF, palette: palette, defaultColor: 0x1234_56FF), 0x1234_56FF)
    }

    func testDirectRGBShiftsAndForcesOpaqueAlpha() {
        // 0xFF tag + RGB 0x11AA22 → 0x11AA22FF.
        XCTAssertEqual(GPUCellColor.resolve(0xFF11_AA22, palette: palette, defaultColor: 0), 0x11AA_22FF)
    }

    func testDirectRGBWithZeroRedStaysDirect() {
        // Regression guard for the old "alpha byte = 0 means indexed"
        // collision: pure green must not fall into the indexed path.
        XCTAssertEqual(GPUCellColor.resolve(0xFF00_FF00, palette: palette, defaultColor: 0), 0x00FF_00FF)
    }

    func testUnknownTagFallsBackToDefault() {
        XCTAssertEqual(GPUCellColor.resolve(0x7F00_0000, palette: palette, defaultColor: 0xDEAD_BEEF), 0xDEAD_BEEF)
    }

    func testDimScalesAlphaTo65Percent() {
        XCTAssertEqual(GPUCellColor.dimmed(0x1122_33FF) & 0xFF, UInt32((255.0 * 0.65).rounded()))
        XCTAssertEqual(GPUCellColor.dimmed(0x1122_33FF) & 0xFFFF_FF00, 0x1122_3300)
    }
}

final class CellQuadResolverTests: XCTestCase {
    private let palette: [UInt32] = (0 ..< 16).map { UInt32(0x10 + $0) << 24 | 0xFF }
    private let defaultFg: UInt32 = 0xCDD6_F4FF
    private let defaultBg: UInt32 = 0x1E1E_2EFF

    private func resolve(
        fg: UInt32 = 0,
        bg: UInt32 = 0,
        flags: UInt16 = 0,
        transparent: Bool = false,
    ) -> CellQuadResolver.Resolved {
        CellQuadResolver.resolve(
            fgPacked: fg, bgPacked: bg, flags: flags,
            transparentMode: transparent,
            palette: palette, defaultFg: defaultFg, defaultBg: defaultBg,
        )
    }

    func testDefaultCellOpaqueModeSkipsBgQuad() {
        let r = resolve()
        XCTAssertEqual(r.fg, defaultFg)
        XCTAssertEqual(r.bg, defaultBg)
        XCTAssertFalse(r.paintBg)
    }

    func testExplicitBgPaintsInOpaqueMode() {
        let r = resolve(bg: 0xFFAA_0000)
        XCTAssertEqual(r.bg, 0xAA00_00FF)
        XCTAssertTrue(r.paintBg)
    }

    func testExplicitBgEqualToThemeBgSkipsInOpaqueMode() {
        // 0xFF tag carrying exactly the theme bg RGB: bounds fill
        // already painted it.
        let r = resolve(bg: 0xFF1E_1E2E)
        XCTAssertEqual(r.bg, defaultBg)
        XCTAssertFalse(r.paintBg)
    }

    func testExplicitBgEqualToThemeBgStillPaintsInTransparentMode() {
        // Same color, but provenance is explicit — transparent mode
        // keys off the sentinel, not the resolved value.
        let r = resolve(bg: 0xFF1E_1E2E, transparent: true)
        XCTAssertTrue(r.paintBg)
    }

    func testDefaultCellTransparentModeSkips() {
        XCTAssertFalse(resolve(transparent: true).paintBg)
    }

    func testInverseSwapsAndPaintsEvenWithDefaultBgInTransparentMode() {
        let r = resolve(flags: CopadCellFlags.inverse, transparent: true)
        // Swapped: fg ← theme bg, bg ← theme fg.
        XCTAssertEqual(r.fg, defaultBg)
        XCTAssertEqual(r.bg, defaultFg)
        XCTAssertTrue(r.paintBg)
    }

    func testInverseDefaultCellPaintsInOpaqueModeToo() {
        // bg resolves to defaultFg after the swap, which differs from
        // the bounds fill — must paint.
        XCTAssertTrue(resolve(flags: CopadCellFlags.inverse).paintBg)
    }

    func testDimAppliesAfterInverseSwap() {
        let r = resolve(flags: CopadCellFlags.inverse | CopadCellFlags.dim)
        XCTAssertEqual(r.fg, GPUCellColor.dimmed(defaultBg))
    }
}

final class AtlasShelfPackerTests: XCTestCase {
    func testPlacesAtOriginFirst() {
        var p = AtlasShelfPacker(width: 64, height: 64)
        XCTAssertEqual(p.place(width: 10, height: 12), AtlasShelfPacker.Placement(x: 0, y: 0))
    }

    func testAdvancesCursorWithPadding() {
        var p = AtlasShelfPacker(width: 64, height: 64, padding: 1)
        _ = p.place(width: 10, height: 12)
        XCTAssertEqual(p.place(width: 10, height: 12), AtlasShelfPacker.Placement(x: 11, y: 0))
    }

    func testOpensNewShelfWhenRowFull() {
        var p = AtlasShelfPacker(width: 32, height: 64, padding: 1)
        _ = p.place(width: 20, height: 10) // shelf 0: cursor 21
        // 20+1 doesn't fit in remaining 11 → new shelf at y=11.
        XCTAssertEqual(p.place(width: 20, height: 10), AtlasShelfPacker.Placement(x: 0, y: 11))
    }

    func testTallerGlyphOpensNewShelf() {
        var p = AtlasShelfPacker(width: 64, height: 64, padding: 1)
        _ = p.place(width: 10, height: 10) // shelf height 11
        // Height 20 exceeds shelf 0's height → new shelf below.
        XCTAssertEqual(p.place(width: 10, height: 20), AtlasShelfPacker.Placement(x: 0, y: 11))
    }

    func testShorterGlyphReusesExistingShelf() {
        var p = AtlasShelfPacker(width: 64, height: 64, padding: 1)
        _ = p.place(width: 10, height: 20)
        XCTAssertEqual(p.place(width: 10, height: 5), AtlasShelfPacker.Placement(x: 11, y: 0))
    }

    func testOverflowReturnsNil() {
        var p = AtlasShelfPacker(width: 16, height: 16, padding: 1)
        XCTAssertNotNil(p.place(width: 14, height: 14))
        XCTAssertNil(p.place(width: 14, height: 14))
    }

    func testWiderThanAtlasReturnsNil() {
        var p = AtlasShelfPacker(width: 16, height: 16)
        XCTAssertNil(p.place(width: 32, height: 4))
    }

    func testResetReclaimsSpace() {
        var p = AtlasShelfPacker(width: 16, height: 16, padding: 1)
        _ = p.place(width: 14, height: 14)
        p.reset()
        XCTAssertEqual(p.place(width: 14, height: 14), AtlasShelfPacker.Placement(x: 0, y: 0))
    }

    func testZeroSizeClampsToOnePixel() {
        var p = AtlasShelfPacker(width: 16, height: 16, padding: 1)
        XCTAssertEqual(p.place(width: 0, height: 0), AtlasShelfPacker.Placement(x: 0, y: 0))
        // Reserved 2×2 (clamped 1 + padding) → next x is 2.
        XCTAssertEqual(p.place(width: 1, height: 1), AtlasShelfPacker.Placement(x: 2, y: 0))
    }
}

final class GridQuadGeometryTests: XCTestCase {
    typealias Span = GridQuadGeometry.CellSpan

    func testSingleRowSpan() {
        let spans = GridQuadGeometry.spanRows(
            startRow: 3, startCol: 2, endRow: 3, endCol: 7, lastCol: 79, isBlock: false,
        )
        XCTAssertEqual(spans, [Span(row: 3, firstCol: 2, finalCol: 7)])
    }

    func testMultiRowWrappedSpan() {
        let spans = GridQuadGeometry.spanRows(
            startRow: 1, startCol: 5, endRow: 3, endCol: 2, lastCol: 9, isBlock: false,
        )
        XCTAssertEqual(spans, [
            Span(row: 1, firstCol: 5, finalCol: 9),
            Span(row: 2, firstCol: 0, finalCol: 9),
            Span(row: 3, firstCol: 0, finalCol: 2),
        ])
    }

    func testBlockSpanRepeatsColumnRange() {
        let spans = GridQuadGeometry.spanRows(
            startRow: 1, startCol: 4, endRow: 3, endCol: 6, lastCol: 9, isBlock: true,
        )
        XCTAssertEqual(spans, [
            Span(row: 1, firstCol: 4, finalCol: 6),
            Span(row: 2, firstCol: 4, finalCol: 6),
            Span(row: 3, firstCol: 4, finalCol: 6),
        ])
    }

    func testBlockSpanClampsToGrid() {
        let spans = GridQuadGeometry.spanRows(
            startRow: 0, startCol: 8, endRow: 0, endCol: 30, lastCol: 9, isBlock: true,
        )
        XCTAssertEqual(spans, [Span(row: 0, firstCol: 8, finalCol: 9)])
    }

    func testInvertedSingleRowSpanDrops() {
        XCTAssertTrue(GridQuadGeometry.spanRows(
            startRow: 2, startCol: 7, endRow: 2, endCol: 3, lastCol: 9, isBlock: false,
        ).isEmpty)
    }

    func testOutlineQuadsCoverBorderExactly() {
        let quads = GridQuadGeometry.outlineQuads(of: GPUQuad(x: 10, y: 20, width: 8, height: 16), thickness: 1)
        XCTAssertEqual(quads, [
            GPUQuad(x: 10, y: 20, width: 8, height: 1),
            GPUQuad(x: 10, y: 35, width: 8, height: 1),
            GPUQuad(x: 10, y: 21, width: 1, height: 14),
            GPUQuad(x: 17, y: 21, width: 1, height: 14),
        ])
    }

    func testHiddenCursorEmitsNothing() {
        let q = GridQuadGeometry.cursorQuads(
            style: 0, blink: 0, blinkVisible: true, isKeyWindow: true,
            needsOutline: false, col: 0, row: 0, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q, .none)
    }

    func testBlinkOffPhaseEmitsNothing() {
        let q = GridQuadGeometry.cursorQuads(
            style: 1, blink: 1, blinkVisible: false, isKeyWindow: true,
            needsOutline: false, col: 0, row: 0, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q, .none)
    }

    func testBlockKeyWindowFillsCellAndInvertsGlyph() {
        let q = GridQuadGeometry.cursorQuads(
            style: 1, blink: 0, blinkVisible: true, isKeyWindow: true,
            needsOutline: false, col: 2, row: 3, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q.accent, [GPUQuad(x: 16, y: 48, width: 8, height: 16)])
        XCTAssertTrue(q.outline.isEmpty)
        XCTAssertTrue(q.redrawGlyphInverted)
    }

    func testBlockKeyWindowWithOutline() {
        let q = GridQuadGeometry.cursorQuads(
            style: 1, blink: 0, blinkVisible: true, isKeyWindow: true,
            needsOutline: true, col: 0, row: 0, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q.outline.count, 4)
    }

    func testBlockNonKeyWindowIsHollowAccent() {
        let q = GridQuadGeometry.cursorQuads(
            style: 1, blink: 0, blinkVisible: true, isKeyWindow: false,
            needsOutline: true, col: 0, row: 0, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q.accent.count, 4)
        XCTAssertTrue(q.outline.isEmpty)
        XCTAssertFalse(q.redrawGlyphInverted)
    }

    func testBeamIsTwoPixelLeadingBar() {
        let q = GridQuadGeometry.cursorQuads(
            style: 2, blink: 0, blinkVisible: true, isKeyWindow: true,
            needsOutline: false, col: 1, row: 1, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q.accent, [GPUQuad(x: 8, y: 16, width: 2, height: 16)])
        XCTAssertFalse(q.redrawGlyphInverted)
    }

    func testUnderlineIsTwoPixelBottomBar() {
        let q = GridQuadGeometry.cursorQuads(
            style: 3, blink: 0, blinkVisible: true, isKeyWindow: true,
            needsOutline: false, col: 1, row: 1, cellWidth: 8, cellHeight: 16,
        )
        XCTAssertEqual(q.accent, [GPUQuad(x: 8, y: 30, width: 8, height: 2)])
    }
}
