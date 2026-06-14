@testable import CopadCore
import XCTest

final class FrameMetricsRecorderTests: XCTestCase {
    func testEmptyRecorderHasNoStats() {
        let r = FrameMetricsRecorder()
        XCTAssertNil(r.stats())
        XCTAssertEqual(r.sampleCount, 0)
    }

    func testSingleSample() throws {
        var r = FrameMetricsRecorder()
        r.record(1000)
        let s = try XCTUnwrap(r.stats())
        XCTAssertEqual(s.count, 1)
        XCTAssertEqual(s.meanNs, 1000)
        XCTAssertEqual(s.p50Ns, 1000)
        XCTAssertEqual(s.p95Ns, 1000)
        XCTAssertEqual(s.p99Ns, 1000)
        XCTAssertEqual(s.maxNs, 1000)
    }

    func testMeanAndMaxAcrossSamples() throws {
        var r = FrameMetricsRecorder()
        for v: UInt64 in [10, 20, 30, 40] {
            r.record(v)
        }
        let s = try XCTUnwrap(r.stats())
        XCTAssertEqual(s.count, 4)
        XCTAssertEqual(s.meanNs, 25)
        XCTAssertEqual(s.maxNs, 40)
    }

    func testNearestRankPercentiles() throws {
        var r = FrameMetricsRecorder()
        // 1...100 → p50 = 50th value (50), p95 = 95, p99 = 99, max 100.
        for v in 1 ... 100 {
            r.record(UInt64(v))
        }
        let s = try XCTUnwrap(r.stats())
        XCTAssertEqual(s.p50Ns, 50)
        XCTAssertEqual(s.p95Ns, 95)
        XCTAssertEqual(s.p99Ns, 99)
        XCTAssertEqual(s.maxNs, 100)
    }

    func testPercentileIgnoresInsertionOrder() {
        var ascending = FrameMetricsRecorder()
        var descending = FrameMetricsRecorder()
        for v in 1 ... 50 {
            ascending.record(UInt64(v))
        }
        for v in stride(from: 50, through: 1, by: -1) {
            descending.record(UInt64(v))
        }
        XCTAssertEqual(ascending.stats(), descending.stats())
    }

    func testRingBufferEvictsOldestBeyondCapacity() throws {
        var r = FrameMetricsRecorder(capacity: 3)
        r.record(1)
        r.record(2)
        r.record(3)
        r.record(4) // evicts the oldest (1)
        let s = try XCTUnwrap(r.stats())
        XCTAssertEqual(s.count, 3)
        XCTAssertEqual(s.maxNs, 4)
        // Window is {2,3,4}; mean 3, the 1 is gone.
        XCTAssertEqual(s.meanNs, 3)
    }

    func testRingBufferStaysAtCapacity() throws {
        var r = FrameMetricsRecorder(capacity: 5)
        for v in 1 ... 100 {
            r.record(UInt64(v))
        }
        XCTAssertEqual(r.sampleCount, 5)
        let s = try XCTUnwrap(r.stats())
        // Last 5 recorded are 96...100.
        XCTAssertEqual(s.maxNs, 100)
        XCTAssertEqual(s.meanNs, 98)
    }

    func testResetClears() {
        var r = FrameMetricsRecorder(capacity: 4)
        r.record(10)
        r.record(20)
        r.reset()
        XCTAssertNil(r.stats())
        XCTAssertEqual(r.sampleCount, 0)
        // Reusable after reset, write index back at 0.
        r.record(5)
        XCTAssertEqual(r.stats()?.meanNs, 5)
    }

    func testCapacityClampedToAtLeastOne() {
        var r = FrameMetricsRecorder(capacity: 0)
        r.record(7)
        r.record(9)
        XCTAssertEqual(r.sampleCount, 1)
        XCTAssertEqual(r.stats()?.maxNs, 9)
    }

    func testNoOverflowOnLargeSamples() throws {
        var r = FrameMetricsRecorder(capacity: 4)
        // Values near UInt64 max — sum would trap with checked `+`,
        // `&+` must keep the path crash-free.
        let big = UInt64.max - 1
        for _ in 0 ..< 4 {
            r.record(big)
        }
        let s = try XCTUnwrap(r.stats())
        XCTAssertEqual(s.maxNs, big)
        XCTAssertEqual(s.p99Ns, big)
    }
}
