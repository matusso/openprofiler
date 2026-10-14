//! Synchronous sampler: walks all Java threads with `GetThreadListStackTraces`.
//!
//! Always provides wall-clock samples and thread-state history. When the async sampler is not
//! active (macOS, `sampler=sync`, or async setup failed) it also produces CPU samples from the
//! threads that are runnable at each tick.

use crate::Agent;
use crate::collector::now_nanos;
use crate::events::thread_index;
use crate::jvm::{jni, jvmti};
use jvmti_sys::*;
use openprofiler_model::{FrameKind, SampleKind, ThreadState};
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub fn start(a: &'static Agent) {
    let r = std::thread::Builder::new().name("openprofiler-sampler".into()).spawn(move || run(a));
    if let Err(e) = r {
        crate::log(&format!("cannot start sampler: {e}"));
    }
}

/// Maps JVMTI thread state bits to an OpenProfiler thread state.
pub fn map_state(s: jint) -> ThreadState {
    if s & JVMTI_THREAD_STATE_ALIVE == 0 {
        return if s & JVMTI_THREAD_STATE_TERMINATED != 0 {
            ThreadState::Terminated
        } else {
            ThreadState::New
        };
    }
    if s & JVMTI_THREAD_STATE_BLOCKED_ON_MONITOR_ENTER != 0 {
        ThreadState::Blocked
    } else if s & JVMTI_THREAD_STATE_SLEEPING != 0 {
        ThreadState::Sleeping
    } else if s & JVMTI_THREAD_STATE_WAITING != 0 {
        ThreadState::Waiting
    } else if s & JVMTI_THREAD_STATE_RUNNABLE != 0 {
        ThreadState::Runnable
    } else {
        ThreadState::Unknown
    }
}

fn is_net_io(class: &str) -> bool {
    class.starts_with("sun.nio.ch.") || class.starts_with("java.net.")
}

fn run(a: &'static Agent) {
    crate::own_thread();
    let Some(jni) = a.vm.attach_daemon("OpenProfiler Sampler") else {
        crate::log("sampler: cannot attach to the VM");
        return;
    };
    let wall = a.opts.wall_interval;
    let mut cpu_times = HashMap::new();
    let mut next_cpu = Instant::now();
    let mut next_wall = Instant::now();
    while !a.shutting_down.load(Ordering::Relaxed) {
        let cpu_interval = Duration::from_nanos(a.interval_ns.load(Ordering::Relaxed));
        let sync_cpu = a.cpu_on.load(Ordering::Relaxed) && !a.async_active.load(Ordering::Relaxed);
        let now = Instant::now();
        let do_cpu = sync_cpu && now >= next_cpu;
        let do_wall = now >= next_wall;
        if do_cpu || do_wall {
            sample_all(a, jni, do_cpu.then_some(cpu_interval), do_wall.then_some(wall), &mut cpu_times);
            if do_cpu {
                next_cpu = (next_cpu + cpu_interval).max(now);
            }
            if do_wall {
                next_wall = (next_wall + wall).max(now);
            }
        }
        let next = if sync_cpu { next_cpu.min(next_wall) } else { next_wall };
        let sleep = next.saturating_duration_since(Instant::now());
        std::thread::sleep(sleep.max(Duration::from_micros(200)));
    }
    a.vm.detach();
}

/// CPU time a runnable thread consumed since its previous CPU tick, if measurable. Threads that
/// JVMTI reports as RUNNABLE but that are blocked in native code (e.g. `accept`, reference
/// handler) consume ~0 and are not counted as CPU samples.
fn cpu_delta(a: &Agent, thread: jthread, idx: u32, last: &mut HashMap<u32, i64>) -> Option<i64> {
    let mut t: jlong = 0;
    if jvmti!(a.jvmti, GetThreadCpuTime, thread, &mut t) != JVMTI_ERROR_NONE {
        return None;
    }
    let prev = last.insert(idx, t)?;
    Some((t - prev).max(0))
}

fn sample_all(
    a: &Agent,
    jni: *mut JNIEnv,
    cpu: Option<Duration>,
    wall: Option<Duration>,
    cpu_times: &mut HashMap<u32, i64>,
) {
    if jni!(jni, PushLocalFrame, 4096) != 0 {
        jni!(jni, ExceptionClear);
        return;
    }
    let mut n = 0;
    let mut threads: *mut jthread = ptr::null_mut();
    if jvmti!(a.jvmti, GetAllThreads, &mut n, &mut threads) != JVMTI_ERROR_NONE {
        jni!(jni, PopLocalFrame, ptr::null_mut());
        return;
    }
    let all = if threads.is_null() || n <= 0 {
        &[][..]
    } else {
        // SAFETY: JVMTI returned `n` thread references.
        unsafe { std::slice::from_raw_parts(threads, n as usize) }
    };
    let mut list = Vec::with_capacity(all.len());
    let mut idx = Vec::with_capacity(all.len());
    for &t in all {
        if let Some(i) = thread_index(a, jni, t) {
            list.push(t);
            idx.push(i);
        }
    }
    a.jvmti.dealloc(threads);
    let mut info: *mut jvmtiStackInfo = ptr::null_mut();
    let max = a.opts.max_frames as jint;
    if list.is_empty()
        || jvmti!(a.jvmti, GetThreadListStackTraces, list.len() as jint, list.as_ptr(), max, &mut info)
            != JVMTI_ERROR_NONE
    {
        jni!(jni, PopLocalFrame, ptr::null_mut());
        return;
    }
    let now = now_nanos();
    // SAFETY: JVMTI returned one stack info per requested thread.
    let infos = unsafe { std::slice::from_raw_parts(info, list.len()) };
    let thread_cpu = crate::events::has_cap(a, "thread_cpu");
    // CPU weight per thread: `None` = not on CPU this tick.
    let cpu_weight: Vec<Option<u64>> = infos
        .iter()
        .enumerate()
        .map(|(i, si)| {
            let iv = cpu?.as_nanos() as i64;
            if map_state(si.state) != ThreadState::Runnable {
                return None;
            }
            if !thread_cpu {
                return Some(iv as u64);
            }
            let d = cpu_delta(a, list[i], idx[i], cpu_times)?;
            (d >= iv / 20).then(|| d.min(iv * 2) as u64)
        })
        .collect();
    {
        let mut c = a.collector.lock();
        let c = &mut *c;
        let mut ids = Vec::with_capacity(max as usize);
        for ((si, &ti), &weight) in infos.iter().zip(&idx).zip(&cpu_weight) {
            let mut state = map_state(si.state);
            if matches!(state, ThreadState::New | ThreadState::Terminated) {
                continue;
            }
            let frames = if si.frame_buffer.is_null() || si.frame_count <= 0 {
                &[][..]
            } else {
                // SAFETY: frame_buffer holds frame_count entries.
                unsafe { std::slice::from_raw_parts(si.frame_buffer, si.frame_count as usize) }
            };
            ids.clear();
            for f in frames {
                ids.push(c.resolver.frame(a.jvmti, &mut c.symbols, f.method, f.location, FrameKind::Unknown));
            }
            if state == ThreadState::Runnable
                && let Some(&leaf) = ids.first()
            {
                let fr = c.symbols.frame(leaf);
                let class = c.symbols.str(c.symbols.method(fr.method).class);
                if fr.kind == FrameKind::Native && is_net_io(class) {
                    state = ThreadState::NetIo;
                }
            }
            let stack = c.stacks.intern_leaf_first(&ids);
            if let Some(w) = weight
                && state == ThreadState::Runnable
                && !ids.is_empty()
            {
                c.add_sample(now, SampleKind::Cpu, ti, state, stack, w, None);
            }
            if let Some(iv) = wall {
                if !ids.is_empty() {
                    c.add_sample(now, SampleKind::Wall, ti, state, stack, iv.as_nanos() as u64, None);
                }
                c.observe_state(ti, state, now);
            }
        }
        if wall.is_some() {
            crate::vthreads::observe_states(c, now);
        }
    }
    a.jvmti.dealloc(info);
    jni!(jni, PopLocalFrame, ptr::null_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states() {
        let alive = JVMTI_THREAD_STATE_ALIVE;
        assert_eq!(map_state(alive | JVMTI_THREAD_STATE_RUNNABLE), ThreadState::Runnable);
        assert_eq!(map_state(alive | JVMTI_THREAD_STATE_BLOCKED_ON_MONITOR_ENTER), ThreadState::Blocked);
        assert_eq!(
            map_state(alive | JVMTI_THREAD_STATE_WAITING | JVMTI_THREAD_STATE_SLEEPING),
            ThreadState::Sleeping
        );
        assert_eq!(
            map_state(alive | JVMTI_THREAD_STATE_WAITING | JVMTI_THREAD_STATE_PARKED),
            ThreadState::Waiting
        );
        assert_eq!(map_state(JVMTI_THREAD_STATE_TERMINATED), ThreadState::Terminated);
    }
}
