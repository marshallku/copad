import Foundation

/// Pure progress math for the Pomodoro status-bar ring (`PomodoroRingView`).
/// Lives in `CopadCore` so it's unit-testable without the AppKit GUI layer —
/// a visual "the arc moved" check at 25-min/16-px granularity can't prove the
/// per-second math, so the math is verified here instead.
public enum PomodoroProgress {
    /// Fraction of the phase REMAINING, clamped to `[0, 1]`. A zero/absent
    /// denominator (idle, missing field, mixed-version daemon) yields `0` so
    /// the ring draws only its track; a clock jump making remaining > total
    /// is clamped rather than overdrawing the circle.
    public static func fraction(remainingMs: Double, totalMs: Double) -> Double {
        guard totalMs > 0 else { return 0 }
        return min(1, max(0, remainingMs / totalMs))
    }

    /// Millis → `MM:SS`, rounding up so a fresh 25:00 shows 25:00 (not 24:59).
    /// Written without `ms + 999` so a pathological near-`UInt64.max` value
    /// (e.g. a malformed payload) can't overflow and trap.
    public static func mmss(_ ms: UInt64) -> String {
        let totalSecs = ms / 1000 + (ms % 1000 == 0 ? 0 : 1)
        return String(format: "%02d:%02d", totalSecs / 60, totalSecs % 60)
    }
}
