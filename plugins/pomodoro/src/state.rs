//! Pure Pomodoro state machine: no I/O, no threads, no wall-clock reads.
//!
//! Every transition takes an explicit `now_ms` (epoch millis), so the machine is testable without
//! sleeping; the scheduler thread in `main.rs` is the only reader of the real clock. A running
//! phase carries an absolute `ends_at_ms`, a paused one the frozen `remaining_ms`. The GUI
//! recomputes locally from `ends_at_ms`, so the plugin only emits on transitions.
//!
//! Loop shape: Idle --start--> Work --(expires)--> Break|LongBreak --(expires)--> Idle. Not an
//! infinite auto-loop: an unattended todo-triggered start must not fire toasts forever, so at most
//! two fire per user-initiated Work. Work completion auto-starts the break; break completion waits
//! for an explicit start/toggle.
//!
//! `completed_work_rounds` counts Work phases since the last reset or long break; the Nth work
//! (N = `rounds_before_long`) enters a LongBreak, which resets the counter.

use serde_json::{Value, json};

/// Duration bounds in integer minutes; the cap keeps minute->millis arithmetic far from overflow.
pub const MIN_MINUTES: u32 = 1;
pub const MAX_MINUTES: u32 = 180;
pub const MIN_ROUNDS: u32 = 1;
pub const MAX_ROUNDS: u32 = 12;

/// Label shown next to the timer (e.g. a todo title); capped so a payload cannot bloat the status bar.
pub const MAX_LABEL_CHARS: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Work,
    Break,
    LongBreak,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Work => "work",
            Phase::Break => "break",
            Phase::LongBreak => "long_break",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            // Idle keeps the tomato so the segment reads as "pomodoro" when stopped.
            Phase::Idle | Phase::Work => "🍅",
            Phase::Break => "☕",
            Phase::LongBreak => "🛋",
        }
    }

    fn is_active(self) -> bool {
        !matches!(self, Phase::Idle)
    }
}

/// Validated durations. Construct only via [`Durations::validated`] (or
/// [`Durations::default`]) so out-of-range values never reach the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Durations {
    pub work_min: u32,
    pub break_min: u32,
    pub long_break_min: u32,
    pub rounds_before_long: u32,
}

impl Default for Durations {
    fn default() -> Self {
        Durations {
            work_min: 25,
            break_min: 5,
            long_break_min: 15,
            rounds_before_long: 4,
        }
    }
}

impl Durations {
    /// Validate all fields up front; on `Err` the caller's state is untouched. Never partially
    /// applies.
    pub fn validated(
        work_min: u32,
        break_min: u32,
        long_break_min: u32,
        rounds_before_long: u32,
    ) -> Result<Self, String> {
        check_minutes("work_min", work_min)?;
        check_minutes("break_min", break_min)?;
        check_minutes("long_break_min", long_break_min)?;
        if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&rounds_before_long) {
            return Err(format!(
                "rounds_before_long must be {MIN_ROUNDS}..={MAX_ROUNDS}, got {rounds_before_long}"
            ));
        }
        Ok(Durations {
            work_min,
            break_min,
            long_break_min,
            rounds_before_long,
        })
    }

    fn work_ms(self) -> u64 {
        minutes_to_ms(self.work_min)
    }
    fn break_ms(self) -> u64 {
        minutes_to_ms(self.break_min)
    }
    fn long_break_ms(self) -> u64 {
        minutes_to_ms(self.long_break_min)
    }
}

fn check_minutes(field: &str, v: u32) -> Result<(), String> {
    if (MIN_MINUTES..=MAX_MINUTES).contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "{field} must be {MIN_MINUTES}..={MAX_MINUTES} minutes, got {v}"
        ))
    }
}

/// `u32` minutes -> `u64` millis; `MAX_MINUTES * 60_000` cannot overflow `u64`.
fn minutes_to_ms(min: u32) -> u64 {
    (min as u64) * 60_000
}

/// A completed phase to announce (toast) after the state lock is released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub toast_title: String,
    pub toast_body: String,
}

/// The authoritative timer state, behind a single mutex in `main.rs`; all mutation goes through these methods.
pub struct PomodoroState {
    phase: Phase,
    /// Absolute expiry while running; `Some` iff active and not paused.
    ends_at_ms: Option<u64>,
    paused_remaining_ms: Option<u64>,
    label: String,
    completed_work_rounds: u32,
    durations: Durations,
    /// Full length of the current phase in millis (0 when Idle). Stored because `start(minutes)`
    /// can
    /// override the work length and `set_durations` only affects future phases, so the configured
    /// duration is no denominator for a progress ring. Preserved across pause/resume.
    current_phase_total_ms: u64,
}

impl PomodoroState {
    pub fn new(durations: Durations) -> Self {
        PomodoroState {
            phase: Phase::Idle,
            ends_at_ms: None,
            paused_remaining_ms: None,
            label: String::new(),
            completed_work_rounds: 0,
            durations,
            current_phase_total_ms: 0,
        }
    }

    pub fn durations(&self) -> Durations {
        self.durations
    }

    /// Start (or restart) a Work phase. `minutes` overrides the work length for this session only
    /// (`Err` leaves state untouched). Does not reset the round counter; only
    /// [`PomodoroState::reset`]
    /// or a completed long break does.
    pub fn start(
        &mut self,
        now_ms: u64,
        minutes: Option<u32>,
        label: Option<&str>,
    ) -> Result<(), String> {
        let dur_ms = match minutes {
            Some(m) => {
                check_minutes("minutes", m)?;
                minutes_to_ms(m)
            }
            None => self.durations.work_ms(),
        };
        self.phase = Phase::Work;
        self.ends_at_ms = Some(now_ms.saturating_add(dur_ms));
        self.paused_remaining_ms = None;
        self.current_phase_total_ms = dur_ms;
        if let Some(l) = label {
            self.label = sanitize_label(l);
        }
        Ok(())
    }

    /// Freeze the countdown. Idempotent; returns whether anything changed. A phase that has already
    /// expired stays running so the scheduler can complete it; otherwise a pause winning the mutex
    /// race at expiry would swallow the transition and toast.
    pub fn pause(&mut self, now_ms: u64) -> bool {
        if let Some(ends) = self.ends_at_ms {
            if now_ms >= ends {
                return false;
            }
            self.paused_remaining_ms = Some(ends - now_ms);
            self.ends_at_ms = None;
            true
        } else {
            false
        }
    }

    pub fn resume(&mut self, now_ms: u64) -> bool {
        if let Some(rem) = self.paused_remaining_ms.take() {
            self.ends_at_ms = Some(now_ms.saturating_add(rem));
            true
        } else {
            false
        }
    }

    pub fn toggle(&mut self, now_ms: u64) {
        match self.phase {
            Phase::Idle => {
                let _ = self.start(now_ms, None, None);
            }
            _ if self.paused_remaining_ms.is_some() => {
                self.resume(now_ms);
            }
            _ => {
                self.pause(now_ms);
            }
        }
    }

    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.ends_at_ms = None;
        self.paused_remaining_ms = None;
        self.current_phase_total_ms = 0;
        self.label.clear();
        self.completed_work_rounds = 0;
    }

    /// Apply new durations to future phases only; the running phase keeps its `ends_at_ms`.
    pub fn set_durations(&mut self, durations: Durations) {
        self.durations = durations;
    }

    /// Advance the machine if the active phase has expired, returning the toast to fire after the
    /// lock is dropped. At most one transition per call.
    pub fn tick_if_due(&mut self, now_ms: u64) -> Option<Transition> {
        let ends = self.ends_at_ms?; // None while idle or paused
        if now_ms < ends {
            return None;
        }
        match self.phase {
            Phase::Work => {
                self.completed_work_rounds = self.completed_work_rounds.saturating_add(1);
                let long = self
                    .completed_work_rounds
                    .is_multiple_of(self.durations.rounds_before_long);
                let (next, dur_ms, kind) = if long {
                    (Phase::LongBreak, self.durations.long_break_ms(), "long")
                } else {
                    (Phase::Break, self.durations.break_ms(), "short")
                };
                self.phase = next;
                // Measured from now, so a long scheduler sleep (laptop suspend) does not
                // double-expire it.
                self.ends_at_ms = Some(now_ms.saturating_add(dur_ms));
                self.paused_remaining_ms = None;
                self.current_phase_total_ms = dur_ms;
                Some(Transition {
                    toast_title: "Pomodoro — work done".to_string(),
                    toast_body: with_label(&format!("Time for a {kind} break"), &self.label),
                })
            }
            Phase::Break | Phase::LongBreak => {
                let was_long = self.phase == Phase::LongBreak;
                self.phase = Phase::Idle;
                self.ends_at_ms = None;
                self.paused_remaining_ms = None;
                self.current_phase_total_ms = 0;
                if was_long {
                    self.completed_work_rounds = 0;
                }
                Some(Transition {
                    toast_title: "Pomodoro — break over".to_string(),
                    toast_body: "Ready to focus again".to_string(),
                })
            }
            Phase::Idle => None,
        }
    }

    /// Instant the scheduler should next wake, or `None` to park until an RPC (idle or paused).
    pub fn next_wake_ms(&self) -> Option<u64> {
        self.ends_at_ms
    }

    pub fn is_paused(&self) -> bool {
        self.paused_remaining_ms.is_some()
    }

    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        if let Some(rem) = self.paused_remaining_ms {
            rem
        } else if let Some(ends) = self.ends_at_ms {
            ends.saturating_sub(now_ms)
        } else {
            0
        }
    }

    pub fn status_text(&self, now_ms: u64) -> String {
        if !self.phase.is_active() {
            return format!("{} idle", self.phase.glyph());
        }
        let mut s = format!(
            "{} {}",
            self.phase.glyph(),
            fmt_mmss(self.remaining_ms(now_ms))
        );
        if self.is_paused() {
            s.push_str(" ⏸");
        }
        if !self.label.is_empty() {
            s.push_str(" · ");
            s.push_str(&self.label);
        }
        s
    }

    /// Wire payload for the `pomodoro.state` event and RPC replies: `ends_at_ms` for local ticking,
    /// `remaining_ms` for CLI snapshots, and the preformatted `text`.
    pub fn event_payload(&self, now_ms: u64) -> Value {
        json!({
            "phase": self.phase.as_str(),
            "ends_at_ms": self.ends_at_ms,
            "paused": self.is_paused(),
            "remaining_ms": self.remaining_ms(now_ms),
            "phase_total_ms": self.current_phase_total_ms,
            "label": self.label,
            "round": self.completed_work_rounds,
            "rounds_before_long": self.durations.rounds_before_long,
            "text": self.status_text(now_ms),
        })
    }
}

/// Strip control characters (newlines too, since a toast/status line is single-line) and cap length.
fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn with_label(base: &str, label: &str) -> String {
    if label.is_empty() {
        base.to_string()
    } else {
        format!("{base} · {label}")
    }
}

/// Millis -> `MM:SS`, rounding up so a timer started at 25:00 shows 25:00, not 24:59.
fn fmt_mmss(ms: u64) -> String {
    let total_secs = ms.div_ceil(1000);
    let mm = total_secs / 60;
    let ss = total_secs % 60;
    format!("{mm:02}:{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000_000_000;

    fn state() -> PomodoroState {
        PomodoroState::new(Durations::default())
    }

    #[test]
    fn starts_idle() {
        let s = state();
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.remaining_ms(T0), 0);
        assert!(s.status_text(T0).contains("idle"));
    }

    #[test]
    fn start_sets_work_with_full_duration() {
        let mut s = state();
        s.start(T0, None, Some("refactor")).unwrap();
        assert_eq!(s.phase, Phase::Work);
        assert_eq!(s.remaining_ms(T0), 25 * 60_000);
        assert!(s.status_text(T0).contains("refactor"));
        assert!(s.status_text(T0).starts_with("🍅 25:00"));
    }

    #[test]
    fn start_minutes_override_validated() {
        let mut s = state();
        assert!(s.start(T0, Some(0), None).is_err());
        assert!(s.start(T0, Some(MAX_MINUTES + 1), None).is_err());
        assert_eq!(s.phase, Phase::Idle);
        s.start(T0, Some(1), None).unwrap();
        assert_eq!(s.remaining_ms(T0), 60_000);
    }

    #[test]
    fn pause_then_resume_preserves_remaining() {
        let mut s = state();
        s.start(T0, Some(25), None).unwrap();
        let t1 = T0 + 10 * 60_000;
        assert!(s.pause(t1));
        assert!(s.is_paused());
        assert_eq!(s.remaining_ms(t1), 15 * 60_000);
        let t2 = t1 + 3 * 60_000;
        assert_eq!(s.remaining_ms(t2), 15 * 60_000);
        assert!(s.resume(t2));
        assert!(!s.is_paused());
        assert_eq!(s.remaining_ms(t2), 15 * 60_000);
        assert_eq!(s.remaining_ms(t2 + 15 * 60_000), 0);
    }

    #[test]
    fn pause_at_or_after_expiry_is_declined() {
        // Pausing at or past expiry must not freeze the phase; the scheduler has to complete it.
        let mut s = state();
        s.start(T0, Some(25), None).unwrap();
        let end = T0 + 25 * 60_000;
        assert!(!s.pause(end), "pause at expiry declined");
        assert!(!s.is_paused());
        assert!(s.tick_if_due(end).is_some());
        assert_eq!(s.phase, Phase::Break);
    }

    #[test]
    fn pause_and_resume_are_idempotent() {
        let mut s = state();
        s.start(T0, None, None).unwrap();
        assert!(s.pause(T0));
        assert!(!s.pause(T0)); // already paused
        assert!(s.resume(T0));
        assert!(!s.resume(T0)); // already running
    }

    #[test]
    fn tick_not_due_returns_none() {
        let mut s = state();
        s.start(T0, Some(25), None).unwrap();
        assert!(s.tick_if_due(T0 + 24 * 60_000).is_none());
        assert_eq!(s.phase, Phase::Work);
    }

    #[test]
    fn work_expiry_auto_starts_short_break() {
        let mut s = state();
        s.start(T0, Some(25), Some("task")).unwrap();
        let end = T0 + 25 * 60_000;
        let t = s.tick_if_due(end).expect("work should complete");
        assert!(t.toast_body.contains("short"));
        assert_eq!(s.phase, Phase::Break);
        assert_eq!(s.remaining_ms(end), 5 * 60_000);
        assert!(s.tick_if_due(end).is_none());
    }

    #[test]
    fn break_expiry_returns_to_idle_and_waits() {
        let mut s = state();
        s.start(T0, Some(25), None).unwrap();
        let we = T0 + 25 * 60_000;
        s.tick_if_due(we).unwrap(); // -> Break
        let be = we + 5 * 60_000;
        let t = s.tick_if_due(be).expect("break should complete");
        assert!(t.toast_body.contains("focus"));
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.next_wake_ms(), None); // parks, no auto next work
    }

    #[test]
    fn long_break_every_nth_round_then_resets_counter() {
        let mut s = state();
        let mut now = T0;
        for round in 1..=4 {
            s.start(now, Some(25), None).unwrap();
            now += 25 * 60_000;
            let t = s.tick_if_due(now).unwrap();
            if round < 4 {
                assert_eq!(s.phase, Phase::Break, "round {round} short");
                assert!(t.toast_body.contains("short"));
                now += 5 * 60_000;
            } else {
                assert_eq!(s.phase, Phase::LongBreak, "round 4 long");
                assert!(t.toast_body.contains("long"));
                now += 15 * 60_000;
            }
            s.tick_if_due(now).unwrap(); // break -> idle
        }
        assert_eq!(s.completed_work_rounds, 0);
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = state();
        s.start(T0, Some(25), Some("x")).unwrap();
        s.tick_if_due(T0 + 25 * 60_000).unwrap();
        s.reset();
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.completed_work_rounds, 0);
        assert!(s.status_text(T0).contains("idle"));
    }

    #[test]
    fn set_durations_is_future_only() {
        let mut s = state();
        s.start(T0, None, None).unwrap(); // 25 min work running
        let d = Durations::validated(50, 10, 30, 4).unwrap();
        s.set_durations(d);
        assert_eq!(s.remaining_ms(T0), 25 * 60_000);
        s.start(T0, None, None).unwrap();
        assert_eq!(s.remaining_ms(T0), 50 * 60_000);
    }

    #[test]
    fn label_is_sanitized_and_capped() {
        let mut s = state();
        let nasty = format!("line1\nline2\t{}", "x".repeat(200));
        s.start(T0, None, Some(&nasty)).unwrap();
        let text = s.status_text(T0);
        assert!(!text.contains('\n'));
        assert!(!text.contains('\t'));
        assert!(
            s.event_payload(T0)["label"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= MAX_LABEL_CHARS
        );
    }

    #[test]
    fn toggle_cycles_idle_pause_resume() {
        let mut s = state();
        s.toggle(T0); // idle -> work
        assert_eq!(s.phase, Phase::Work);
        s.toggle(T0); // running -> pause
        assert!(s.is_paused());
        s.toggle(T0); // paused -> resume
        assert!(!s.is_paused());
    }

    #[test]
    fn phase_total_ms_tracks_actual_phase_length() {
        let mut s = state();
        s.start(T0, Some(10), None).unwrap();
        assert_eq!(s.event_payload(T0)["phase_total_ms"], 10 * 60_000);
        let end = T0 + 10 * 60_000;
        s.tick_if_due(end).unwrap();
        assert_eq!(s.event_payload(end)["phase_total_ms"], 5 * 60_000);
        s.pause(end + 60_000);
        assert_eq!(s.event_payload(end + 60_000)["phase_total_ms"], 5 * 60_000);
        s.resume(end + 60_000);
        let be = end + 60_000 + s.remaining_ms(end + 60_000);
        s.tick_if_due(be).unwrap();
        assert_eq!(s.event_payload(be)["phase_total_ms"], 0);
    }

    #[test]
    fn fmt_mmss_rounds_up() {
        assert_eq!(fmt_mmss(25 * 60_000), "25:00");
        assert_eq!(fmt_mmss(1), "00:01");
        assert_eq!(fmt_mmss(0), "00:00");
        assert_eq!(fmt_mmss(59_001), "01:00");
    }
}
