import Foundation

/// Fixed-capacity ring buffer of per-frame render durations (ns) with percentile readout.
/// Not thread-safe: only touched on the main thread (GPU `render()` and CPU `draw(_:)`).
public struct FrameMetricsRecorder: Sendable {
    public struct Stats: Equatable, Sendable {
        public let count: Int
        public let meanNs: UInt64
        public let p50Ns: UInt64
        public let p95Ns: UInt64
        public let p99Ns: UInt64
        public let maxNs: UInt64

        public init(count: Int, meanNs: UInt64, p50Ns: UInt64, p95Ns: UInt64, p99Ns: UInt64, maxNs: UInt64) {
            self.count = count
            self.meanNs = meanNs
            self.p50Ns = p50Ns
            self.p95Ns = p95Ns
            self.p99Ns = p99Ns
            self.maxNs = maxNs
        }
    }

    private let capacity: Int
    private var samples: [UInt64] = []
    private var writeIndex = 0

    public init(capacity: Int = 240) {
        self.capacity = max(1, capacity)
        samples.reserveCapacity(self.capacity)
    }

    public var sampleCount: Int {
        samples.count
    }

    public mutating func record(_ ns: UInt64) {
        if samples.count < capacity {
            samples.append(ns)
        } else {
            samples[writeIndex] = ns
        }
        writeIndex = (writeIndex + 1) % capacity
    }

    public mutating func reset() {
        samples.removeAll(keepingCapacity: true)
        writeIndex = 0
    }

    public func stats() -> Stats? {
        guard !samples.isEmpty else { return nil }
        let sorted = samples.sorted()
        let n = sorted.count
        // `&+`: an outlier must not trap the metrics path.
        let sum = sorted.reduce(UInt64(0)) { $0 &+ $1 }

        func nearestRank(_ p: Double) -> UInt64 {
            let rank = Int((p * Double(n)).rounded(.up))
            let idx = min(max(rank - 1, 0), n - 1)
            return sorted[idx]
        }

        return Stats(
            count: n,
            meanNs: sum / UInt64(n),
            p50Ns: nearestRank(0.50),
            p95Ns: nearestRank(0.95),
            p99Ns: nearestRank(0.99),
            maxNs: sorted[n - 1],
        )
    }
}
