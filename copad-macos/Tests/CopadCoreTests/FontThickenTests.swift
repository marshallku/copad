@testable import CopadCore
import XCTest

final class FontThickenTests: XCTestCase {
    func testAutoSmoothsOnlyBelowRetina() {
        XCTAssertTrue(FontThicken.auto.smooths(scale: 1))
        XCTAssertFalse(FontThicken.auto.smooths(scale: 2))
        XCTAssertFalse(FontThicken.auto.smooths(scale: 3))
    }

    func testExplicitValuesIgnoreScale() {
        XCTAssertTrue(FontThicken.on.smooths(scale: 2))
        XCTAssertFalse(FontThicken.off.smooths(scale: 1))
    }

    func testParsesBoolsAndStrings() {
        XCTAssertEqual(FontThicken.parse(bool: true, string: nil), .on)
        XCTAssertEqual(FontThicken.parse(bool: false, string: nil), .off)
        XCTAssertEqual(FontThicken.parse(bool: nil, string: "auto"), .auto)
        XCTAssertEqual(FontThicken.parse(bool: nil, string: "AUTO"), .auto)
        XCTAssertEqual(FontThicken.parse(bool: nil, string: "off"), .off)
        XCTAssertNil(FontThicken.parse(bool: nil, string: "bold"))
        XCTAssertNil(FontThicken.parse(bool: nil, string: nil))
    }
}
