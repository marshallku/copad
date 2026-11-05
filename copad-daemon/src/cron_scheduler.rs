//! Cron-driven trigger scheduler: one worker thread sleeps until the soonest `next_fire`, calls
//! `TriggerEngine::dispatch_cron`, and recomputes. `reload()` swaps the schedules and wakes it.
//!
//! A slot found stale beyond a grace window on wake is skipped, never caught up
//! (`on_missed = skip` is the only policy). A cron parse failure drops only that trigger.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Local};
use cron::Schedule;

/// Max drift between a slot's `next_fire` and the clock at evaluation. Beyond it the slot counts
/// as missed (daemon stalled, laptop slept) and advances without firing.
const MISSED_RUN_GRACE: ChronoDuration = ChronoDuration::seconds(5);

use copad_core::context::ContextService;
use copad_core::trigger::{Trigger, TriggerEngine, validate_source_invariants};

/// `next_fire` is recomputed at reload and after each fire so a slot never fires twice. `None`
/// means no future fires; the slot stays dormant until reload.
struct CompiledCron {
    trigger: Trigger,
    schedule: Schedule,
    next_fire: Option<DateTime<Local>>,
}

impl CompiledCron {
    /// Next occurrence STRICTLY after `now`: `upcoming(Local)` would re-fire an every-second
    /// schedule on the same second.
    fn advance(&mut self, now: DateTime<Local>) {
        self.next_fire = self.schedule.after(&now).next();
    }
}

/// Separate from the outer struct so `condvar.wait` can release the lock while sleeping.
struct State {
    schedules: Vec<CompiledCron>,
    shutdown: bool,
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

    /// Drops triggers whose cron string fails to parse or that violate the shared invariants
    /// (cron + condition/await etc., same validation as core), and wakes the worker.
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

    /// Callers still `join()` the handle from `spawn()`.
    pub fn shutdown(&self) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap();
        state.shutdown = true;
        cvar.notify_all();
    }

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
            state = cvar.wait(state).unwrap();
            if state.shutdown {
                return;
            }
            continue;
        }

        let now = Local::now();

        // Partition under the lock: `to_fire` has `next_fire` in `[now - GRACE, now]`; `to_skip`
        // is older than GRACE (missed slot, advance without firing); future slots stay put.
        // Fired and skipped slots advance so one occurrence can't match twice.
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
        let next_horizon = state.schedules.iter().filter_map(|c| c.next_fire).min();
        drop(state);

        for name in &skipped_names {
            log::warn!(
                "cron trigger {:?} missed run (next_fire past {}s grace), skipping",
                name,
                MISSED_RUN_GRACE.num_seconds()
            );
        }

        // Re-check generation between dispatches: a `reload()` after the fire list was cloned
        // must not fire a stale schedule.
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

        // Recompute against a fresh `now` after any work: `sleep_for` was computed pre-dispatch,
        // so a slow `dispatch_cron` could push `now` past the horizon and misclassify the window.
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
        if parked.generation != snapshot_gen {
            continue;
        }
        let (woken, _) = cvar.wait_timeout(parked, sleep_for).unwrap();
        if woken.shutdown {
            return;
        }
        // Reload or timeout: loop and recompute.
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
        let (engine, count) = mk_engine_with_counter();
        let trigger = mk_cron("tick", "* * * * * *", "noop");
        engine.set_triggers(vec![trigger.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(&[trigger]);
        let handle = sched.spawn();
        let start = Instant::now();
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
        // Max legal fires in 1.5s is 2; more means the same wall-clock second re-fired (the
        // `compute_due` look-ahead bug).
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
        // Seeds a stale `next_fire` directly; wall-clock sleep can't be faked from a test.
        let (engine, count) = mk_engine_with_counter();
        let trigger = mk_cron("missed", "0 0 0 1 1 ? 2099", "noop");
        engine.set_triggers(vec![trigger.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine, ctx);
        sched.reload(std::slice::from_ref(&trigger));
        {
            let mut state = sched.inner.0.lock().unwrap();
            state.schedules[0].next_fire = Some(Local::now() - ChronoDuration::seconds(60));
        }
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(200));
        sched.shutdown();
        handle.join().unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "missed slot fired despite grace"
        );
    }

    #[test]
    fn reload_during_fire_invalidates_batch() {
        // After reloading to a different trigger, later fires must be for the new one: the
        // generation gate aborts the stale batch.
        let (engine, count) = mk_engine_with_counter();
        let old = mk_cron("old", "* * * * * *", "noop");
        engine.set_triggers(vec![old.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine.clone(), ctx);
        sched.reload(&[old]);
        let handle = sched.spawn();
        let start = Instant::now();
        while count.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        // Reload to empty: the generation bump makes the worker abort any in-flight batch.
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
        let (engine, count) = mk_engine_with_counter();
        let far = mk_cron("far", "0 0 0 1 1 ? 2099", "noop");
        engine.set_triggers(vec![far.clone()]);
        let ctx = Arc::new(ContextService::new());
        let sched = CronScheduler::new(engine.clone(), ctx);
        sched.reload(&[far]);
        let handle = sched.spawn();
        thread::sleep(Duration::from_millis(100));
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
