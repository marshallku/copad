@testable import CopadCore
import XCTest

final class AlphaBlendingTests: XCTestCase {
    func testOnlyOpaqueBackgroundsBlendLinearly() {
        XCTAssertTrue(AlphaBlending.linear.blendsLinearly(transparentBackground: false))
        XCTAssertFalse(AlphaBlending.linear.blendsLinearly(transparentBackground: true))
        XCTAssertFalse(AlphaBlending.native.blendsLinearly(transparentBackground: false))
    }

    func testParses() {
        XCTAssertEqual(AlphaBlending.parse("native"), .native)
        XCTAssertEqual(AlphaBlending.parse("Linear"), .linear)
        XCTAssertNil(AlphaBlending.parse("linear-corrected"))
        XCTAssertNil(AlphaBlending.parse(nil))
    }

    func testLinearizeIsTheSRGBCurve() {
        XCTAssertEqual(AlphaBlending.linearize(0), 0)
        XCTAssertEqual(AlphaBlending.linearize(1), 1, accuracy: 1e-6)
        XCTAssertEqual(AlphaBlending.linearize(0.5), 0.2140, accuracy: 1e-4)
        // The linear segment and the power segment meet at the threshold.
        XCTAssertEqual(AlphaBlending.linearize(0.04045), 0.04045 / 12.92, accuracy: 1e-6)
    }
}
