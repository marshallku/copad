import XCTest
@testable import copad_ios
import UIKit

/// The frame path's cost on a busy full screen (an agent streaming output): wire decode, grid
/// apply, and one full draw. Prints the medians so a change can be compared; asserts only that
/// the result is what the grid should show.
final class RenderPerfTests: XCTestCase {
    private let cols = 120, rows = 40

    /// A full frame of mixed content: coloured runs, bold, CJK, and a few emoji.
    private func frameLine(full: Bool, salt: Int) -> Data {
        var cells: [String] = []
        let colors = ["Reset", "Red", "#89B4FA", "12", "Green", "#F9E2AF"]
        for y in 0 ..< rows {
            var x = 0
            while x < cols {
                let k = (x + y + salt) % 37
                let sym: String
                if k == 5, x + 1 < cols { sym = "한" } else if k == 9, x + 1 < cols { sym = "🙂" } else {
                    sym = String(UnicodeScalar(UInt8(97 + (x * 7 + y + salt) % 26)))
                }
                let fg = colors[(x / 8 + y) % colors.count]
                let bg = y % 7 == 0 ? "#313244" : "Reset"
                let mods = k % 11 == 0 ? "BOLD" : (k % 13 == 0 ? "BOLD | ITALIC" : "")
                cells.append(#"{"x":\#(x),"y":\#(y),"sym":"\#(sym)","fg":"\#(fg)","bg":"\#(bg)","mods":"\#(mods)","skip":false}"#)
                x += sym.utf8.count > 1 ? 2 : 1
            }
        }
        let json = #"{"t":"frame","epoch":1,"cols":\#(cols),"rows":\#(rows),"full":\#(full),"cells":[\#(cells.joined(separator: ","))],"cursor":[3,4],"cursor_visible":true,"cursor_shape":0}"#
        return Data(json.utf8)
    }

    private func median(_ samples: [Double]) -> Double {
        samples.sorted()[samples.count / 2]
    }

    private func time(_ n: Int, _ body: () -> Void) -> Double {
        var samples: [Double] = []
        for _ in 0 ..< n {
            let t = CFAbsoluteTimeGetCurrent()
            body()
            samples.append((CFAbsoluteTimeGetCurrent() - t) * 1000)
        }
        return median(samples)
    }

    @MainActor
    func testFramePathCost() {
        let full = frameLine(full: true, salt: 0)
        let deltas = (1 ... 20).map { frameLine(full: false, salt: $0) }

        let decode = time(20) { _ = ServerMsg.parse(full) }

        var grid = Grid()
        guard case let .frame(first)? = ServerMsg.parse(full) else { return XCTFail("frame did not parse") }
        let frames = deltas.compactMap { line -> FrameMsg? in
            if case let .frame(f)? = ServerMsg.parse(line) { return f }
            return nil
        }
        grid.apply(first)
        var i = 0
        let apply = time(20) {
            grid.apply(frames[i % frames.count])
            i += 1
        }
        XCTAssertEqual(grid.cols, cols)
        XCTAssertEqual(grid.text(row: 0).count > 0, true)

        let session = MuxSession.previewForTests(grid: grid)
        let view = TerminalView(frame: CGRect(x: 0, y: 0, width: 1000, height: 700))
        view.session = session
        let renderer = UIGraphicsImageRenderer(bounds: view.bounds)
        let draw = time(10) {
            _ = renderer.image { _ in view.draw(view.bounds) }
        }
        print(String(format: "[render-perf] decode %.2f ms · apply %.2f ms · draw %.2f ms (%dx%d)",
                     decode, apply, draw, cols, rows))
    }

    /// A wide (non-ASCII) cell keeps its strike-through: Core Text does not draw UIKit's
    /// strike attribute, so the view draws it.
    @MainActor
    func testStrikeThroughOnAWideCell() {
        let line = Data(##"{"t":"frame","epoch":1,"cols":4,"rows":1,"full":true,"cells":[{"x":0,"y":0,"sym":"한","fg":"#FF0000","bg":"Reset","mods":"CROSSED_OUT","skip":false}],"cursor":null,"cursor_visible":false,"cursor_shape":0}"##.utf8)
        guard case let .frame(f)? = ServerMsg.parse(line) else { return XCTFail("frame did not parse") }
        var grid = Grid()
        grid.apply(f)
        let view = TerminalView(frame: CGRect(x: 0, y: 0, width: 200, height: 40))
        let session = MuxSession.previewForTests(grid: grid) // the view holds it weakly
        view.session = session
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        format.preferredRange = .standard
        let image = UIGraphicsImageRenderer(bounds: view.bounds, format: format).image { _ in view.draw(view.bounds) }
        guard let cg = image.cgImage, let data = cg.dataProvider?.data, let bytes = CFDataGetBytePtr(data) else {
            return XCTFail("no pixels")
        }
        let (w, perRow) = (cg.width, cg.bytesPerRow)
        // Some pixel row within the cell must be red across (nearly) the whole two-cell width.
        let width = Int(view.cellW * 2)
        let best = (0 ..< Int(view.cellH)).map { y in
            (0 ..< min(width, w)).filter { x in
                let p = bytes + y * perRow + x * 4
                // Red in either byte order (the renderer's is BGRA).
                return p[1] < 90 && ((p[0] > 150 && p[2] < 90) || (p[2] > 150 && p[0] < 90))
            }.count
        }.max() ?? 0
        XCTAssertGreaterThan(best, width * 8 / 10, "strike line spans \(best) of \(width) px")
    }
}
