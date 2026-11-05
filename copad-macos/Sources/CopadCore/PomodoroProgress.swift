import Foundation

/// Pure progress math for the Pomodoro status-bar ring, kept in `CopadCore` so it is unit-testable.
public enum PomodoroProgress {
/// Fraction of the phase remaining, in `[0, 1]`. A zero total (idle, missing field, mixed-version
/// daemon) yields 0.
    public static func fraction(remainingMs: Double, totalMs: Double) -> Double {
        guard totalMs > 0 else { return 0 }
        return min(1, max(0, remainingMs / totalMs))
    }

/// Millis to `MM:SS`, rounding up. Avoids `ms + 999` so a near-`UInt64.max` payload cannot trap.
    public static func mmss(_ ms: UInt64) -> String {
        let totalSecs = ms / 1000 + (ms % 1000 == 0 ? 0 : 1)
        return String(format: "%02d:%02d", totalSecs / 60, totalSecs % 60)
    }
}
