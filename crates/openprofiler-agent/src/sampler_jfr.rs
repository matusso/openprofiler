//! JFR-streaming sampler: CPU samples from the JVM's own JFR sampler instead of signals.
//!
//! The boot class `dev.openprofiler.boot.JfrSampler` opens a `jdk.jfr.consumer.RecordingStream`
//! (JDK 14+) with `jdk.ExecutionSample` or, where the JVM has it (JDK 25+ on Linux),
//! `jdk.CPUTimeSample`, and hands every event's thread and frames to the natives below. Nothing
//! here needs `AsyncGetCallTrace`, `SIGPROF` or VMStructs, so it works where signals are not
//! allowed and keeps working if `AsyncGetCallTrace` goes away. JFR delivers events about once per
//! second, so live CPU views lag by that much.
//!
//! Started with `sampler=jfr`, and on Linux with `sampler=auto` when the async sampler could not
//! start. If the stream cannot be opened, or dies later, the sync sampler takes over CPU samples.

use crate::jvm::{jni, jstring_to_string, take_exception};
use crate::{Agent, JfrEvent, SamplerChoice, agent, log};
use jvmti_sys::*;
use openprofiler_model::{FrameKind, SampleKind, ThreadState};
use std::ffi::c_void;
use std::sync::atomic::Ordering;

macro_rules! native {
    ($name:literal, $sig:literal, $f:expr) => {
        JNINativeMethod {
            name: concat!($name, "\0").as_ptr() as *mut _,
            signature: concat!($sig, "\0").as_ptr() as *mut _,
            fnPtr: $f as *mut c_void,
        }
    };
}

fn wanted(a: &Agent) -> bool {
    match a.opts.sampler {
        SamplerChoice::Jfr => true,
        // Fallback when AsyncGetCallTrace is missing or signals cannot be used.
        SamplerChoice::Auto => cfg!(target_os = "linux") && !a.async_active.load(Ordering::SeqCst),
        SamplerChoice::Async | SamplerChoice::Sync => false,
    }
}

/// Starts the JFR stream if this sampler is wanted; on failure the sync sampler keeps producing
/// CPU samples.
pub fn start(a: &'static Agent, jni: *mut JNIEnv) {
    if !wanted(a) {
        return;
    }
    let explicit = a.opts.sampler == SamplerChoice::Jfr;
    match open(a, jni) {
        Ok(event) => {
            a.jfr_active.store(true, Ordering::SeqCst);
            if explicit {
                crate::debug(&format!("JFR sampler streaming {event}"));
            } else {
                log(&format!("async sampler unavailable; streaming JFR {event} instead"));
            }
        }
        Err(e) => log(&format!("JFR sampler unavailable ({e}); using the sync sampler")),
    }
}

fn open(a: &Agent, jni: *mut JNIEnv) -> Result<String, String> {
    if a.java.get().is_none() {
        return Err("boot classes unavailable".into());
    }
    let cls = jni!(jni, FindClass, c"dev/openprofiler/boot/JfrSampler".as_ptr());
    if cls.is_null() {
        return Err(
            take_exception(jni).unwrap_or_else(|| "dev.openprofiler.boot.JfrSampler not found".into())
        );
    }
    let methods = [
        native!("period0", "()J", period0),
        native!("method0", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I", method0),
        native!("sample0", "(JJJJI[I[I[B)V", sample0),
        native!("stopped0", "(Ljava/lang/String;)V", stopped0),
    ];
    let r = (|| {
        if jni!(jni, RegisterNatives, cls, methods.as_ptr(), methods.len() as jint) != 0 {
            return Err(take_exception(jni).unwrap_or_else(|| "RegisterNatives failed".into()));
        }
        let m = jni!(jni, GetStaticMethodID, cls, c"start".as_ptr(), c"(I)Ljava/lang/String;".as_ptr());
        if m.is_null() {
            return Err(take_exception(jni).unwrap_or_else(|| "JfrSampler.start not found".into()));
        }
        let event = match a.opts.jfr_event {
            JfrEvent::Auto => 0,
            JfrEvent::Execution => 1,
            JfrEvent::CpuTime => 2,
        };
        let args = [jvalue { i: event }];
        let name = jni!(jni, CallStaticObjectMethodA, cls, m, args.as_ptr());
        if let Some(e) = take_exception(jni) {
            return Err(e);
        }
        let s = jstring_to_string(jni, name).unwrap_or_default();
        jni!(jni, DeleteLocalRef, name);
        Ok(s)
    })();
    jni!(jni, DeleteLocalRef, cls);
    r
}

/// Sampling period in nanoseconds; 0 while CPU recording is off, -1 once the agent shuts down.
extern "system" fn period0(_env: *mut JNIEnv, _cls: jclass) -> jlong {
    let Some(a) = agent() else { return -1 };
    if a.shutting_down.load(Ordering::Relaxed) {
        -1
    } else if a.cpu_on.load(Ordering::Relaxed) {
        a.interval_ns.load(Ordering::Relaxed) as jlong
    } else {
        0
    }
}

/// Interns a method (`java.lang.String`, `indexOf`, `(I)I`) and returns its id.
extern "system" fn method0(
    env: *mut JNIEnv,
    _cls: jclass,
    class: jstring,
    name: jstring,
    desc: jstring,
) -> jint {
    let Some(a) = agent() else { return 0 };
    let class = jstring_to_string(env, class).unwrap_or_default();
    let name = jstring_to_string(env, name).unwrap_or_default();
    let desc = jstring_to_string(env, desc).unwrap_or_default();
    a.collector.lock().symbols.intern_method(&class, &name, &desc) as jint
}

/// One CPU sample: `depth` frames, leaf first, as method ids from `method0`, line numbers (-1:
/// unknown) and [`FrameKind`] values.
#[allow(clippy::too_many_arguments)]
extern "system" fn sample0(
    env: *mut JNIEnv,
    _cls: jclass,
    java_id: jlong,
    os_id: jlong,
    time: jlong,
    weight: jlong,
    depth: jint,
    methods: jintArray,
    lines: jintArray,
    kinds: jbyteArray,
) {
    let Some(a) = agent() else { return };
    let n = depth.clamp(0, a.opts.max_frames as jint);
    let mut m = vec![0 as jint; n as usize];
    let mut l = vec![0 as jint; n as usize];
    let mut k = vec![0 as jbyte; n as usize];
    if n > 0 {
        jni!(env, GetIntArrayRegion, methods, 0, n, m.as_mut_ptr());
        jni!(env, GetIntArrayRegion, lines, 0, n, l.as_mut_ptr());
        jni!(env, GetByteArrayRegion, kinds, 0, n, k.as_mut_ptr());
        if jni!(env, ExceptionCheck) != 0 {
            jni!(env, ExceptionClear);
            return;
        }
    }
    let mut c = a.collector.lock();
    let c = &mut *c;
    let thread = c.thread_by_java_id(java_id as u64).or_else(|| c.thread_by_os_id(os_id as u64));
    let Some(thread) = thread else {
        // Not registered yet (the sync sampler picks up threads every wall tick) or our own.
        return;
    };
    let mut ids: Vec<u32> = (0..n as usize)
        .map(|i| c.symbols.intern_frame(m[i] as u32, l[i], FrameKind::from_u8(k[i] as u8)))
        .collect();
    if ids.is_empty() {
        ids.push(c.synthetic_frame("[no Java frames]"));
    }
    if let Some(label) = crate::probes::label_frame_of(thread) {
        ids.push(label);
    }
    let stack = c.stacks.intern_leaf_first(&ids);
    c.add_sample(time, SampleKind::Cpu, thread, ThreadState::Runnable, stack, weight.max(0) as u64, None);
}

/// The stream ended unexpectedly; CPU samples fall back to the sync sampler.
extern "system" fn stopped0(env: *mut JNIEnv, _cls: jclass, why: jstring) {
    let Some(a) = agent() else { return };
    if a.jfr_active.swap(false, Ordering::SeqCst) && !a.shutting_down.load(Ordering::SeqCst) {
        let why = jstring_to_string(env, why).unwrap_or_default();
        log(&format!("JFR sampler stopped ({why}); using the sync sampler"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_kinds_match_the_java_side() {
        // JfrSampler.kind() sends these values.
        for (v, k) in [
            (0, FrameKind::Interpreted),
            (1, FrameKind::Jit),
            (2, FrameKind::Inlined),
            (3, FrameKind::Native),
        ] {
            assert_eq!(FrameKind::from_u8(v), k);
        }
    }
}
