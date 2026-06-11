//! Cron-driven trigger scheduler. Owns one worker thread that wakes
//! on the soonest next-fire time of any registered cron schedule,
//! invokes the trigger's action via `TriggerEngine::dispatch_cron`,
//! then re-computes. Hot-reload via `reload()` swaps the compiled
//! schedule list and wakes the worker so it picks up the new soonest
//! fire time without waiting for whatever sleep it was in.
//!
//! Wake-from-sleep safety: when the worker wakes, it re-checks the
//! wall clock against the originally-scheduled fire time. If the
//! actual wake landed AFTER the scheduled time + a grace window
//! (e.g. the laptop slept past it), the missed slot is skipped and
//! the worker advances to the NEXT future fire. No catchup runs in
//! v1 — `on_missed = skip` is the only policy.
//!
//! All cron parse failures drop the offending trigger with a warning;
//! a single bad schedule never poisons the whole scheduler.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Local};
use cron::Schedule;

/// Maximum drift between a slot's scheduled `next_fire` and the
/// wall-clock time at fire-evaluation. Within this window, the slot
/// is treated as "the one we just woke for" and fires normally;
/// beyond it, the slot is considered missed (the daemon was stalled,
/// the laptop slept, etc.) and silently advances without firing —
/// the skip-only contract from the v1 plan. 5 seconds is generous
/// enough to absorb scheduler/CPU spikes but tight enough that a
/// real sleep-through is recognized.
const MISSED_RUN_GRACE: ChronoDuration = ChronoDuration::seconds(5);

use copad_core::context::ContextService;
use copad_core::trigger::{Trigger, TriggerEngine, validate_source_invariants};

/// Single compiled cron schedule + the surrounding trigger metadata
/// the scheduler needs at fire time. `next_fire` is recomputed
/// (`schedule.after(&now).next()`) at reload and after each fire so
/// the same wall-clock slot is never fired twice. `None` means the
/// schedule has no future fires (e.g. a one-shot date in the past
/// or a malformed window) — the slot stays dormant until reload.
struct CompiledCron {
    trigger: Trigger,
    schedule: Schedule,
    next_fire: Option<DateTime<Local>>,
}

impl CompiledCron {
    /// Recompute `next_fire` as the soonest schedule occurrence
    /// STRICTLY after `now`. Using `after(&now).next()` (rather than
    /// `upcoming(Local).next()`) means a slot that just fired at
    /// `now` advances to its NEXT occurrence — otherwise an
    /// every-second schedule would re-fire on the same second.
    fn advance(&mut self, now: DateTime<Local>) {
        self.next_fire = self.schedule.after(&now).next();
    }
}

/// Inner state guarded by the mutex. Separated from the outer struct
/// so the worker's `condvar.wait` can release the lock while sleeping.
struct State {
    schedules: Vec<CompiledCron>,
    /// Set true on shutdown; worker checks under the mutex on every wake.
    shutdown: bool,
    /// Bumped on every `reload()` call; worker reads under the mutex to
    /// know whether its current sleep horizon is still valid.
    generation: u64,
}

pub struct CronScheduler {
    inner: Arc<(Mutex<State>, Condvar)>,
    engine: Arc<TriggerEngine>,
    context: Arc<ContextService>,
}

impl CronScheduler {
    pub fn new(engine: Arc<TriggerEngine>, context: Arc<ContextService>) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(State {
                    schedules: Vec::new(),
                    shutdown: false,
                    generation: 0,
                }),
                Condvar::new(),
            )),
            engine,
            context,
        }
    }

    /// Replace the active schedule list. Drops triggers whose cron
    /// string fails to parse (logged with the trigger name). The
    /// worker is notified so it picks up the new soonest fire time.
    /// Also rejects triggers that violate the shared invariants
    /// (cron + condition/await etc.) — same validation core uses, so
    /// scheduler-visible and engine-visible trigger sets agree.
    pub fn reload(&self, triggers: &[Trigger]) {
        let compiled: Vec<CompiledCron> = triggers
            .iter()
            .filter(|t| t.is_cron())
            .filter_map(|t| {
                if let Err(reason) = validate_source_invariants(t) {
                    log::warn!("cron trigger {:?} dropped: {reason}", t.name);
                    return None;
                }
                let raw = t.cron.as_ref().expect("is_cron filter guarantees Some");
                match raw.parse::<Schedule>() {
                    Ok(s) => {
                        let mut c = CompiledCron {
                            trigger: t.clone(),
                            schedule: s,
                            next_fire: None,
                        };
                        c.advance(Local::now());
                        Some(c)
                    }
                    Err(e) => {
                        log::warn!(
                            "cron trigger {:?} schedule {:?} parse error: {e}",
                            t.name,
                            raw
                        );
                        None
                    }
                }
            })
            .collect();
        let count = compiled.len();
        {
            let (lock, cvar) = &*self.inner;
            let mut state = lock.lock().unwrap();
            state.schedules = compiled;
            state.generation = state.generation.wrapping_add(1);
            cvar.notify_all();
        }
        log::info!("cron scheduler reloaded ({} cron triggers)", count);
    }

    /// Signal shutdown and wake the worker so it returns promptly.
    /// Caller still needs to `join()` the thread handle from `spawn()`.
    pub fn shutdown(&self) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap();
        state.shutdown = true;
        cvar.notify_all();
    }

    /// Spawn the worker thread. Returns a JoinHandle the caller owns;
    /// invoke `shutdown()` then `.join()` to stop cooperatively.
    pub fn spawn(&self) -> JoinHandle<()> {
        let inner = self.inner.clone();
        let engine = self.engine.clone();
        let context = self.context.clone();
        thread::Builder::new()
            .name("copad-cron-scheduler".into())
            .spawn(move || worker_loop(inner, engine, context))
            .expect("spawn cron scheduler thread")
    }
}

fn worker_loop(
    inner: Arc<(Mutex<State>, Condvar)>,
    engine: Arc<TriggerEngine>,
    context: Arc<ContextService>,
) {
    let (lock, cvar) = &*inner;
    loop {
        let mut state = lock.lock().unwrap();
        if state.shutdown {
            return;
        }
        if state.schedules.is_empty() {
            // No work. Park until reload or shutdown.
            state = cvar.wait(state).unwrap();
            if state.shutdown {
                return;
            }
            continue;
        }

        let now = Local::now();

        // Walk schedules under the lock, partitioning into:
        //   - `to_fire`: `next_fire` in `[now - GRACE, now]` — the
        //     slot we explicitly woke for (or a near-miss from CPU
        //     spike). Fire normally.
        //   - `to_skip`: `next_fire < now - GRACE` — missed slot
        //     (daemon stalled or laptop slept past it). Advance
        //     without firing — skip-only missed-run policy.
        //   - else: future fire — leave alone, contributes to the
        //     sleep horizon.
        // In all (fire OR skip) cases, advance the slot's `next_fire`
        // so the same wall-clock occurrence can't be matched twice on
        // subsequent ticks. Future-only slots are untouched.
        let mut to_fire: Vec<Trigger> = Vec::new();
        let mut skipped_names: Vec<String> = Vec::new();
        for cron in state.schedules.iter_mut() {
            let Some(next) = cron.next_fire else { continue };
            if next > now {
                continue;
            }
            if next < now - MISSED_RUN_GRACE {
                skipped_names.push(cron.trigger.name.clone());
            } else {
                to_fire.push(cron.trigger.clone());
            }
            cron.advance(now);
        }
        let snapshot_gen = state.generation;
        // Compute sleep horizon while we still hold the lock (post-
        // advance). Cron expressions always produce a future
        // occurrence, so `next_fire` is `Some` after `advance()`
        // unless the schedule has truly exhausted itself.
        let next_horizon = state.schedules.iter().filter_map(|c| c.next_fire).min();
        drop(state);

        for name in &skipped_names {
            log::warn!(
                "cron trigger {:?} missed run (next_fire past {}s grace), skipping",
                name,
                MISSED_RUN_GRACE.num_seconds()
            );
        }

        // Fire path (if any): re-check generation between dispatches so
        // a `reload()` that landed AFTER we cloned the fire list (and
        // removed/replaced this trigger) doesn't result in firing a
        // stale schedule. Generation bumps on every reload — mismatch
        // = abort the fire batch, loop will recompute.
        let fired_any = !to_fire.is_empty();
        for trigger in to_fire {
            let proceed = {
                let cur = lock.lock().unwrap();
                if cur.shutdown {
                    return;
                }
                cur.generation == snapshot_gen
            };
            if !proceed {
                log::debug!(
                    "cron worker: reload invalidated fire batch (gen mismatch), restarting"
                );
                break;
            }
            let snap = context.snapshot();
            match engine.dispatch_cron(&trigger, Some(&snap)) {
                Ok(_) => log::debug!("cron trigger {:?} fired", trigger.name),
                Err(e) => log::warn!(
                    "cron trigger {:?} dispatch returned error: {} {}",
                    trigger.name,
                    e.code,
                    e.message
                ),
            }
        }

        // If we did any work (fire or skip), loop to recompute against
        // a fresh `now`. `next_horizon` and `sleep_for` were computed
        // pre-dispatch, so a slow `dispatch_cron` could push `now` past
        // the horizon and either skip to a future slot incorrectly or
        // mis-classify the just-elapsed window as missed. Recomputing
        // dodges both.
        if fired_any || !skipped_names.is_empty() {
            continue;
        }

        let Some(horizon) = next_horizon else {
            let parked = lock.lock().unwrap();
            if parked.shutdown {
                return;
            }
            let woken = cvar.wait(parked).unwrap();
            if woken.shutdown {
                return;
            }
            continue;
        };
        let sleep_for = (horizon - now).to_std().unwrap_or(Duration::from_secs(0));
        let parked = lock.lock().unwrap();
        if parked.shutdown {
            return;
        }
        // If a reload landed between drop(state) and re-lock, skip the
        // wait — generation already changed, loop should recompute.
        if parked.generation != snapshot_gen {
            continue;
        }
        let (woken, _) = cvar.wait_timeout(parked, sleep_for).unwrap();
        if woken.shutdown {
            return;
        }
        // Reload or timeout — either way, loop and recompute.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_core::event_bus::EventBus;
    use copad_core::trigger::{SecurityBlock, Trigger, TriggerEngine, TriggerSink};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn mk_cron(name: &str, schedule: &str, action: &str) -> Trigger {
        Trigger {
            name: name.into(),
            when: None,
            cron: Some(schedule.into()),
            action: action.into(),
            params: Value::Null,
            condition: None,
            r#await: None,
            security: SecurityBlock::default(),
        }
    }

    struct CountingSink {
        count: Arc<AtomicUsize>,
    }
    impl TriggerSink for CountingSink {
        fn dispatch_action(
            &self,
            _action: &str,
            _params: Value,
        ) -> copad_core::action_registry::ActionResult {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        }
    }

    fn mk_engine_with_counter() -> (Arc<TriggerEngine>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn TriggerSink> = Arc::new(CountingSink {
            count: count.clone(),
        });
        let bus = Arc::new(EventBus::new());
        let engine = Arc::new(TriggerEngine::with_publish_bus(sink, bus));
        (engine, count)
    }

    #[test]
    fn reload_drops_invalid_cron_schedule() {
        let (engine, _) = mk_engine_with_counter();
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(&[
            mk_cron("good", "* * * * * *", "noop"),
            mk_cron("bad", "not a cron string", "noop"),
        ]);
        let state = sched.inner.0.lock().unwrap();
        assert_eq!(state.schedules.len(), 1);
        assert_eq!(state.schedules[0].trigger.name, "good");
    }

    #[test]
    fn reload_ignores_event_triggers() {
        let (engine, _) = mk_engine_with_counter();
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        let evt_trigger = Trigger {
            name: "evt".into(),
            when: Some(copad_core::trigger::WhenSpec {
                event_kind: "x".into(),
                payload_match: Default::default(),
            }),
            cron: None,
            action: "noop".into(),
            params: Value::Null,
            condition: None,
            r#await: None,
            security: SecurityBlock::default(),
        };
        sched.reload(&[evt_trigger, mk_cron("c", "* * * * * *", "noop")]);
        let state = sched.inner.0.lock().unwrap();
        assert_eq!(state.schedules.len(), 1);
        assert_eq!(state.schedules[0].trigger.name, "c");
    }

    #[test]
    fn shutdown_wakes_idle_worker() {
        let (engine, _) = mk_engine_with_counter();
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        // No schedules → worker parks on cvar.wait(state).
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        sched.shutdown();
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "shutdown took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn worker_fires_every_second_cron() {
        // Use a "* * * * * *" (every second) schedule and observe
        // the counting sink advances within 3 seconds. Slightly wider
        // window than codex suggested 2s to dampen flakiness around
        // second-boundary alignment.
        let (engine, count) = mk_engine_with_counter();
        // Register the trigger on the engine too so dispatch_cron's
        // sink call routes correctly. (engine.set_triggers is the only
        // public path that stores cron triggers; scheduler reads via
        // engine.cron_triggers() in production but tests can drive
        // CronScheduler directly.)
        let trigger = mk_cron("tick", "* * * * * *", "noop");
        engine.set_triggers(vec![trigger.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(&[trigger]);
        let handle = sched.spawn();
        let start = Instant::now();
        // Wait up to 3 seconds for at least one fire.
        while count.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        sched.shutdown();
        handle.join().unwrap();
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "expected >=1 fire, got {}",
            count.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn worker_does_not_double_fire_same_slot() {
        // Per-second schedule, 1.5s observation window. Maximum
        // legal fires = 2 (one at the half-second boundary if
        // we happen to start there, one at the next). Anything
        // higher means the worker re-fired the same wall-clock
        // second — the pre-fix bug where `compute_due` looked
        // ahead within a grace window.
        let (engine, count) = mk_engine_with_counter();
        let trigger = mk_cron("tick", "* * * * * *", "noop");
        engine.set_triggers(vec![trigger.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(&[trigger]);
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(1500));
        sched.shutdown();
        handle.join().unwrap();
        let fired = count.load(Ordering::SeqCst);
        assert!(
            fired <= 2,
            "expected <=2 fires in 1.5s, got {fired} (re-fire regression)"
        );
    }

    #[test]
    fn missed_slot_is_skipped_not_fired() {
        // Synthetic stale `next_fire` (well past the grace window) —
        // the worker should advance the slot without firing. We seed
        // state directly because there's no clean way to fake-sleep
        // wall clock from a test.
        let (engine, count) = mk_engine_with_counter();
        let trigger = mk_cron("missed", "0 0 0 1 1 ? 2099", "noop"); // far future
        engine.set_triggers(vec![trigger.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(std::slice::from_ref(&trigger));
        // Tamper: set next_fire to 60s ago (well past 5s grace).
        {
            let mut state = sched.inner.0.lock().unwrap();
            state.schedules[0].next_fire = Some(Local::now() - ChronoDuration::seconds(60));
        }
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(200));
        sched.shutdown();
        handle.join().unwrap();
        // Slot was past the grace window → skipped, not fired.
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "missed slot fired despite grace"
        );
    }

    #[test]
    fn reload_during_fire_invalidates_batch() {
        // Worker loops on a per-second cron. We let it fire once,
        // then reload to a different trigger — subsequent fires must
        // be for the NEW trigger, not the old one. With the
        // generation gate, the worker aborts the stale batch on
        // gen-mismatch.
        let (engine, count) = mk_engine_with_counter();
        let old = mk_cron("old", "* * * * * *", "noop");
        engine.set_triggers(vec![old.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine.clone(), ctx);
        sched.reload(&[old]);
        let handle = sched.spawn();
        // Let at least one fire happen.
        let start = Instant::now();
        while count.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        // Reload to empty trigger list — gen bumps, worker aborts
        // any in-flight batch + parks.
        let baseline = count.load(Ordering::SeqCst);
        sched.reload(&[]);
        thread::sleep(Duration::from_secs(2));
        let after = count.load(Ordering::SeqCst);
        sched.shutdown();
        handle.join().unwrap();
        // The post-reload count should be <= baseline + 1 (race
        // window where a fire might land between snapshot_gen read
        // and dispatch). Without the gate, count would keep ticking
        // up at ~1/s for 2s.
        assert!(
            after <= baseline + 1,
            "reload didn't stop fires: baseline {baseline}, after {after}"
        );
    }

    #[test]
    fn reload_wakes_sleeping_worker() {
        // Worker sleeping until a far-future fire; reload swaps in a
        // soon-firing schedule and the worker picks it up without
        // having to wait for the original far-future slot.
        let (engine, count) = mk_engine_with_counter();
        let far = mk_cron("far", "0 0 0 1 1 ? 2099", "noop"); // 2099-01-01 — far future
        engine.set_triggers(vec![far.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine.clone(), ctx);
        sched.reload(&[far]);
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(100));
        // Swap in a per-second schedule.
        let soon = mk_cron("soon", "* * * * * *", "noop");
        engine.set_triggers(vec![soon.clone()]);
        sched.reload(&[soon]);
        let start = Instant::now();
        while count.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        sched.shutdown();
        handle.join().unwrap();
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "reload did not wake worker"
        );
    }
}
