//! Thin wrappers over the raw JVMTI and JNI function tables.

use jvmti_sys::*;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

/// Calls a JVMTI function: `jvmti!(env, GetPhase, &mut phase)`.
macro_rules! jvmti {
    ($env:expr, $f:ident $(, $a:expr)* $(,)?) => {{
        let e: *mut jvmti_sys::jvmtiEnv = $env.raw();
        // SAFETY: `e` is a valid jvmtiEnv obtained from GetEnv; JVMTI functions are thread-safe and
        // the table entry is always populated for the JVMTI versions we accept.
        unsafe { ((**e).$f.expect(concat!("JVMTI function ", stringify!($f))))(e $(, $a)*) }
    }};
}

/// Calls a JNI function: `jni!(env, FindClass, name)`.
macro_rules! jni {
    ($env:expr, $f:ident $(, $a:expr)* $(,)?) => {{
        let e: *mut jvmti_sys::JNIEnv = $env;
        // SAFETY: `e` is the JNIEnv of the current, attached thread.
        unsafe { ((**e).$f.expect(concat!("JNI function ", stringify!($f))))(e $(, $a)*) }
    }};
}

pub(crate) use {jni, jvmti};

/// A JVMTI environment pointer. JVMTI environments may be used from any thread.
#[derive(Clone, Copy)]
pub struct Jvmti(pub *mut jvmtiEnv);

// SAFETY: JVMTI functions may be called from any thread (attached or not, except where the spec
// requires an attached thread, which callers respect).
unsafe impl Send for Jvmti {}
// SAFETY: see above.
unsafe impl Sync for Jvmti {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvmtiError(pub jvmtiError);

impl std::fmt::Display for JvmtiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JVMTI error {}", self.0)
    }
}

pub type JResult<T> = Result<T, JvmtiError>;

pub fn check(e: jvmtiError) -> JResult<()> {
    if e == JVMTI_ERROR_NONE { Ok(()) } else { Err(JvmtiError(e)) }
}

impl Jvmti {
    pub fn raw(&self) -> *mut jvmtiEnv {
        self.0
    }

    /// Frees memory returned by JVMTI.
    pub fn dealloc<T>(&self, p: *mut T) {
        if !p.is_null() {
            jvmti!(self, Deallocate, p as *mut u8);
        }
    }

    /// Copies and frees a JVMTI-allocated C string.
    pub fn take_string(&self, p: *mut c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: JVMTI returns NUL-terminated modified-UTF-8 strings.
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        self.dealloc(p);
        s
    }

    pub fn phase(&self) -> jvmtiPhase {
        let mut p = 0;
        jvmti!(self, GetPhase, &mut p);
        p
    }

    pub fn add_capabilities(&self, caps: &jvmtiCapabilities) -> JResult<()> {
        check(jvmti!(self, AddCapabilities, caps))
    }

    pub fn potential_capabilities(&self) -> jvmtiCapabilities {
        let mut c = jvmtiCapabilities::default();
        jvmti!(self, GetPotentialCapabilities, &mut c);
        c
    }

    pub fn enable_event(&self, event: jvmtiEvent, on: bool) -> JResult<()> {
        let mode = if on { JVMTI_ENABLE } else { JVMTI_DISABLE };
        check(jvmti!(self, SetEventNotificationMode, mode, event, ptr::null_mut()))
    }

    pub fn set_callbacks(&self, cb: &jvmtiEventCallbacks) -> JResult<()> {
        check(jvmti!(self, SetEventCallbacks, cb, std::mem::size_of::<jvmtiEventCallbacks>() as jint))
    }

    pub fn add_to_boot_classpath(&self, path: &str) -> JResult<()> {
        let c = CString::new(path).map_err(|_| JvmtiError(JVMTI_ERROR_ILLEGAL_ARGUMENT))?;
        check(jvmti!(self, AddToBootstrapClassLoaderSearch, c.as_ptr()))
    }

    pub fn class_signature(&self, klass: jclass) -> JResult<String> {
        let mut sig = ptr::null_mut();
        check(jvmti!(self, GetClassSignature, klass, &mut sig, ptr::null_mut()))?;
        Ok(self.take_string(sig))
    }

    pub fn thread_name(&self, thread: jthread) -> (String, bool) {
        // SAFETY: plain-old-data struct; zero is a valid initial value.
        let mut info: jvmtiThreadInfo = unsafe { std::mem::zeroed() };
        if jvmti!(self, GetThreadInfo, thread, &mut info) != JVMTI_ERROR_NONE {
            return (String::new(), false);
        }
        (self.take_string(info.name), info.is_daemon != 0)
    }

    pub fn get_tag(&self, obj: jobject) -> jlong {
        let mut t = 0;
        jvmti!(self, GetTag, obj, &mut t);
        t
    }

    pub fn set_tag(&self, obj: jobject, tag: jlong) {
        jvmti!(self, SetTag, obj, tag);
    }

    pub fn create_raw_monitor(&self, name: &CStr) -> jrawMonitorID {
        let mut m = ptr::null_mut();
        jvmti!(self, CreateRawMonitor, name.as_ptr(), &mut m);
        m
    }
}

/// The `JavaVM` invocation interface.
#[derive(Clone, Copy)]
pub struct Vm(pub *mut JavaVM);

// SAFETY: the JavaVM pointer is process-global and its functions are thread-safe.
unsafe impl Send for Vm {}
// SAFETY: see above.
unsafe impl Sync for Vm {}

impl Vm {
    /// JNIEnv of the current thread, if attached.
    pub fn env(&self) -> Option<*mut JNIEnv> {
        let mut env: *mut c_void = ptr::null_mut();
        // SAFETY: valid JavaVM; GetEnv only reads thread-local VM state.
        let r = unsafe { ((**self.0).GetEnv.expect("GetEnv"))(self.0, &mut env, JNI_VERSION_1_8) };
        (r == JNI_OK && !env.is_null()).then_some(env as *mut JNIEnv)
    }

    /// Attaches the current native thread as a daemon thread (no-op if already attached).
    pub fn attach_daemon(&self, name: &str) -> Option<*mut JNIEnv> {
        if let Some(e) = self.env() {
            return Some(e);
        }
        let cname = CString::new(name).ok()?;
        let mut args = JavaVMAttachArgs {
            version: JNI_VERSION_1_8,
            name: cname.as_ptr() as *mut c_char,
            group: ptr::null_mut(),
        };
        let mut env: *mut c_void = ptr::null_mut();
        // SAFETY: valid JavaVM; args outlive the call.
        let r = unsafe {
            ((**self.0).AttachCurrentThreadAsDaemon.expect("AttachCurrentThreadAsDaemon"))(
                self.0,
                &mut env,
                &mut args as *mut _ as *mut c_void,
            )
        };
        (r == JNI_OK).then_some(env as *mut JNIEnv)
    }

    pub fn detach(&self) {
        // SAFETY: valid JavaVM; detaching an unattached thread is harmless.
        unsafe { ((**self.0).DetachCurrentThread.expect("DetachCurrentThread"))(self.0) };
    }
}

/// Clears and returns a pending Java exception as text.
pub fn take_exception(env: *mut JNIEnv) -> Option<String> {
    if jni!(env, ExceptionCheck) == 0 {
        return None;
    }
    let exc = jni!(env, ExceptionOccurred);
    jni!(env, ExceptionClear);
    let msg = object_to_string(env, exc).unwrap_or_else(|| "java exception".into());
    jni!(env, DeleteLocalRef, exc);
    Some(msg)
}

fn object_to_string(env: *mut JNIEnv, obj: jobject) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let cls = jni!(env, GetObjectClass, obj);
    let mid = jni!(env, GetMethodID, cls, c"toString".as_ptr(), c"()Ljava/lang/String;".as_ptr());
    jni!(env, DeleteLocalRef, cls);
    if mid.is_null() {
        jni!(env, ExceptionClear);
        return None;
    }
    let s = jni!(env, CallObjectMethodA, obj, mid, ptr::null());
    if jni!(env, ExceptionCheck) != 0 {
        jni!(env, ExceptionClear);
        return None;
    }
    let r = jstring_to_string(env, s);
    jni!(env, DeleteLocalRef, s);
    r
}

pub fn jstring_to_string(env: *mut JNIEnv, s: jstring) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let chars = jni!(env, GetStringUTFChars, s, ptr::null_mut());
    if chars.is_null() {
        return None;
    }
    // SAFETY: GetStringUTFChars returns a NUL-terminated modified-UTF-8 buffer.
    let out = unsafe { CStr::from_ptr(chars) }.to_string_lossy().into_owned();
    jni!(env, ReleaseStringUTFChars, s, chars);
    Some(out)
}

pub fn new_string(env: *mut JNIEnv, s: &str) -> jstring {
    let c = CString::new(s.replace('\0', "")).unwrap_or_default();
    jni!(env, NewStringUTF, c.as_ptr())
}

/// Reads a `String[]` into Rust strings.
pub fn string_array(env: *mut JNIEnv, arr: jobjectArray) -> Vec<String> {
    if arr.is_null() {
        return vec![];
    }
    let n = jni!(env, GetArrayLength, arr);
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let s = jni!(env, GetObjectArrayElement, arr, i);
        out.push(jstring_to_string(env, s).unwrap_or_default());
        jni!(env, DeleteLocalRef, s);
    }
    out
}
