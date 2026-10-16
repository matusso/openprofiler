//! JNI glue to the boot classes (`dev.openprofiler.boot.*`): native method bindings and upcalls.

use crate::collector::now_nanos;
use crate::jvm::{jni, jstring_to_string, new_string, new_string_array, string_array, take_exception};
use crate::{agent, log};
use jvmti_sys::*;
use openprofiler_model::{Bookmark, GcEvent};
use openprofiler_proto::v1;
use std::ffi::c_void;
use std::ptr;

/// Global references and method ids for upcalls.
pub struct JavaHelpers {
    agent_class: jclass,
    m_start: jmethodID,
    m_jvm_info: jmethodID,
    m_dump_heap: jmethodID,
    m_thread_dump: jmethodID,
    m_register_park: jmethodID,
    m_mbean: jmethodID,
    pub m_thread_id: jmethodID,
}

// SAFETY: global references and method ids are valid on any attached thread.
unsafe impl Send for JavaHelpers {}
// SAFETY: see above.
unsafe impl Sync for JavaHelpers {}

macro_rules! native {
    ($name:literal, $sig:literal, $f:expr) => {
        JNINativeMethod {
            name: concat!($name, "\0").as_ptr() as *mut _,
            signature: concat!($sig, "\0").as_ptr() as *mut _,
            fnPtr: $f as *mut c_void,
        }
    };
}

/// Defines the embedded boot classes in the bootstrap class loader (`DefineClass` with a null
/// loader). Classes that already exist (second agent instance, `bootjar=`) are left alone.
fn define_boot_classes(jni: *mut JNIEnv) {
    let mut b = crate::BOOT_CLASSES;
    let take = |b: &mut &[u8], n: usize| -> Option<Vec<u8>> {
        let (h, t) = (b.get(..n)?.to_vec(), b.get(n..)?);
        *b = t;
        Some(h)
    };
    let read_u32 = |b: &mut &[u8]| take(b, 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]) as usize);
    while !b.is_empty() {
        let Some(nl) = read_u32(&mut b) else { break };
        let Some(name) = take(&mut b, nl) else { break };
        let Some(len) = read_u32(&mut b) else { break };
        let Some(bytes) = take(&mut b, len) else { break };
        let Ok(cname) = std::ffi::CString::new(name) else { continue };
        let existing = jni!(jni, FindClass, cname.as_ptr());
        if !existing.is_null() {
            jni!(jni, DeleteLocalRef, existing);
            continue;
        }
        jni!(jni, ExceptionClear);
        let k = jni!(
            jni,
            DefineClass,
            cname.as_ptr(),
            ptr::null_mut(),
            bytes.as_ptr() as *const jbyte,
            len as jsize
        );
        if k.is_null() {
            if let Some(e) = take_exception(jni) {
                log(&format!("cannot define {}: {e}", cname.to_string_lossy()));
            }
        } else {
            jni!(jni, DeleteLocalRef, k);
        }
    }
}

/// Loads the boot classes, binds natives, and resolves upcall method ids.
pub fn init(jni: *mut JNIEnv) -> Result<JavaHelpers, String> {
    define_boot_classes(jni);
    let thread_class = jni!(jni, FindClass, c"java/lang/Thread".as_ptr());
    let m_thread_id = jni!(jni, GetMethodID, thread_class, c"getId".as_ptr(), c"()J".as_ptr());
    jni!(jni, DeleteLocalRef, thread_class);
    let local = jni!(jni, FindClass, c"dev/openprofiler/boot/Agent".as_ptr());
    if local.is_null() {
        return Err(take_exception(jni).unwrap_or_else(|| "dev.openprofiler.boot.Agent not found".into()));
    }
    let methods = [
        native!("telemetryKeys0", "([Ljava/lang/String;[Ljava/lang/String;[I)V", telemetry_keys0),
        native!("telemetry0", "(J[D)V", telemetry0),
        native!("gcEvent0", "(JJLjava/lang/String;Ljava/lang/String;JJJ)V", gc_event0),
        native!("bookmark0", "(Ljava/lang/String;Ljava/lang/String;)V", bookmark0),
        native!("setRecording0", "(Ljava/lang/String;Z)V", set_recording0),
        native!("saveSnapshot0", "(Ljava/lang/String;)Ljava/lang/String;", save_snapshot0),
        native!("registerPark0", "(Ljava/lang/Class;)Z", register_park0),
    ];
    if jni!(jni, RegisterNatives, local, methods.as_ptr(), methods.len() as jint) != 0 {
        return Err(take_exception(jni).unwrap_or_else(|| "RegisterNatives failed".into()));
    }
    let agent_class = jni!(jni, NewGlobalRef, local) as jclass;
    jni!(jni, DeleteLocalRef, local);
    let sm = |name: &std::ffi::CStr, sig: &std::ffi::CStr| {
        let m = jni!(jni, GetStaticMethodID, agent_class, name.as_ptr(), sig.as_ptr());
        if m.is_null() {
            take_exception(jni);
        }
        m
    };
    Ok(JavaHelpers {
        agent_class,
        m_start: sm(c"start", c"()V"),
        m_jvm_info: sm(c"jvmInfo", c"()[Ljava/lang/String;"),
        m_dump_heap: sm(c"dumpHeap", c"(Ljava/lang/String;Z)Ljava/lang/String;"),
        m_thread_dump: sm(c"threadDump", c"()[Ljava/lang/String;"),
        m_register_park: sm(c"registerPark", c"(Ljava/lang/Class;)Z"),
        m_mbean: sm(c"mbean", c"(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;"),
        m_thread_id,
    })
}

impl JavaHelpers {
    pub fn start(&self, jni: *mut JNIEnv) {
        if !self.m_start.is_null() {
            jni!(jni, CallStaticVoidMethodA, self.agent_class, self.m_start, ptr::null());
            if let Some(e) = take_exception(jni) {
                log(&format!("Agent.start failed: {e}"));
            }
        }
    }

    pub fn jvm_info(&self, jni: *mut JNIEnv) -> v1::JvmInfo {
        let mut info = v1::JvmInfo { pid: std::process::id(), ..Default::default() };
        if self.m_jvm_info.is_null() {
            return info;
        }
        let arr = jni!(jni, CallStaticObjectMethodA, self.agent_class, self.m_jvm_info, ptr::null());
        take_exception(jni);
        let kv = string_array(jni, arr);
        jni!(jni, DeleteLocalRef, arr);
        for pair in kv.chunks(2) {
            let [k, v] = pair else { continue };
            match k.as_str() {
                "pid" => info.pid = v.parse().unwrap_or(info.pid),
                "name" => info.name = v.clone(),
                "vm_name" => info.vm_name = v.clone(),
                "vm_version" => info.vm_version = v.clone(),
                "vm_vendor" => info.vm_vendor = v.clone(),
                "java_version" => info.java_version = v.clone(),
                "jvm_args" => {
                    info.jvm_args = v.split('\u{1}').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "main_class" => info.main_class = v.clone(),
                "os" => info.os = v.clone(),
                "arch" => info.arch = v.clone(),
                "cpus" => info.cpus = v.parse().unwrap_or(0),
                "max_heap" => info.max_heap = v.parse().unwrap_or(0),
                "start_time" => info.start_time = v.parse::<i64>().unwrap_or(0) * 1_000_000,
                _ => {}
            }
        }
        info
    }

    /// Writes an HPROF dump to `path`.
    pub fn dump_heap(&self, jni: *mut JNIEnv, path: &str, live: bool) -> Result<(), String> {
        if self.m_dump_heap.is_null() {
            return Err("heap dump helper unavailable".into());
        }
        let jpath = new_string(jni, path);
        let args = [jvalue { l: jpath }, jvalue { z: live as jboolean }];
        let r = jni!(jni, CallStaticObjectMethodA, self.agent_class, self.m_dump_heap, args.as_ptr());
        let exc = take_exception(jni);
        let err = jstring_to_string(jni, r);
        jni!(jni, DeleteLocalRef, r);
        jni!(jni, DeleteLocalRef, jpath);
        match (exc, err) {
            (Some(e), _) | (None, Some(e)) => Err(e),
            (None, None) => Ok(()),
        }
    }

    /// Binds the `Unsafe.park` hook through `Agent.registerPark` (a boot-class caller).
    pub fn register_park(&self, jni: *mut JNIEnv, unsafe_class: jclass) -> bool {
        if self.m_register_park.is_null() {
            return crate::park::register(jni, unsafe_class);
        }
        let args = [jvalue { l: unsafe_class }];
        let ok = jni!(jni, CallStaticBooleanMethodA, self.agent_class, self.m_register_park, args.as_ptr());
        take_exception(jni).is_none() && ok != 0
    }

    pub fn thread_dump(&self, jni: *mut JNIEnv) -> v1::ThreadDump {
        let mut dump = v1::ThreadDump { time: now_nanos(), label: "Thread dump".into(), threads: vec![] };
        if self.m_thread_dump.is_null() {
            return dump;
        }
        let arr = jni!(jni, CallStaticObjectMethodA, self.agent_class, self.m_thread_dump, ptr::null());
        take_exception(jni);
        let lines = string_array(jni, arr);
        jni!(jni, DeleteLocalRef, arr);
        dump.threads = parse_thread_dump(&lines);
        dump
    }
}

impl JavaHelpers {
    /// Calls `Agent.mbean(op, args)` and returns its lines; a single `E\tmessage` line (the
    /// helper's failure format) becomes an error.
    fn mbean(&self, jni: *mut JNIEnv, op: &str, args: &[&str]) -> Result<Vec<String>, String> {
        if self.m_mbean.is_null() {
            return Err("MBean helper unavailable".into());
        }
        let jop = new_string(jni, op);
        let jargs = new_string_array(jni, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let values = [jvalue { l: jop }, jvalue { l: jargs }];
        let arr = jni!(jni, CallStaticObjectMethodA, self.agent_class, self.m_mbean, values.as_ptr());
        let exc = take_exception(jni);
        let lines = string_array(jni, arr);
        for r in [arr, jop, jargs] {
            jni!(jni, DeleteLocalRef, r);
        }
        if let Some(e) = exc {
            return Err(e);
        }
        match lines.first().and_then(|l| l.strip_prefix("E\t")) {
            Some(e) if lines.len() == 1 => Err(e.to_string()),
            _ => Ok(lines),
        }
    }

    /// Runs one MBean request (they all go through `MBeans.call`).
    pub fn mbean_request(
        &self,
        jni: *mut JNIEnv,
        kind: &v1::request::Kind,
    ) -> Result<Option<v1::response::Result>, String> {
        use v1::request::Kind;
        use v1::response::Result as R;
        let flag = |b: bool| if b { "true" } else { "false" };
        let r = match kind {
            Kind::MbeanServers(_) => {
                self.mbean(jni, "servers", &[]).map(|l| R::MbeanServers(parse_mbean_servers(&l)))
            }
            Kind::MbeanConnect(r) => {
                let args = [r.url.as_str(), &r.username, &r.password, flag(r.enabled)];
                self.mbean(jni, "connect", &args).map(|l| R::MbeanServers(parse_mbean_servers(&l)))
            }
            Kind::MbeanList(r) => {
                self.mbean(jni, "list", &[&r.server, &r.pattern]).map(|l| R::MbeanList(parse_mbean_list(&l)))
            }
            Kind::MbeanGet(r) => {
                self.mbean(jni, "get", &[&r.server, &r.name]).map(|l| R::MbeanInfo(parse_mbean_info(&l)))
            }
            Kind::MbeanInvoke(r) => {
                let n = r.signature.len().to_string();
                let mut args = vec![r.server.as_str(), &r.name, &r.operation, &n];
                args.extend(r.signature.iter().map(String::as_str));
                args.extend(r.args.iter().map(String::as_str));
                let lines = self.mbean(jni, "invoke", &args)?;
                let value_json =
                    lines.first().and_then(|l| l.strip_prefix("R\t")).unwrap_or("null").to_string();
                Ok(R::MbeanInvoke(v1::MBeanInvokeResult { value_json }))
            }
            Kind::MbeanSet(r) => {
                return self.mbean(jni, "set", &[&r.server, &r.name, &r.attribute, &r.value]).map(|_| None);
            }
            Kind::MbeanTrack(r) => {
                let unit = r.unit.to_string();
                let args =
                    [r.server.as_str(), &r.name, &r.attribute, &r.item, &r.label, &unit, flag(r.enabled)];
                self.mbean(jni, "track", &args).map(|l| R::MbeanList(parse_mbean_list(&l)))
            }
            Kind::MbeanSubscribe(r) => self
                .mbean(jni, "subscribe", &[&r.server, &r.name, flag(r.enabled)])
                .map(|l| R::MbeanNotifications(parse_mbean_notifications(&l))),
            Kind::MbeanNotifications(r) => self
                .mbean(jni, "notifications", &[&r.after.to_string()])
                .map(|l| R::MbeanNotifications(parse_mbean_notifications(&l))),
            _ => Err("not an MBean request".into()),
        };
        r.map(Some)
    }
}

/// Parses `MBeans.servers()` output.
pub fn parse_mbean_servers(lines: &[String]) -> v1::MBeanServerList {
    let servers = lines
        .iter()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
            (f[0] == "S").then(|| v1::MBeanServerInfo {
                id: at(1),
                kind: at(2),
                label: at(3),
                default_domain: at(4),
                bean_count: at(5).parse().unwrap_or(0),
                error: at(6),
            })
        })
        .collect();
    v1::MBeanServerList { servers }
}

/// Parses `MBeans.notifications()` / `MBeans.subscribe()` output.
pub fn parse_mbean_notifications(lines: &[String]) -> v1::MBeanNotifications {
    let mut out = v1::MBeanNotifications::default();
    for line in lines {
        let f: Vec<&str> = line.splitn(9, '\t').collect();
        let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        match f[0] {
            "N" => out.notifications.push(v1::MBeanNotification {
                seq: at(1).parse().unwrap_or(0),
                time: at(2).parse::<i64>().unwrap_or(0).saturating_mul(1_000_000),
                server: at(3),
                source: at(4),
                r#type: at(5),
                sequence_number: at(6).parse().unwrap_or(0),
                message: at(7),
                data_json: at(8),
            }),
            "U" => out.subscriptions.push(v1::MBeanSubscription { server: at(1), name: at(2) }),
            "L" => {
                out.last_seq = at(1).parse().unwrap_or(0);
                out.missed = at(2).parse().unwrap_or(0);
            }
            _ => {}
        }
    }
    out
}

/// Parses `MBeans.list()` / `MBeans.track()` output.
pub fn parse_mbean_list(lines: &[String]) -> v1::MBeanList {
    let mut out = v1::MBeanList::default();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        match f[0] {
            "B" => out.beans.push(v1::MBeanSummary { name: at(1), class_name: at(2), description: at(3) }),
            "T" => out.tracked.push(v1::TrackedAttribute {
                key: at(1),
                label: at(2),
                unit: at(3).parse().unwrap_or(2),
                mbean: at(4),
                attribute: at(5),
                item: at(6),
                server: at(7),
            }),
            _ => {}
        }
    }
    out
}

/// Parses `MBeans.get()` output.
pub fn parse_mbean_info(lines: &[String]) -> v1::MBeanInfo {
    let mut out = v1::MBeanInfo::default();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        match f[0] {
            "I" => {
                out.name = at(1);
                out.class_name = at(2);
                out.description = at(3);
            }
            "A" => out.attributes.push(v1::MBeanAttribute {
                name: at(1),
                r#type: at(2),
                description: at(3),
                readable: at(4).contains('r'),
                writable: at(4).contains('w'),
                value_json: at(5),
                error: at(6),
            }),
            "O" => out.operations.push(v1::MBeanOperation {
                name: at(1),
                return_type: at(2),
                description: at(3),
                impact: at(4),
                params: vec![],
            }),
            "P" => {
                if let Some(o) = out.operations.last_mut() {
                    o.params.push(v1::MBeanParameter { name: at(1), r#type: at(2), description: at(3) });
                }
            }
            "N" => out.notifications.push(v1::MBeanNotificationInfo {
                name: at(1),
                description: at(2),
                types: f
                    .get(3)
                    .map_or(vec![], |t| t.split(',').filter(|t| !t.is_empty()).map(String::from).collect()),
            }),
            _ => {}
        }
    }
    out
}

/// Parses the line format produced by `Dumps.threadDump()`.
pub fn parse_thread_dump(lines: &[String]) -> Vec<v1::DumpedThread> {
    let mut out: Vec<v1::DumpedThread> = vec![];
    for line in lines {
        let mut parts = line.splitn(2, '\t');
        let tag = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match tag {
            "T" => {
                let f: Vec<&str> = rest.split('\t').collect();
                out.push(v1::DumpedThread {
                    name: f.first().unwrap_or(&"").to_string(),
                    java_id: f.get(1).and_then(|s| s.parse().ok()).unwrap_or(0),
                    state: f.get(2).unwrap_or(&"").to_string(),
                    is_virtual: f.get(3) == Some(&"1"),
                    ..Default::default()
                });
            }
            "F" => {
                if let Some(t) = out.last_mut() {
                    t.frames.push(rest.to_string());
                }
            }
            "L" => {
                if let Some(t) = out.last_mut() {
                    t.locked.push(rest.to_string());
                }
            }
            "W" => {
                if let Some(t) = out.last_mut() {
                    t.waiting_on = rest.to_string();
                }
            }
            _ => {}
        }
    }
    out
}

extern "system" fn telemetry_keys0(
    env: *mut JNIEnv,
    _cls: jclass,
    keys: jobjectArray,
    labels: jobjectArray,
    units: jintArray,
) {
    let Some(a) = agent() else { return };
    let keys = string_array(env, keys);
    let labels = string_array(env, labels);
    let n = jni!(env, GetArrayLength, units).max(0) as usize;
    let mut u = vec![0 as jint; n];
    jni!(env, GetIntArrayRegion, units, 0, n as jint, u.as_mut_ptr());
    let rows = keys
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, labels.get(i).cloned().unwrap_or_default(), u.get(i).copied().unwrap_or(2) as u32))
        .collect();
    a.collector.lock().set_telemetry_keys(rows);
}

extern "system" fn telemetry0(env: *mut JNIEnv, _cls: jclass, time: jlong, values: jdoubleArray) {
    let Some(a) = agent() else { return };
    let n = jni!(env, GetArrayLength, values).max(0) as usize;
    let mut v = vec![0f64; n];
    jni!(env, GetDoubleArrayRegion, values, 0, n as jint, v.as_mut_ptr());
    let keys = {
        let mut c = a.collector.lock();
        c.add_telemetry_row(time, &v);
        c.telemetry_keys()
    };
    crate::triggers::on_telemetry(time, &keys, &v);
}

#[allow(clippy::too_many_arguments)]
extern "system" fn gc_event0(
    env: *mut JNIEnv,
    _cls: jclass,
    start: jlong,
    duration: jlong,
    name: jstring,
    cause: jstring,
    before: jlong,
    after: jlong,
    pause: jlong,
) {
    let Some(a) = agent() else { return };
    let ev = GcEvent {
        start,
        duration,
        name: jstring_to_string(env, name).unwrap_or_default(),
        cause: jstring_to_string(env, cause).unwrap_or_default(),
        heap_before: (before > 0).then_some(before as u64),
        heap_after: (after > 0).then_some(after as u64),
        pause: (pause > 0).then_some(pause),
    };
    a.collector.lock().add_gc(ev);
}

extern "system" fn bookmark0(env: *mut JNIEnv, _cls: jclass, label: jstring, color: jstring) {
    let Some(a) = agent() else { return };
    let b = Bookmark {
        time: now_nanos(),
        label: jstring_to_string(env, label).unwrap_or_default(),
        color: jstring_to_string(env, color),
    };
    a.collector.lock().add_bookmark(b);
}

extern "system" fn set_recording0(env: *mut JNIEnv, _cls: jclass, kind: jstring, enabled: jboolean) {
    let Some(a) = agent() else { return };
    let kind = jstring_to_string(env, kind).unwrap_or_default();
    crate::events::set_recording(a, &kind, enabled != 0, 0);
}

extern "system" fn register_park0(env: *mut JNIEnv, _cls: jclass, unsafe_class: jclass) -> jboolean {
    crate::park::register(env, unsafe_class) as jboolean
}

extern "system" fn save_snapshot0(env: *mut JNIEnv, _cls: jclass, path: jstring) -> jstring {
    let Some(a) = agent() else { return ptr::null_mut() };
    let path = jstring_to_string(env, path).unwrap_or_default();
    match crate::recording::write(a, std::path::Path::new(&path)) {
        Ok(()) => ptr::null_mut(),
        Err(e) => new_string(env, &e.to_string()),
    }
}

/// Java thread id of `thread` (`Thread.getId()`), 0 if unavailable.
pub fn thread_java_id(jni: *mut JNIEnv, thread: jthread) -> u64 {
    let Some(h) = agent().and_then(|a| a.java.get()) else { return 0 };
    if h.m_thread_id.is_null() || thread.is_null() {
        return 0;
    }
    let id = jni!(jni, CallLongMethodA, thread, h.m_thread_id, ptr::null());
    if take_exception(jni).is_some() { 0 } else { id as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dump_lines() {
        let lines: Vec<String> = [
            "T\tmain\t1\tRUNNABLE\t0",
            "F\tapp.Main.run(Main.java:3)",
            "L\tjava.lang.Object@1f",
            "W\tx@2",
            "T\tv\t30\tVIRTUAL\t1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let t = parse_thread_dump(&lines);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].frames.len(), 1);
        assert_eq!(t[0].waiting_on, "x@2");
        assert!(t[1].is_virtual);
    }

    #[test]
    fn parses_mbean_lines() {
        let lines: Vec<String> = [
            "I\tjava.lang:type=Memory\tsun.management.MemoryImpl\tMemory",
            "A\tHeapMemoryUsage\tjavax.management.openmbean.CompositeData\t\tr\t{\"used\":5}\t",
            "A\tVerbose\tboolean\t\trw\tfalse\t",
            "O\tgc\tvoid\t\taction",
            "P\tp0\tint\t",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let i = parse_mbean_info(&lines);
        assert_eq!(i.class_name, "sun.management.MemoryImpl");
        assert_eq!(i.attributes.len(), 2);
        assert!(i.attributes[1].writable && i.attributes[1].readable && !i.attributes[0].writable);
        assert_eq!(i.attributes[0].value_json, "{\"used\":5}");
        assert_eq!(i.operations[0].params[0].r#type, "int");
        let l =
            parse_mbean_list(&["B\ta:type=X\tC\t".into(), "T\tmbean:a:type=X#N\tN\t1\ta:type=X\tN\t".into()]);
        assert_eq!(l.beans[0].name, "a:type=X");
        assert_eq!(l.tracked[0].unit, 1);
    }

    #[test]
    fn parses_mbean_servers_and_notifications() {
        let s = parse_mbean_servers(&["S\t\tplatform\tPlatform\tDefaultDomain\t42\t".into()]);
        assert_eq!(s.servers[0].bean_count, 42);
        assert_eq!(s.servers[0].kind, "platform");
        let n = parse_mbean_notifications(&[
            "N\t3\t1700000000000\t\ta:type=X\tjmx.attribute.change\t7\tchanged\t{\"a\":\"x\ty\"}".into(),
            "U\t\ta:type=X".into(),
            "L\t3\t2".into(),
        ]);
        assert_eq!(n.notifications[0].time, 1_700_000_000_000_000_000);
        assert_eq!(n.notifications[0].data_json, "{\"a\":\"x\ty\"}");
        assert_eq!(n.subscriptions[0].name, "a:type=X");
        assert_eq!((n.last_seq, n.missed), (3, 2));
    }
}
