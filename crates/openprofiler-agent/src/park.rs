//! `java.util.concurrent` lock and condition waits.
//!
//! Every j.u.c blocking primitive ends in `LockSupport.park*` and then in the native
//! `Unsafe.park(boolean, long)` (virtual threads excepted, see `vthreads`). The agent replaces
//! that native with a wrapper: it reads the thread's `parkBlocker` and, for locks, the blocker's
//! `AbstractOwnableSynchronizer.exclusiveOwnerThread` before parking, and records a park event
//! when the call took at least `monitor_threshold` (see `events::record_park`). Parks without a
//! blocker object (bare `LockSupport.park()`, mostly JDK-internal idle waits) carry nothing to
//! report and are skipped.
//!
//! The original function is read from the `Method` (the native entry sits right after it; its
//! size comes from VMStructs) and the wrapper is installed with JNI `RegisterNatives`, which also
//! invalidates compiled callers. This works both at start-up and after a dynamic attach.

use crate::collector::now_nanos;
use crate::events::thread_index;
use crate::jvm::{jni, jvmti};
use crate::{Agent, agent, debug};
use jvmti_sys::*;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

type ParkFn = unsafe extern "system" fn(*mut JNIEnv, jobject, jboolean, jlong);

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static IDS: OnceLock<Ids> = OnceLock::new();

/// Field ids used to describe what a thread waits for.
struct Ids {
    park_blocker: jfieldID,
    aos_class: jclass,
    exclusive_owner: jfieldID,
}

// SAFETY: field ids and global class references are valid on any attached thread.
unsafe impl Send for Ids {}
// SAFETY: see above.
unsafe impl Sync for Ids {}

/// What a parking thread waits for.
pub struct Blocker {
    /// Local reference to the `parkBlocker`, or null.
    pub object: jobject,
    /// Thread index of the lock's exclusive owner, if any.
    pub owner: Option<u32>,
}

fn init_ids(jni: *mut JNIEnv) -> Option<Ids> {
    let thread = jni!(jni, FindClass, c"java/lang/Thread".as_ptr());
    let aos = jni!(jni, FindClass, c"java/util/concurrent/locks/AbstractOwnableSynchronizer".as_ptr());
    let ids = if thread.is_null() || aos.is_null() {
        None
    } else {
        let park_blocker =
            jni!(jni, GetFieldID, thread, c"parkBlocker".as_ptr(), c"Ljava/lang/Object;".as_ptr());
        let exclusive_owner =
            jni!(jni, GetFieldID, aos, c"exclusiveOwnerThread".as_ptr(), c"Ljava/lang/Thread;".as_ptr());
        (!park_blocker.is_null() && !exclusive_owner.is_null()).then(|| Ids {
            park_blocker,
            aos_class: jni!(jni, NewGlobalRef, aos) as jclass,
            exclusive_owner,
        })
    };
    jni!(jni, ExceptionClear);
    for c in [thread, aos] {
        if !c.is_null() {
            jni!(jni, DeleteLocalRef, c);
        }
    }
    ids
}

/// The blocker `thread` parks on and its owner. Cheap: two field reads and a tag lookup.
pub fn blocker(a: &Agent, jni: *mut JNIEnv, thread: jthread) -> Blocker {
    let mut b = Blocker { object: std::ptr::null_mut(), owner: None };
    let Some(ids) = IDS.get() else { return b };
    if thread.is_null() {
        return b;
    }
    b.object = jni!(jni, GetObjectField, thread, ids.park_blocker);
    if !b.object.is_null() && jni!(jni, IsInstanceOf, b.object, ids.aos_class) != 0 {
        let owner = jni!(jni, GetObjectField, b.object, ids.exclusive_owner);
        if !owner.is_null() {
            b.owner = thread_index(a, jni, owner);
            jni!(jni, DeleteLocalRef, owner);
        }
    }
    b
}

/// The `Unsafe.park` replacement.
unsafe extern "system" fn park_hook(env: *mut JNIEnv, unsafe_: jobject, absolute: jboolean, time: jlong) {
    // SAFETY: `ORIGINAL` holds the VM's implementation, set before the hook was registered.
    let original: ParkFn = unsafe { std::mem::transmute::<usize, ParkFn>(ORIGINAL.load(Ordering::Relaxed)) };
    let Some(a) =
        agent().filter(|a| a.monitor_on.load(Ordering::Relaxed) && !a.shutting_down.load(Ordering::Relaxed))
    else {
        // SAFETY: same arguments as the VM passed to us.
        return unsafe { original(env, unsafe_, absolute, time) };
    };
    let mut thread: jthread = std::ptr::null_mut();
    jvmti!(a.jvmti, GetCurrentThread, &mut thread);
    let b = blocker(a, env, thread);
    if b.object.is_null() {
        // SAFETY: as above.
        return unsafe { original(env, unsafe_, absolute, time) };
    }
    let start = now_nanos();
    // SAFETY: as above.
    unsafe { original(env, unsafe_, absolute, time) };
    let dur = now_nanos() - start;
    if dur as u128 >= a.opts.monitor_threshold.as_nanos()
        && let Some(idx) = thread_index(a, env, thread)
    {
        crate::events::record_park(a, env, idx, start, dur, b.object, b.owner);
    }
    // Local references die with this native frame.
}

/// Address of the native code bound to `cls.name(sig)` (`Method::native_function()`).
///
/// jmethodIDs are opaque (JDK 25+ no longer makes them pointers), so the `Method` is found
/// through the class: `Klass*` from the mirror's `klass` field (read with `Unsafe.getLong`, which
/// is GC-safe), then `InstanceKlass::_methods`, matching name and signature through the constant
/// pool. All of it is metaspace, which GC does not move; offsets come from VMStructs.
#[cfg(unix)]
fn native_function(jni: *mut JNIEnv, cls: jclass, name: &str, sig: &str) -> Option<usize> {
    let v = crate::vmstructs::VmStructs::load(&[
        "java_lang_Class",
        "InstanceKlass",
        "Array<Method*>",
        "Array<int>",
        "Method",
        "ConstMethod",
        "ConstantPool",
        "Symbol",
    ]);
    let klass_offset = v.field("java_lang_Class", "_klass_offset").filter(|f| f.address != 0)?.address;
    // SAFETY: address of the static `java_lang_Class::_klass_offset` (an int).
    let klass_offset = unsafe { *(klass_offset as *const i32) };
    let klass = unsafe_get_long(jni, cls, klass_offset as jlong)? as usize;
    let (methods, len, data) = (
        v.offset("InstanceKlass", "_methods")?,
        // `_length` is only registered for some instantiations; it is the same for all.
        v.offset("Array<Method*>", "_length").or_else(|| v.offset("Array<int>", "_length"))?,
        v.offset("Array<Method*>", "_data")?,
    );
    let (const_method, constants, name_index, sig_index) = (
        v.offset("Method", "_constMethod")?,
        v.offset("ConstMethod", "_constants")?,
        v.offset("ConstMethod", "_name_index")?,
        v.offset("ConstMethod", "_signature_index")?,
    );
    let (cp_size, method_size) = (v.size("ConstantPool")?, v.size("Method")?);
    let (sym_len, sym_body) = (v.offset("Symbol", "_length")?, v.offset("Symbol", "_body")?);
    let word = |p: usize| -> Option<usize> {
        // SAFETY: metaspace structures reached from a live, linked class; null and misaligned
        // pointers are rejected before dereferencing.
        (p != 0 && p.is_multiple_of(size_of::<usize>())).then(|| unsafe { *(p as *const usize) })
    };
    let symbol = |cp: usize, index: u16| -> Option<Vec<u8>> {
        let sym = word(cp + cp_size + index as usize * size_of::<usize>())?;
        if sym == 0 {
            return None;
        }
        // SAFETY: a Symbol: u2 length followed by the bytes.
        let n = unsafe { *((sym + sym_len) as *const u16) } as usize;
        Some(unsafe { std::slice::from_raw_parts((sym + sym_body) as *const u8, n) }.to_vec())
    };
    let array = word(klass + methods)?;
    // SAFETY: Array<Method*>: int length, then the elements.
    let n = unsafe { *((array + len) as *const i32) };
    if !(0..65536).contains(&n) {
        return None;
    }
    for i in 0..n as usize {
        let m = word(array + data + i * size_of::<usize>())?;
        let cm = word(m + const_method)?;
        let cp = word(cm + constants)?;
        // SAFETY: u2 indices in the ConstMethod.
        let (ni, si) = unsafe { (*((cm + name_index) as *const u16), *((cm + sig_index) as *const u16)) };
        if symbol(cp, ni).as_deref() == Some(name.as_bytes())
            && symbol(cp, si).as_deref() == Some(sig.as_bytes())
        {
            let f = word(m + method_size)?;
            return in_libjvm(f).then_some(f);
        }
    }
    None
}

#[cfg(not(unix))]
fn native_function(_: *mut JNIEnv, _: jclass, _: &str, _: &str) -> Option<usize> {
    None
}

/// Whether `f` points into libjvm (a sanity check on everything read above).
#[cfg(unix)]
fn in_libjvm(f: usize) -> bool {
    // SAFETY: zeroed Dl_info is a valid out parameter; dladdr accepts any address.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    (unsafe { libc::dladdr(f as *const c_void, &mut info) }) != 0
        && !info.dli_fname.is_null()
        // SAFETY: dladdr returned a NUL-terminated path.
        && unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy().contains("libjvm")
}

/// `theUnsafe.getLong(object, offset)` on the given Unsafe class.
fn unsafe_get_long(jni: *mut JNIEnv, unsafe_class: jclass, offset: jlong) -> Option<jlong> {
    // The field's type is the class itself: `Ljdk/internal/misc/Unsafe;` or `Lsun/misc/Unsafe;`.
    let sig = std::ffi::CString::new(agent()?.jvmti.class_signature(unsafe_class).ok()?).ok()?;
    let fid = jni!(jni, GetStaticFieldID, unsafe_class, c"theUnsafe".as_ptr(), sig.as_ptr());
    let mid = jni!(jni, GetMethodID, unsafe_class, c"getLong".as_ptr(), c"(Ljava/lang/Object;J)J".as_ptr());
    if fid.is_null() || mid.is_null() {
        jni!(jni, ExceptionClear);
        return None;
    }
    let u = jni!(jni, GetStaticObjectField, unsafe_class, fid);
    if u.is_null() {
        return None;
    }
    let args = [jvalue { l: unsafe_class }, jvalue { j: offset }];
    let v = jni!(jni, CallLongMethodA, u, mid, args.as_ptr());
    jni!(jni, DeleteLocalRef, u);
    if jni!(jni, ExceptionCheck) != 0 {
        jni!(jni, ExceptionClear);
        return None;
    }
    (v != 0).then_some(v)
}

/// Binds `Unsafe.park` to the hook (`ORIGINAL` must be set).
pub fn register(jni: *mut JNIEnv, unsafe_class: jclass) -> bool {
    if ORIGINAL.load(Ordering::SeqCst) == 0 {
        return false;
    }
    let m = [JNINativeMethod {
        name: c"park".as_ptr() as *mut _,
        signature: c"(ZJ)V".as_ptr() as *mut _,
        fnPtr: park_hook as *mut c_void,
    }];
    let ok = jni!(jni, RegisterNatives, unsafe_class, m.as_ptr(), 1) == 0;
    if !ok {
        jni!(jni, ExceptionClear);
    }
    ok
}

/// Installs the hook. Returns whether park events are available.
pub fn install(a: &Agent, jni: *mut JNIEnv) -> bool {
    if ORIGINAL.load(Ordering::SeqCst) != 0 {
        return true;
    }
    let Some(ids) = init_ids(jni) else {
        debug("park hook: Thread.parkBlocker / AbstractOwnableSynchronizer not found");
        return false;
    };
    let _ = IDS.set(ids);
    for class in [c"jdk/internal/misc/Unsafe", c"sun/misc/Unsafe"] {
        let cls = jni!(jni, FindClass, class.as_ptr());
        if cls.is_null() {
            jni!(jni, ExceptionClear);
            continue;
        }
        let mid = jni!(jni, GetMethodID, cls, c"park".as_ptr(), c"(ZJ)V".as_ptr());
        if mid.is_null() {
            jni!(jni, ExceptionClear);
            jni!(jni, DeleteLocalRef, cls);
            continue;
        }
        let mut native: jboolean = 0;
        jvmti!(a.jvmti, IsMethodNative, mid, &mut native);
        let installed = native != 0
            && match native_function(jni, cls, "park", "(ZJ)V") {
                Some(f) => {
                    ORIGINAL.store(f, Ordering::SeqCst);
                    // Registering from a boot-class frame (`Agent.registerPark`) keeps JDK 22+
                    // from warning about re-bound platform natives.
                    let ok = match a.java.get() {
                        Some(h) => h.register_park(jni, cls),
                        None => register(jni, cls),
                    };
                    if !ok {
                        ORIGINAL.store(0, Ordering::SeqCst);
                    }
                    ok
                }
                None => false,
            };
        jni!(jni, DeleteLocalRef, cls);
        if installed {
            debug(&format!("park hook installed on {}", class.to_string_lossy()));
            return true;
        }
        break;
    }
    debug("park hook unavailable (cannot locate the Unsafe.park native)");
    false
}
