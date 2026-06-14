import Foundation

/// Fixed-capacity ring buffer of per-frame render durations (ns) with
/// percentile readout — the pure-logic core of the slice-2 render perf
/// harness (docs/macos-gpu-renderer-plan.md). Lives in CopadCore so it
/// stays unit-testable independently of the GUI/Metal layer.
///
/// Both painters feed the same recorder type; the render view labels
/// the report `gpu` vs `cpu` so the same run produces directly
/// comparable numbers under a given workload. The recorder is value
/// type / not thread-safe by design — it is only ever touched on the
/// main thread (both the GPU `render()` call and the CPU `draw(_:)`
/// run there).
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
    /// Next slot to overwrite once `samples` is full — keeps the window
    /// to the most recent `capacity` frames so a long-running session
    /// reports current behavior, not a lifetime average.
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

    /// nil until at least one sample is recorded. Percentiles use the
    /// nearest-rank method (`ceil(p·n)`), which needs no interpolation
    /// and is stable for the small windows (≤ a few hundred frames)
    /// this records.
    public func stats() -> Stats? {
        guard !samples.isEmpty else { return nil }
        let sorted = samples.sorted()
        let n = sorted.count
        // `&+` so a single pathological outlier can't trap the metrics
        // path; realistic frame durations never approach UInt64 range.
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
