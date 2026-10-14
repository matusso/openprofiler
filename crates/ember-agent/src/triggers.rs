//! Triggers: conditions evaluated in the agent that run actions.
//!
//! Sources are the `triggers=<file>` option (JSON, see `ember_proto::triggers`) and the
//! `SetTriggers` request, which replaces the set. Conditions:
//! - timer: on the trigger thread, `delay` after installation, then every `period`;
//! - threshold: on every telemetry row (once per second), edge-triggered with a hold time;
//! - out of memory: JVMTI `ResourceExhausted`;
//! - JVM start: when installed on a live agent; JVM exit: inline in `VMDeath`.
//!
//! Every firing adds a bookmark. Actions run on the "Ember Triggers" thread (exit actions run in
//! `VMDeath` itself), one firing at a time.

use crate::collector::now_nanos;
use crate::jvm::jvmti;
use crate::{Agent, debug, log};
use ember_model::Bookmark;
use ember_proto::v1;
use ember_proto::v1::trigger::Condition;
use ember_proto::v1::trigger_action::Kind;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(100);
const FIRE_COLOR: &str = "#f59e0b";

struct Armed {
    spec: v1::Trigger,
    fires: u32,
    last_fire: Option<Instant>,
    /// Timer: when it fires next.
    next: Option<Instant>,
    /// Threshold: when the condition started to hold (telemetry time), and whether this
    /// excursion already fired.
    holding_since: Option<i64>,
    latched: bool,
}

impl Armed {
    fn new(spec: v1::Trigger) -> Self {
        let installed = Instant::now();
        let next = match &spec.condition {
            Some(Condition::Timer(t)) => Some(installed + Duration::from_millis(t.delay_ms.max(0) as u64)),
            _ => None,
        };
        Self { spec, fires: 0, last_fire: None, next, holding_since: None, latched: false }
    }

    /// Counts a firing if cooldown and `max_fires` allow it; returns the job to run.
    fn fire(&mut self, now: Instant) -> Option<Job> {
        if self.spec.max_fires > 0 && self.fires >= self.spec.max_fires {
            return None;
        }
        if let Some(last) = self.last_fire
            && now.duration_since(last) < Duration::from_millis(self.spec.cooldown_ms.max(0) as u64)
        {
            return None;
        }
        self.fires += 1;
        self.last_fire = Some(now);
        Some(Job { id: self.spec.id.clone(), n: self.fires, actions: self.spec.actions.clone(), why: None })
    }

    /// Feeds a threshold trigger one observation `v` at telemetry time `time`. Fires once the
    /// condition has held for `for_ms`, then not again until it stopped holding.
    fn observe(&mut self, time: i64, v: f64, now: Instant) -> Option<Job> {
        let Some(Condition::Threshold(c)) = &self.spec.condition else { return None };
        let holds = c.above.is_none_or(|x| v > x) && c.below.is_none_or(|x| v < x);
        let (hold_ns, metric) = (c.for_ms.max(0) * 1_000_000, c.metric.clone());
        if !holds {
            self.holding_since = None;
            self.latched = false;
            return None;
        }
        let since = *self.holding_since.get_or_insert(time);
        if self.latched || time - since < hold_ns {
            return None;
        }
        self.latched = true;
        let mut j = self.fire(now)?;
        j.why = Some(format!("{metric} = {v:.1}"));
        Some(j)
    }
}

struct Job {
    id: String,
    n: u32,
    actions: Vec<v1::TriggerAction>,
    /// Extra bookmark text (threshold value, OOM description).
    why: Option<String>,
}

static TRIGGERS: Mutex<Vec<Armed>> = Mutex::new(Vec::new());
static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
static WAKE: Condvar = Condvar::new();

/// Checks a trigger set for things the parser cannot see.
fn validate(triggers: &[v1::Trigger]) -> Result<(), String> {
    for t in triggers {
        if t.condition.is_none() {
            return Err(format!("trigger `{}` has no condition", t.id));
        }
        if t.actions.iter().any(|a| a.kind.is_none()) {
            return Err(format!("trigger `{}` has an empty action", t.id));
        }
        if let Some(Condition::Threshold(c)) = &t.condition
            && c.above.is_none()
            && c.below.is_none()
        {
            return Err(format!("trigger `{}`: threshold needs above or below", t.id));
        }
    }
    Ok(())
}

/// Replaces the trigger set; JVM-start triggers fire right away.
pub fn set(triggers: Vec<v1::Trigger>) -> Result<(), String> {
    validate(&triggers)?;
    let mut armed: Vec<Armed> = triggers.into_iter().map(Armed::new).collect();
    let now = Instant::now();
    let jobs: Vec<Job> = armed
        .iter_mut()
        .filter(|t| matches!(t.spec.condition, Some(Condition::JvmStart(_))))
        .filter_map(|t| t.fire(now))
        .collect();
    debug(&format!("{} trigger(s) installed", armed.len()));
    *TRIGGERS.lock() = armed;
    enqueue(jobs);
    Ok(())
}

fn enqueue(jobs: Vec<Job>) {
    if jobs.is_empty() {
        return;
    }
    QUEUE.lock().extend(jobs);
    WAKE.notify_one();
}

/// Loads the `triggers=` file and starts the trigger thread.
pub fn start(a: &'static Agent) {
    if let Some(path) = &a.opts.triggers {
        let r = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| ember_proto::triggers::parse_json(&text))
            .and_then(set);
        if let Err(e) = r {
            log(&format!("triggers file {}: {e}", path.display()));
        }
    }
    let r = std::thread::Builder::new().name("ember-triggers".into()).spawn(move || run(a));
    if let Err(e) = r {
        log(&format!("cannot start trigger thread: {e}"));
    }
}

fn run(a: &'static Agent) {
    crate::own_thread();
    let Some(jni) = a.vm.attach_daemon("Ember Triggers") else {
        log("triggers: cannot attach to the VM");
        return;
    };
    while !a.shutting_down.load(Ordering::Relaxed) {
        let job = {
            let mut q = QUEUE.lock();
            if q.is_empty() {
                WAKE.wait_for(&mut q, TICK);
            }
            q.pop_front()
        };
        match job {
            Some(j) => run_job(a, jni, j),
            None => check_timers(),
        }
    }
    a.vm.detach();
}

fn check_timers() {
    let now = Instant::now();
    let mut jobs = vec![];
    for t in TRIGGERS.lock().iter_mut() {
        let (Some(Condition::Timer(c)), Some(next)) = (&t.spec.condition, t.next) else { continue };
        if now < next {
            continue;
        }
        t.next = (c.period_ms > 0).then(|| {
            // Skip missed periods rather than firing a burst.
            let period = Duration::from_millis(c.period_ms as u64);
            let mut n = next + period;
            while n <= now {
                n += period;
            }
            n
        });
        jobs.extend(t.fire(now));
    }
    enqueue(jobs);
}

/// Value of `metric` in a telemetry row; `heap.used_pct` is derived from used and max heap.
fn metric(keys: &[String], values: &[f64], name: &str) -> Option<f64> {
    let get = |k: &str| {
        keys.iter().position(|x| x == k).and_then(|i| values.get(i).copied()).filter(|v| v.is_finite())
    };
    if name == "heap.used_pct" {
        let used = get("heap.used")?;
        let max =
            get("heap.max").filter(|&m| m > 0.0).or_else(|| get("heap.committed").filter(|&c| c > 0.0))?;
        return Some(used * 100.0 / max);
    }
    get(name)
}

/// Evaluates threshold triggers against one telemetry row.
pub fn on_telemetry(time: i64, keys: &[String], values: &[f64]) {
    let now = Instant::now();
    let mut jobs = vec![];
    for t in TRIGGERS.lock().iter_mut() {
        let Some(Condition::Threshold(c)) = &t.spec.condition else { continue };
        let Some(v) = metric(keys, values, &c.metric) else { continue };
        jobs.extend(t.observe(time, v, now));
    }
    enqueue(jobs);
}

pub fn on_out_of_memory(flags: i32, description: &str) {
    let now = Instant::now();
    let jobs: Vec<Job> = TRIGGERS
        .lock()
        .iter_mut()
        .filter(|t| matches!(t.spec.condition, Some(Condition::OutOfMemory(_))))
        .filter_map(|t| t.fire(now))
        .map(|mut j| {
            j.why = Some(if description.is_empty() {
                format!("resource exhausted ({flags:#x})")
            } else {
                description.into()
            });
            j
        })
        .collect();
    enqueue(jobs);
}

/// Runs JVM-exit triggers inline (called from `VMDeath`).
pub fn on_exit(a: &Agent, jni: *mut jvmti_sys::JNIEnv) {
    let now = Instant::now();
    let jobs: Vec<Job> = TRIGGERS
        .lock()
        .iter_mut()
        .filter(|t| matches!(t.spec.condition, Some(Condition::JvmExit(_))))
        .filter_map(|t| t.fire(now))
        .collect();
    for j in jobs {
        run_job(a, jni, j);
    }
}

/// Substitutes `{pid}`, `{time}`, `{trigger}` and `{n}` in an action path.
pub fn expand(path: &str, trigger: &str, n: u32) -> String {
    let safe: String = trigger
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect();
    path.replace("{pid}", &std::process::id().to_string())
        .replace("{time}", &(now_nanos() / 1_000_000_000).to_string())
        .replace("{trigger}", &safe)
        .replace("{n}", &n.to_string())
}

fn run_job(a: &Agent, jni: *mut jvmti_sys::JNIEnv, j: Job) {
    let label = match &j.why {
        Some(w) => format!("Trigger {}: {w}", j.id),
        None => format!("Trigger {}", j.id),
    };
    log(&format!("{label} (firing {})", j.n));
    let bookmark = |text: String| {
        a.collector.lock().add_bookmark(Bookmark {
            time: now_nanos(),
            label: text,
            color: Some(FIRE_COLOR.into()),
        });
    };
    bookmark(label);
    for action in &j.actions {
        let Some(kind) = &action.kind else { continue };
        let result: Result<Option<String>, String> = match kind {
            Kind::StartRecording(k) => {
                crate::events::set_recording(a, k, true, 0);
                Ok(None)
            }
            Kind::StopRecording(k) => {
                crate::events::set_recording(a, k, false, 0);
                Ok(None)
            }
            Kind::Bookmark(b) => {
                a.collector.lock().add_bookmark(Bookmark {
                    time: now_nanos(),
                    label: if b.label.is_empty() { j.id.clone() } else { b.label.clone() },
                    color: (!b.color.is_empty()).then(|| b.color.clone()),
                });
                Ok(None)
            }
            Kind::SaveSnapshot(p) => {
                let path = expand(p, &j.id, j.n);
                crate::recording::write(a, std::path::Path::new(&path))
                    .map(|()| Some(format!("snapshot saved to {path}")))
                    .map_err(|e| format!("snapshot {path}: {e}"))
            }
            Kind::HeapDump(h) => match a.java.get() {
                Some(helpers) => {
                    let path = expand(&h.path, &j.id, j.n);
                    if let Some(dir) =
                        std::path::Path::new(&path).parent().filter(|d| !d.as_os_str().is_empty())
                    {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    helpers
                        .dump_heap(jni, &path, h.live_only)
                        .map(|()| Some(format!("heap dump written to {path}")))
                        .map_err(|e| format!("heap dump {path}: {e}"))
                }
                None => Err("heap dump: boot classes unavailable".into()),
            },
            Kind::ThreadDump(_) => match a.java.get() {
                Some(helpers) => {
                    let mut d = helpers.thread_dump(jni);
                    d.label = format!("Trigger {}", j.id);
                    a.collector.lock().add_thread_dump(d);
                    Ok(None)
                }
                None => Err("thread dump: boot classes unavailable".into()),
            },
            Kind::RunGc(_) => {
                let e = jvmti!(a.jvmti, ForceGarbageCollection);
                if e == 0 { Ok(None) } else { Err(format!("ForceGarbageCollection failed: {e}")) }
            }
        };
        match result {
            Ok(Some(msg)) => {
                log(&format!("trigger {}: {msg}", j.id));
                bookmark(msg);
            }
            Ok(None) => {}
            Err(e) => {
                log(&format!("trigger {}: {e}", j.id));
                bookmark(format!("Trigger {} failed: {e}", j.id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threshold(above: f64, for_ms: i64) -> v1::Trigger {
        v1::Trigger {
            id: "t".into(),
            condition: Some(Condition::Threshold(v1::ThresholdCondition {
                metric: "heap.used_pct".into(),
                above: Some(above),
                below: None,
                for_ms,
            })),
            actions: vec![v1::TriggerAction { kind: Some(Kind::RunGc(v1::RunGc {})) }],
            cooldown_ms: 0,
            max_fires: 0,
        }
    }

    #[test]
    fn thresholds_hold_latch_and_rearm() {
        let keys: Vec<String> = ["heap.used", "heap.max"].iter().map(|s| s.to_string()).collect();
        let mut t = Armed::new(threshold(90.0, 2000));
        let mut fired = vec![];
        for (sec, used) in [(0, 95.0), (1, 95.0), (2, 95.0), (3, 96.0), (4, 50.0), (5, 95.0), (7, 95.0)] {
            let v = metric(&keys, &[used, 100.0], "heap.used_pct").unwrap();
            if let Some(j) = t.observe(sec * 1_000_000_000, v, Instant::now()) {
                assert_eq!(j.why.as_deref(), Some("heap.used_pct = 95.0"));
                fired.push(sec);
            }
        }
        assert_eq!(fired, vec![2, 7], "fires once per excursion, after the hold time");
    }

    #[test]
    fn cooldown_and_max_fires() {
        let mut spec = threshold(1.0, 0);
        spec.max_fires = 2;
        spec.cooldown_ms = 60_000;
        let mut t = Armed::new(spec);
        let now = Instant::now();
        assert!(t.fire(now).is_some());
        assert!(t.fire(now + Duration::from_secs(1)).is_none(), "cooldown");
        assert!(t.fire(now + Duration::from_secs(61)).is_some());
        assert!(t.fire(now + Duration::from_secs(200)).is_none(), "max fires");
        assert!(validate(&[v1::Trigger { condition: None, ..threshold(1.0, 0) }]).is_err());
        assert_eq!(expand("/x/{trigger}-{n}.hprof", "heap 90%", 3), "/x/heap_90_-3.hprof");
    }
}
