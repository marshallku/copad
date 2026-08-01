@testable import CopadCore
import XCTest

final class PomodoroProgressTests: XCTestCase {
    func testFractionFull() {
        XCTAssertEqual(PomodoroProgress.fraction(remainingMs: 1_500_000, totalMs: 1_500_000), 1.0)
    }

    func testFractionHalf() {
        XCTAssertEqual(
            PomodoroProgress.fraction(remainingMs: 750_000, totalMs: 1_500_000),
            0.5,
            accuracy: 1e-9,
        )
    }

    func testFractionZeroDenominatorIsZero() {
        XCTAssertEqual(PomodoroProgress.fraction(remainingMs: 100, totalMs: 0), 0)
    }

    func testFractionClampsAboveOne() {
        // Clock jump: remaining > total must not overdraw the ring.
        XCTAssertEqual(PomodoroProgress.fraction(remainingMs: 2000, totalMs: 1000), 1.0)
    }

    func testFractionClampsNegative() {
        XCTAssertEqual(PomodoroProgress.fraction(remainingMs: -50, totalMs: 1000), 0)
    }

    func testMmssRoundsUp() {
        XCTAssertEqual(PomodoroProgress.mmss(1_500_000), "25:00")
        XCTAssertEqual(PomodoroProgress.mmss(1), "00:01")
        XCTAssertEqual(PomodoroProgress.mmss(0), "00:00")
        XCTAssertEqual(PomodoroProgress.mmss(59_001), "01:00")
    }

    func testMmssHugeValueDoesNotOverflow() {
        // A malformed payload could yield a near-max value; must not trap on
        // `ms + 999`. Just assert it returns without crashing.
        _ = PomodoroProgress.mmss(UInt64.max)
        _ = PomodoroProgress.mmss(UInt64.max - 500)
    }
}
