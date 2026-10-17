//! Raw JVMTI bindings.
//!
//! JNI types come from `jni-sys`. The JVMTI function table in [`table`] is generated once from a
//! JDK 21+ `jvmti.h` by `tools/gen_table.py` and committed, so building needs no libclang. The
//! remaining types, constants and the event-callback struct are written by hand against the same
//! header. Only the event callbacks the agent uses carry precise signatures; the others are
//! pointer-sized placeholders with the same layout.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

mod table;

pub use jni_sys::*;
pub use table::jvmtiInterface_1_;

use std::ffi::{c_char, c_void};

pub type jvmtiEnv = *const jvmtiInterface_1_;
pub type jthread = jobject;
pub type jthreadGroup = jobject;
pub type jlocation = jlong;
pub type jrawMonitorID = *mut c_void;

pub type jvmtiError = u32;
pub type jvmtiEvent = u32;
pub type jvmtiEventMode = u32;
pub type jvmtiPhase = u32;
pub type jvmtiVerboseFlag = u32;
pub type jvmtiJlocationFormat = u32;
pub type jvmtiHeapObjectFilter = u32;
pub type jvmtiHeapRootKind = u32;
pub type jvmtiObjectReferenceKind = u32;
pub type jvmtiIterationControl = u32;
pub type jvmtiHeapReferenceKind = u32;
pub type jvmtiPrimitiveType = u32;
pub type jvmtiTimerKind = u32;
pub type jvmtiParamKind = u32;
pub type jvmtiParamTypes = u32;

// Versions.
pub const JVMTI_VERSION_1_2: jint = 0x3001_0200;
pub const JVMTI_VERSION_9: jint = 0x3009_0000;
pub const JVMTI_VERSION_11: jint = 0x300B_0000;
pub const JVMTI_VERSION_21: jint = 0x3015_0000;

// Event modes.
pub const JVMTI_ENABLE: jvmtiEventMode = 1;
pub const JVMTI_DISABLE: jvmtiEventMode = 0;

// Phases.
pub const JVMTI_PHASE_ONLOAD: jvmtiPhase = 1;
pub const JVMTI_PHASE_PRIMORDIAL: jvmtiPhase = 2;
pub const JVMTI_PHASE_START: jvmtiPhase = 6;
pub const JVMTI_PHASE_LIVE: jvmtiPhase = 4;
pub const JVMTI_PHASE_DEAD: jvmtiPhase = 8;

// Thread state bits.
pub const JVMTI_THREAD_STATE_ALIVE: jint = 0x0001;
pub const JVMTI_THREAD_STATE_TERMINATED: jint = 0x0002;
pub const JVMTI_THREAD_STATE_RUNNABLE: jint = 0x0004;
pub const JVMTI_THREAD_STATE_WAITING_INDEFINITELY: jint = 0x0010;
pub const JVMTI_THREAD_STATE_WAITING_WITH_TIMEOUT: jint = 0x0020;
pub const JVMTI_THREAD_STATE_SLEEPING: jint = 0x0040;
pub const JVMTI_THREAD_STATE_WAITING: jint = 0x0080;
pub const JVMTI_THREAD_STATE_IN_OBJECT_WAIT: jint = 0x0100;
pub const JVMTI_THREAD_STATE_PARKED: jint = 0x0200;
pub const JVMTI_THREAD_STATE_BLOCKED_ON_MONITOR_ENTER: jint = 0x0400;
pub const JVMTI_THREAD_STATE_SUSPENDED: jint = 0x10_0000;
pub const JVMTI_THREAD_STATE_INTERRUPTED: jint = 0x20_0000;
pub const JVMTI_THREAD_STATE_IN_NATIVE: jint = 0x40_0000;

// Errors.
pub const JVMTI_ERROR_NONE: jvmtiError = 0;
pub const JVMTI_ERROR_INVALID_THREAD: jvmtiError = 10;
pub const JVMTI_ERROR_THREAD_NOT_ALIVE: jvmtiError = 15;
pub const JVMTI_ERROR_INVALID_CLASS: jvmtiError = 21;
pub const JVMTI_ERROR_CLASS_NOT_PREPARED: jvmtiError = 22;
pub const JVMTI_ERROR_INVALID_METHODID: jvmtiError = 23;
pub const JVMTI_ERROR_NO_MORE_FRAMES: jvmtiError = 31;
pub const JVMTI_ERROR_NOT_FOUND: jvmtiError = 41;
pub const JVMTI_ERROR_UNSUPPORTED_VERSION: jvmtiError = 68;
pub const JVMTI_ERROR_UNSUPPORTED_OPERATION: jvmtiError = 73;
pub const JVMTI_ERROR_NOT_AVAILABLE: jvmtiError = 98;
pub const JVMTI_ERROR_MUST_POSSESS_CAPABILITY: jvmtiError = 99;
pub const JVMTI_ERROR_NULL_POINTER: jvmtiError = 100;
pub const JVMTI_ERROR_ABSENT_INFORMATION: jvmtiError = 101;
pub const JVMTI_ERROR_INVALID_EVENT_TYPE: jvmtiError = 102;
pub const JVMTI_ERROR_ILLEGAL_ARGUMENT: jvmtiError = 103;
pub const JVMTI_ERROR_NATIVE_METHOD: jvmtiError = 104;
pub const JVMTI_ERROR_OUT_OF_MEMORY: jvmtiError = 110;
pub const JVMTI_ERROR_ACCESS_DENIED: jvmtiError = 111;
pub const JVMTI_ERROR_WRONG_PHASE: jvmtiError = 112;
pub const JVMTI_ERROR_INTERNAL: jvmtiError = 113;
pub const JVMTI_ERROR_UNATTACHED_THREAD: jvmtiError = 115;
pub const JVMTI_ERROR_INVALID_ENVIRONMENT: jvmtiError = 116;

// Events.
pub const JVMTI_EVENT_VM_INIT: jvmtiEvent = 50;
pub const JVMTI_EVENT_VM_DEATH: jvmtiEvent = 51;
pub const JVMTI_EVENT_THREAD_START: jvmtiEvent = 52;
pub const JVMTI_EVENT_THREAD_END: jvmtiEvent = 53;
pub const JVMTI_EVENT_CLASS_FILE_LOAD_HOOK: jvmtiEvent = 54;
pub const JVMTI_EVENT_CLASS_LOAD: jvmtiEvent = 55;
pub const JVMTI_EVENT_CLASS_PREPARE: jvmtiEvent = 56;
pub const JVMTI_EVENT_VM_START: jvmtiEvent = 57;
pub const JVMTI_EVENT_EXCEPTION: jvmtiEvent = 58;
pub const JVMTI_EVENT_NATIVE_METHOD_BIND: jvmtiEvent = 67;
pub const JVMTI_EVENT_COMPILED_METHOD_LOAD: jvmtiEvent = 68;
pub const JVMTI_EVENT_COMPILED_METHOD_UNLOAD: jvmtiEvent = 69;
pub const JVMTI_EVENT_DYNAMIC_CODE_GENERATED: jvmtiEvent = 70;
pub const JVMTI_EVENT_DATA_DUMP_REQUEST: jvmtiEvent = 71;
pub const JVMTI_EVENT_MONITOR_WAIT: jvmtiEvent = 73;
pub const JVMTI_EVENT_MONITOR_WAITED: jvmtiEvent = 74;
pub const JVMTI_EVENT_MONITOR_CONTENDED_ENTER: jvmtiEvent = 75;
pub const JVMTI_EVENT_MONITOR_CONTENDED_ENTERED: jvmtiEvent = 76;
pub const JVMTI_EVENT_RESOURCE_EXHAUSTED: jvmtiEvent = 80;
pub const JVMTI_EVENT_GARBAGE_COLLECTION_START: jvmtiEvent = 81;
pub const JVMTI_EVENT_GARBAGE_COLLECTION_FINISH: jvmtiEvent = 82;
pub const JVMTI_EVENT_OBJECT_FREE: jvmtiEvent = 83;
pub const JVMTI_EVENT_VM_OBJECT_ALLOC: jvmtiEvent = 84;
pub const JVMTI_EVENT_SAMPLED_OBJECT_ALLOC: jvmtiEvent = 86;
pub const JVMTI_EVENT_VIRTUAL_THREAD_START: jvmtiEvent = 87;
pub const JVMTI_EVENT_VIRTUAL_THREAD_END: jvmtiEvent = 88;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiThreadInfo {
    pub name: *mut c_char,
    pub priority: jint,
    pub is_daemon: jboolean,
    pub thread_group: jthreadGroup,
    pub context_class_loader: jobject,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiMonitorStackDepthInfo {
    pub monitor: jobject,
    pub stack_depth: jint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiThreadGroupInfo {
    pub parent: jthreadGroup,
    pub name: *mut c_char,
    pub max_priority: jint,
    pub is_daemon: jboolean,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiFrameInfo {
    pub method: jmethodID,
    pub location: jlocation,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiStackInfo {
    pub thread: jthread,
    pub state: jint,
    pub frame_buffer: *mut jvmtiFrameInfo,
    pub frame_count: jint,
}

pub type jvmtiHeapIterationCallback = Option<
    unsafe extern "C" fn(
        class_tag: jlong,
        size: jlong,
        tag_ptr: *mut jlong,
        length: jint,
        user_data: *mut c_void,
    ) -> jint,
>;
pub type jvmtiHeapReferenceCallback = Option<
    unsafe extern "C" fn(
        reference_kind: jvmtiHeapReferenceKind,
        reference_info: *const jvmtiHeapReferenceInfo,
        class_tag: jlong,
        referrer_class_tag: jlong,
        size: jlong,
        tag_ptr: *mut jlong,
        referrer_tag_ptr: *mut jlong,
        length: jint,
        user_data: *mut c_void,
    ) -> jint,
>;
pub type jvmtiPrimitiveFieldCallback = Option<
    unsafe extern "C" fn(
        kind: jvmtiHeapReferenceKind,
        info: *const jvmtiHeapReferenceInfo,
        object_class_tag: jlong,
        object_tag_ptr: *mut jlong,
        value: jvalue,
        value_type: jvmtiPrimitiveType,
        user_data: *mut c_void,
    ) -> jint,
>;
pub type jvmtiArrayPrimitiveValueCallback = Option<
    unsafe extern "C" fn(
        class_tag: jlong,
        size: jlong,
        tag_ptr: *mut jlong,
        element_count: jint,
        element_type: jvmtiPrimitiveType,
        elements: *const c_void,
        user_data: *mut c_void,
    ) -> jint,
>;

#[repr(C)]
pub struct jvmtiHeapCallbacks {
    pub heap_iteration_callback: jvmtiHeapIterationCallback,
    pub heap_reference_callback: jvmtiHeapReferenceCallback,
    pub primitive_field_callback: jvmtiPrimitiveFieldCallback,
    pub array_primitive_value_callback: jvmtiArrayPrimitiveValueCallback,
    pub string_primitive_value_callback: *const c_void,
    pub reserved: [*const c_void; 11],
}

impl Default for jvmtiHeapCallbacks {
    fn default() -> Self {
        Self {
            heap_iteration_callback: None,
            heap_reference_callback: None,
            primitive_field_callback: None,
            array_primitive_value_callback: None,
            string_primitive_value_callback: std::ptr::null(),
            reserved: [std::ptr::null(); 11],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiHeapReferenceInfoField {
    pub index: jint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiHeapReferenceInfoStackLocal {
    pub thread_tag: jlong,
    pub thread_id: jlong,
    pub depth: jint,
    pub method: jmethodID,
    pub location: jlocation,
    pub slot: jint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiHeapReferenceInfoJniLocal {
    pub thread_tag: jlong,
    pub thread_id: jlong,
    pub depth: jint,
    pub method: jmethodID,
}

/// `field`, `array` and `constant_pool` share the single `index` member.
#[repr(C)]
#[derive(Clone, Copy)]
pub union jvmtiHeapReferenceInfo {
    pub field: jvmtiHeapReferenceInfoField,
    pub stack_local: jvmtiHeapReferenceInfoStackLocal,
    pub jni_local: jvmtiHeapReferenceInfoJniLocal,
    pub reserved: [jlong; 8],
}

// Heap reference kinds.
pub const JVMTI_HEAP_REFERENCE_CLASS: jvmtiHeapReferenceKind = 1;
pub const JVMTI_HEAP_REFERENCE_FIELD: jvmtiHeapReferenceKind = 2;
pub const JVMTI_HEAP_REFERENCE_ARRAY_ELEMENT: jvmtiHeapReferenceKind = 3;
pub const JVMTI_HEAP_REFERENCE_CLASS_LOADER: jvmtiHeapReferenceKind = 4;
pub const JVMTI_HEAP_REFERENCE_SIGNERS: jvmtiHeapReferenceKind = 5;
pub const JVMTI_HEAP_REFERENCE_PROTECTION_DOMAIN: jvmtiHeapReferenceKind = 6;
pub const JVMTI_HEAP_REFERENCE_INTERFACE: jvmtiHeapReferenceKind = 7;
pub const JVMTI_HEAP_REFERENCE_STATIC_FIELD: jvmtiHeapReferenceKind = 8;
pub const JVMTI_HEAP_REFERENCE_CONSTANT_POOL: jvmtiHeapReferenceKind = 9;
pub const JVMTI_HEAP_REFERENCE_SUPERCLASS: jvmtiHeapReferenceKind = 10;
pub const JVMTI_HEAP_REFERENCE_JNI_GLOBAL: jvmtiHeapReferenceKind = 21;
pub const JVMTI_HEAP_REFERENCE_SYSTEM_CLASS: jvmtiHeapReferenceKind = 22;
pub const JVMTI_HEAP_REFERENCE_MONITOR: jvmtiHeapReferenceKind = 23;
pub const JVMTI_HEAP_REFERENCE_STACK_LOCAL: jvmtiHeapReferenceKind = 24;
pub const JVMTI_HEAP_REFERENCE_JNI_LOCAL: jvmtiHeapReferenceKind = 25;
pub const JVMTI_HEAP_REFERENCE_THREAD: jvmtiHeapReferenceKind = 26;
pub const JVMTI_HEAP_REFERENCE_OTHER: jvmtiHeapReferenceKind = 27;

// Primitive types (the JNI type signature characters).
pub const JVMTI_PRIMITIVE_TYPE_BOOLEAN: jvmtiPrimitiveType = b'Z' as u32;
pub const JVMTI_PRIMITIVE_TYPE_BYTE: jvmtiPrimitiveType = b'B' as u32;
pub const JVMTI_PRIMITIVE_TYPE_CHAR: jvmtiPrimitiveType = b'C' as u32;
pub const JVMTI_PRIMITIVE_TYPE_SHORT: jvmtiPrimitiveType = b'S' as u32;
pub const JVMTI_PRIMITIVE_TYPE_INT: jvmtiPrimitiveType = b'I' as u32;
pub const JVMTI_PRIMITIVE_TYPE_LONG: jvmtiPrimitiveType = b'J' as u32;
pub const JVMTI_PRIMITIVE_TYPE_FLOAT: jvmtiPrimitiveType = b'F' as u32;
pub const JVMTI_PRIMITIVE_TYPE_DOUBLE: jvmtiPrimitiveType = b'D' as u32;

// Heap visit control flags.
pub const JVMTI_VISIT_OBJECTS: jint = 0x100;
pub const JVMTI_VISIT_ABORT: jint = 0x8000;

// Class status bits.
pub const JVMTI_CLASS_STATUS_PREPARED: jint = 2;
pub const JVMTI_CLASS_STATUS_ARRAY: jint = 16;
pub const JVMTI_CLASS_STATUS_PRIMITIVE: jint = 32;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiClassDefinition {
    pub klass: jclass,
    pub class_byte_count: jint,
    pub class_bytes: *const u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiMonitorUsage {
    pub owner: jthread,
    pub entry_count: jint,
    pub waiter_count: jint,
    pub waiters: *mut jthread,
    pub notify_waiter_count: jint,
    pub notify_waiters: *mut jthread,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiLineNumberEntry {
    pub start_location: jlocation,
    pub line_number: jint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiLocalVariableEntry {
    pub start_location: jlocation,
    pub length: jint,
    pub name: *mut c_char,
    pub signature: *mut c_char,
    pub generic_signature: *mut c_char,
    pub slot: jint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiParamInfo {
    pub name: *mut c_char,
    pub kind: jvmtiParamKind,
    pub base_type: jvmtiParamTypes,
    pub null_ok: jboolean,
}

pub type jvmtiExtensionFunction = Option<unsafe extern "C" fn(jvmti_env: *mut jvmtiEnv, ...) -> jvmtiError>;
pub type jvmtiExtensionEvent = Option<unsafe extern "C" fn(jvmti_env: *mut jvmtiEnv, ...)>;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiExtensionFunctionInfo {
    pub func: jvmtiExtensionFunction,
    pub id: *mut c_char,
    pub short_description: *mut c_char,
    pub param_count: jint,
    pub params: *mut jvmtiParamInfo,
    pub error_count: jint,
    pub errors: *mut jvmtiError,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiExtensionEventInfo {
    pub extension_event_index: jint,
    pub id: *mut c_char,
    pub short_description: *mut c_char,
    pub param_count: jint,
    pub params: *mut jvmtiParamInfo,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiTimerInfo {
    pub max_value: jlong,
    pub may_skip_forward: jboolean,
    pub may_skip_backward: jboolean,
    pub kind: jvmtiTimerKind,
    pub reserved1: jlong,
    pub reserved2: jlong,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct jvmtiAddrLocationMap {
    pub start_address: *const c_void,
    pub location: jlocation,
}

pub type jvmtiStartFunction =
    Option<unsafe extern "C" fn(jvmti_env: *mut jvmtiEnv, jni_env: *mut JNIEnv, arg: *mut c_void)>;
pub type jvmtiHeapObjectCallback = Option<
    unsafe extern "C" fn(class_tag: jlong, size: jlong, tag_ptr: *mut jlong, user_data: *mut c_void) -> u32,
>;
pub type jvmtiHeapRootCallback = *const c_void;
pub type jvmtiStackReferenceCallback = *const c_void;
pub type jvmtiObjectReferenceCallback = *const c_void;

/// `jvmtiCapabilities`: 128 one-bit fields packed into four `u32`s (LSB-first on all supported
/// targets). Use the `CAN_*` bit indices with [`jvmtiCapabilities::set`]/[`get`](Self::get).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct jvmtiCapabilities {
    pub bits: [u32; 4],
}

impl jvmtiCapabilities {
    pub fn set(&mut self, bit: u32, on: bool) {
        let (w, b) = ((bit / 32) as usize, bit % 32);
        if on {
            self.bits[w] |= 1 << b;
        } else {
            self.bits[w] &= !(1 << b);
        }
    }

    pub fn get(&self, bit: u32) -> bool {
        let (w, b) = ((bit / 32) as usize, bit % 32);
        self.bits[w] & (1 << b) != 0
    }

    pub fn with(mut self, bits: &[u32]) -> Self {
        for &b in bits {
            self.set(b, true);
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.bits == [0; 4]
    }
}

pub const CAN_TAG_OBJECTS: u32 = 0;
pub const CAN_GENERATE_FIELD_MODIFICATION_EVENTS: u32 = 1;
pub const CAN_GENERATE_FIELD_ACCESS_EVENTS: u32 = 2;
pub const CAN_GET_BYTECODES: u32 = 3;
pub const CAN_GET_SYNTHETIC_ATTRIBUTE: u32 = 4;
pub const CAN_GET_OWNED_MONITOR_INFO: u32 = 5;
pub const CAN_GET_CURRENT_CONTENDED_MONITOR: u32 = 6;
pub const CAN_GET_MONITOR_INFO: u32 = 7;
pub const CAN_POP_FRAME: u32 = 8;
pub const CAN_REDEFINE_CLASSES: u32 = 9;
pub const CAN_SIGNAL_THREAD: u32 = 10;
pub const CAN_GET_SOURCE_FILE_NAME: u32 = 11;
pub const CAN_GET_LINE_NUMBERS: u32 = 12;
pub const CAN_GET_SOURCE_DEBUG_EXTENSION: u32 = 13;
pub const CAN_ACCESS_LOCAL_VARIABLES: u32 = 14;
pub const CAN_MAINTAIN_ORIGINAL_METHOD_ORDER: u32 = 15;
pub const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 16;
pub const CAN_GENERATE_EXCEPTION_EVENTS: u32 = 17;
pub const CAN_GENERATE_FRAME_POP_EVENTS: u32 = 18;
pub const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 19;
pub const CAN_SUSPEND: u32 = 20;
pub const CAN_REDEFINE_ANY_CLASS: u32 = 21;
pub const CAN_GET_CURRENT_THREAD_CPU_TIME: u32 = 22;
pub const CAN_GET_THREAD_CPU_TIME: u32 = 23;
pub const CAN_GENERATE_METHOD_ENTRY_EVENTS: u32 = 24;
pub const CAN_GENERATE_METHOD_EXIT_EVENTS: u32 = 25;
pub const CAN_GENERATE_ALL_CLASS_HOOK_EVENTS: u32 = 26;
pub const CAN_GENERATE_COMPILED_METHOD_LOAD_EVENTS: u32 = 27;
pub const CAN_GENERATE_MONITOR_EVENTS: u32 = 28;
pub const CAN_GENERATE_VM_OBJECT_ALLOC_EVENTS: u32 = 29;
pub const CAN_GENERATE_NATIVE_METHOD_BIND_EVENTS: u32 = 30;
pub const CAN_GENERATE_GARBAGE_COLLECTION_EVENTS: u32 = 31;
pub const CAN_GENERATE_OBJECT_FREE_EVENTS: u32 = 32;
pub const CAN_FORCE_EARLY_RETURN: u32 = 33;
pub const CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO: u32 = 34;
pub const CAN_GET_CONSTANT_POOL: u32 = 35;
pub const CAN_SET_NATIVE_METHOD_PREFIX: u32 = 36;
pub const CAN_RETRANSFORM_CLASSES: u32 = 37;
pub const CAN_RETRANSFORM_ANY_CLASS: u32 = 38;
pub const CAN_GENERATE_RESOURCE_EXHAUSTION_HEAP_EVENTS: u32 = 39;
pub const CAN_GENERATE_RESOURCE_EXHAUSTION_THREADS_EVENTS: u32 = 40;
pub const CAN_GENERATE_EARLY_VMSTART: u32 = 41;
pub const CAN_GENERATE_EARLY_CLASS_HOOK_EVENTS: u32 = 42;
pub const CAN_GENERATE_SAMPLED_OBJECT_ALLOC_EVENTS: u32 = 43;
pub const CAN_SUPPORT_VIRTUAL_THREADS: u32 = 44;

/// Placeholder for callbacks the agent never installs (same size and alignment as a function
/// pointer).
pub type jvmtiEventReserved = Option<unsafe extern "C" fn()>;

pub type jvmtiEventVMInit = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread)>;
pub type jvmtiEventVMDeath = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv)>;
pub type jvmtiEventVMStart = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv)>;
pub type jvmtiEventThreadStart = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread)>;
pub type jvmtiEventThreadEnd = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread)>;
pub type jvmtiEventClassFileLoadHook = Option<
    unsafe extern "C" fn(
        *mut jvmtiEnv,
        *mut JNIEnv,
        jclass,
        jobject,
        *const c_char,
        jobject,
        jint,
        *const u8,
        *mut jint,
        *mut *mut u8,
    ),
>;
pub type jvmtiEventClassLoad = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jclass)>;
pub type jvmtiEventClassPrepare = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jclass)>;
pub type jvmtiEventException = Option<
    unsafe extern "C" fn(
        *mut jvmtiEnv,
        *mut JNIEnv,
        jthread,
        jmethodID,
        jlocation,
        jobject,
        jmethodID,
        jlocation,
    ),
>;
pub type jvmtiEventNativeMethodBind = Option<
    unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jmethodID, *mut c_void, *mut *mut c_void),
>;
pub type jvmtiEventCompiledMethodLoad = Option<
    unsafe extern "C" fn(
        *mut jvmtiEnv,
        jmethodID,
        jint,
        *const c_void,
        jint,
        *const jvmtiAddrLocationMap,
        *const c_void,
    ),
>;
pub type jvmtiEventCompiledMethodUnload =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, jmethodID, *const c_void)>;
pub type jvmtiEventDynamicCodeGenerated =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *const c_char, *const c_void, jint)>;
pub type jvmtiEventDataDumpRequest = Option<unsafe extern "C" fn(*mut jvmtiEnv)>;
pub type jvmtiEventMonitorWait =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject, jlong)>;
pub type jvmtiEventMonitorWaited =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject, jboolean)>;
pub type jvmtiEventMonitorContendedEnter =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject)>;
pub type jvmtiEventMonitorContendedEntered =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject)>;
pub type jvmtiEventResourceExhausted =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jint, *const c_void, *const c_char)>;
pub type jvmtiEventGarbageCollectionStart = Option<unsafe extern "C" fn(*mut jvmtiEnv)>;
pub type jvmtiEventGarbageCollectionFinish = Option<unsafe extern "C" fn(*mut jvmtiEnv)>;
pub type jvmtiEventObjectFree = Option<unsafe extern "C" fn(*mut jvmtiEnv, jlong)>;
pub type jvmtiEventVMObjectAlloc =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject, jclass, jlong)>;
pub type jvmtiEventSampledObjectAlloc =
    Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread, jobject, jclass, jlong)>;
pub type jvmtiEventVirtualThreadStart = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread)>;
pub type jvmtiEventVirtualThreadEnd = Option<unsafe extern "C" fn(*mut jvmtiEnv, *mut JNIEnv, jthread)>;

/// Event callbacks, in event-number order starting at `JVMTI_EVENT_VM_INIT` (50).
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct jvmtiEventCallbacks {
    pub VMInit: jvmtiEventVMInit,
    pub VMDeath: jvmtiEventVMDeath,
    pub ThreadStart: jvmtiEventThreadStart,
    pub ThreadEnd: jvmtiEventThreadEnd,
    pub ClassFileLoadHook: jvmtiEventClassFileLoadHook,
    pub ClassLoad: jvmtiEventClassLoad,
    pub ClassPrepare: jvmtiEventClassPrepare,
    pub VMStart: jvmtiEventVMStart,
    pub Exception: jvmtiEventException,
    pub ExceptionCatch: jvmtiEventReserved,
    pub SingleStep: jvmtiEventReserved,
    pub FramePop: jvmtiEventReserved,
    pub Breakpoint: jvmtiEventReserved,
    pub FieldAccess: jvmtiEventReserved,
    pub FieldModification: jvmtiEventReserved,
    pub MethodEntry: jvmtiEventReserved,
    pub MethodExit: jvmtiEventReserved,
    pub NativeMethodBind: jvmtiEventNativeMethodBind,
    pub CompiledMethodLoad: jvmtiEventCompiledMethodLoad,
    pub CompiledMethodUnload: jvmtiEventCompiledMethodUnload,
    pub DynamicCodeGenerated: jvmtiEventDynamicCodeGenerated,
    pub DataDumpRequest: jvmtiEventDataDumpRequest,
    pub reserved72: jvmtiEventReserved,
    pub MonitorWait: jvmtiEventMonitorWait,
    pub MonitorWaited: jvmtiEventMonitorWaited,
    pub MonitorContendedEnter: jvmtiEventMonitorContendedEnter,
    pub MonitorContendedEntered: jvmtiEventMonitorContendedEntered,
    pub reserved77: jvmtiEventReserved,
    pub reserved78: jvmtiEventReserved,
    pub reserved79: jvmtiEventReserved,
    pub ResourceExhausted: jvmtiEventResourceExhausted,
    pub GarbageCollectionStart: jvmtiEventGarbageCollectionStart,
    pub GarbageCollectionFinish: jvmtiEventGarbageCollectionFinish,
    pub ObjectFree: jvmtiEventObjectFree,
    pub VMObjectAlloc: jvmtiEventVMObjectAlloc,
    pub reserved85: jvmtiEventReserved,
    pub SampledObjectAlloc: jvmtiEventSampledObjectAlloc,
    pub VirtualThreadStart: jvmtiEventVirtualThreadStart,
    pub VirtualThreadEnd: jvmtiEventVirtualThreadEnd,
}

/// `AsyncGetCallTrace` (HotSpot-internal, resolved with `dlsym`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ASGCT_CallFrame {
    /// Line number / BCI; negative values encode native or unknown frames.
    pub lineno: jint,
    pub method_id: jmethodID,
}

#[repr(C)]
#[derive(Debug)]
pub struct ASGCT_CallTrace {
    pub env: *mut JNIEnv,
    /// Frame count, or a negative error code (see `ASGCT_*`).
    pub num_frames: jint,
    pub frames: *mut ASGCT_CallFrame,
}

pub type AsyncGetCallTraceFn =
    unsafe extern "C" fn(trace: *mut ASGCT_CallTrace, depth: jint, ucontext: *mut c_void);

pub const ASGCT_NO_JAVA_FRAME: jint = 0;
pub const ASGCT_NO_CLASS_LOAD: jint = -1;
pub const ASGCT_GC_ACTIVE: jint = -2;
pub const ASGCT_UNKNOWN_NOT_JAVA: jint = -3;
pub const ASGCT_NOT_WALKABLE_NOT_JAVA: jint = -4;
pub const ASGCT_UNKNOWN_JAVA: jint = -5;
pub const ASGCT_NOT_WALKABLE_JAVA: jint = -6;
pub const ASGCT_UNKNOWN_STATE: jint = -7;
pub const ASGCT_THREAD_EXIT: jint = -8;
pub const ASGCT_DEOPT: jint = -9;
pub const ASGCT_SAFEPOINT: jint = -10;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn layouts() {
        assert_eq!(size_of::<jvmtiCapabilities>(), 16);
        // 39 callback slots (events 50..=88).
        assert_eq!(size_of::<jvmtiEventCallbacks>(), 39 * size_of::<usize>());
        // 156 function slots in jvmtiInterface_1_ (JDK 21+).
        assert_eq!(size_of::<jvmtiInterface_1_>(), 156 * size_of::<usize>());
        let c = jvmtiCapabilities::default().with(&[CAN_TAG_OBJECTS, CAN_SUPPORT_VIRTUAL_THREADS]);
        assert!(c.get(CAN_SUPPORT_VIRTUAL_THREADS) && c.bits[1] == 1 << 12);
    }
}
