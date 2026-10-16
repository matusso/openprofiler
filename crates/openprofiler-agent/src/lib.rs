//! `libopenprofiler_agent`: OpenProfiler's native JVMTI agent.
//!
//! Lifecycle:
//! 1. `Agent_OnLoad` / `Agent_OnAttach` parse options, obtain a JVMTI environment, request
//!    capabilities in independent groups, install event callbacks and put the embedded
//!    `openprofiler-boot.jar` on the boot class path.
//! 2. When the VM is live (VMInit, or immediately on attach) the agent binds the boot classes'
//!    natives, starts the Java telemetry sampler, the samplers and — in live mode — the agent-link
//!    server.
//! 3. At VM death an offline recording is written (offline mode) and the info file is removed.

#![allow(clippy::missing_safety_doc)]

mod collector;
mod events;
mod java;
mod jvm;
mod options;
mod park;
mod recording;
mod resolve;
mod sampler;
#[cfg(target_os = "linux")]
mod sampler_async;
mod server;
mod tls;
mod triggers;
#[cfg(unix)]
mod vmstructs;
mod vthreads;

pub use collector::Collector;
pub use options::{Mode, Options, SamplerChoice};

use jvm::{Jvmti, Vm};
use jvmti_sys::*;
use parking_lot::Mutex;
use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Boot classes compiled from `java/openprofiler-boot` at build time (empty if no JDK was available).
/// Format: repeated `u32 name_len | internal name | u32 len | class bytes`.
pub(crate) static BOOT_CLASSES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/openprofiler-boot.bin"));

/// Process-wide agent state.
pub struct Agent {
    pub jvmti: Jvmti,
    pub vm: Vm,
    pub opts: Options,
    pub collector: Mutex<Collector>,
    /// Granted feature groups (`cpu`, `alloc`, `monitor`, `tagging`, `line_numbers`, ...).
    pub caps: Mutex<Vec<String>>,
    pub cpu_on: AtomicBool,
    pub alloc_on: AtomicBool,
    pub monitor_on: AtomicBool,
    pub interval_ns: AtomicU64,
    pub live_started: AtomicBool,
    pub shutting_down: AtomicBool,
    pub java: OnceLock<java::JavaHelpers>,
    pub jvm_info: Mutex<openprofiler_proto::v1::JvmInfo>,
    pub instance_id: u64,
    pub token: String,
    pub tls: Option<tls::TlsIdentity>,
    pub info_file: Mutex<Option<std::path::PathBuf>>,
    pub connected: (std::sync::Mutex<bool>, std::sync::Condvar),
    pub async_active: AtomicBool,
}

static AGENT: OnceLock<Agent> = OnceLock::new();

pub fn agent() -> Option<&'static Agent> {
    AGENT.get()
}

pub(crate) fn log(msg: &str) {
    eprintln!("[openprofiler] {msg}");
}

/// Marks the calling thread as one of the agent's own (never CPU-sampled).
pub(crate) fn own_thread() {
    #[cfg(target_os = "linux")]
    sampler_async::register_own_thread();
}

pub(crate) fn debug(msg: &str) {
    if agent().is_some_and(|a| a.opts.debug) {
        log(msg);
    }
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b)
}

pub(crate) fn random_token() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    hex::encode(b)
}

/// Capability groups requested independently so a refused group only disables its feature.
const CAP_GROUPS: &[(&str, &[u32])] = &[
    ("tagging", &[CAN_TAG_OBJECTS]),
    ("line_numbers", &[CAN_GET_LINE_NUMBERS, CAN_GET_SOURCE_FILE_NAME]),
    ("monitor", &[CAN_GENERATE_MONITOR_EVENTS]),
    ("alloc", &[CAN_GENERATE_SAMPLED_OBJECT_ALLOC_EVENTS]),
    ("thread_cpu", &[CAN_GET_THREAD_CPU_TIME]),
    ("monitor_info", &[CAN_GET_OWNED_MONITOR_INFO, CAN_GET_CURRENT_CONTENDED_MONITOR]),
    ("monitor_usage", &[CAN_GET_MONITOR_INFO]),
    ("oom", &[CAN_GENERATE_RESOURCE_EXHAUSTION_HEAP_EVENTS, CAN_GENERATE_RESOURCE_EXHAUSTION_THREADS_EVENTS]),
    ("virtual_threads", &[CAN_SUPPORT_VIRTUAL_THREADS]),
];

fn request_capabilities(jvmti: Jvmti, opts: &Options) -> Vec<String> {
    let potential = jvmti.potential_capabilities();
    let mut granted = vec![];
    for (name, bits) in CAP_GROUPS {
        let wanted = match *name {
            "virtual_threads" => opts.vthreads,
            "monitor_usage" => opts.owner_rate > 0,
            _ => true,
        };
        if !wanted || !bits.iter().all(|&b| potential.get(b)) {
            continue;
        }
        let caps = jvmtiCapabilities::default().with(bits);
        if jvmti.add_capabilities(&caps).is_ok() {
            granted.push((*name).to_string());
        }
    }
    granted
}

/// Shared initialization for `Agent_OnLoad` and `Agent_OnAttach`.
unsafe fn init(vm: *mut JavaVM, options: *const c_char, attach: bool) -> jint {
    let args = if options.is_null() {
        String::new()
    } else {
        // SAFETY: the VM passes a NUL-terminated option string.
        unsafe { CStr::from_ptr(options) }.to_string_lossy().into_owned()
    };
    if let Some(a) = AGENT.get() {
        log("agent already loaded; ignoring second load");
        if attach && a.live_started.load(Ordering::SeqCst) {
            return 0;
        }
        return 0;
    }
    let env_opts = std::env::var("OPENPROFILER_AGENT_OPTIONS").ok();
    let (opts, warnings) = Options::parse(&args, env_opts.as_deref());
    for w in warnings {
        log(&w);
    }
    let vm = Vm(vm);
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid JavaVM pointer from the VM.
    let r = unsafe { ((**vm.0).GetEnv.expect("GetEnv"))(vm.0, &mut env, JVMTI_VERSION_1_2) };
    if r != JNI_OK || env.is_null() {
        log("JVMTI 1.2+ not available");
        return 1;
    }
    let jvmti = Jvmti(env as *mut jvmtiEnv);
    let mut caps = request_capabilities(jvmti, &opts);
    caps.push("cpu".into());
    caps.push("threads".into());
    caps.push("telemetry".into());
    caps.push("thread_dump".into());
    caps.push("heap_dump".into());
    caps.push("triggers".into());
    caps.push("mbeans".into());

    let tls = if opts.tls && opts.mode == Mode::Live {
        match tls::TlsIdentity::generate() {
            Ok(t) => Some(t),
            Err(e) => {
                log(&format!("TLS setup failed ({e}); continuing without TLS"));
                None
            }
        }
    } else {
        None
    };
    let token = opts.token.clone().unwrap_or_else(random_token);
    let has_monitor = caps.iter().any(|c| c == "monitor");
    let has_alloc = caps.iter().any(|c| c == "alloc");
    let mut collector = Collector::new(opts.mode == Mode::Live);
    collector.resolver.line_numbers = caps.iter().any(|c| c == "line_numbers");
    let agent = Agent {
        jvmti,
        vm,
        cpu_on: AtomicBool::new(opts.cpu),
        alloc_on: AtomicBool::new(opts.alloc && has_alloc),
        monitor_on: AtomicBool::new(opts.monitors && has_monitor),
        interval_ns: AtomicU64::new(opts.interval.as_nanos() as u64),
        opts,
        collector: Mutex::new(collector),
        caps: Mutex::new(caps),
        live_started: AtomicBool::new(false),
        shutting_down: AtomicBool::new(false),
        java: OnceLock::new(),
        jvm_info: Mutex::new(Default::default()),
        instance_id: random_u64(),
        token,
        tls,
        info_file: Mutex::new(None),
        connected: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
        async_active: AtomicBool::new(false),
    };
    if AGENT.set(agent).is_err() {
        return 0;
    }
    let agent = AGENT.get().unwrap();

    // An explicit boot jar goes on the boot class path; otherwise the embedded classes are
    // defined into the bootstrap loader once the VM is live (avoids disabling CDS for app classes).
    if let Some(p) = &agent.opts.bootjar
        && let Err(e) = jvmti.add_to_boot_classpath(&p.display().to_string())
    {
        log(&format!("cannot add boot jar {}: {e}", p.display()));
    }

    if let Err(e) = events::install(agent) {
        log(&format!("cannot install event callbacks: {e}"));
        return 1;
    }

    if attach || jvmti.phase() == JVMTI_PHASE_LIVE {
        match vm.env() {
            Some(jni) => events::on_live(agent, jni),
            None => log("attach: no JNIEnv on the attach thread"),
        }
    }
    0
}

/// Called by the VM for `-agentpath:` / `-agentlib:`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Agent_OnLoad(
    vm: *mut JavaVM,
    options: *mut c_char,
    _reserved: *mut c_void,
) -> jint {
    // SAFETY: arguments come straight from the VM.
    unsafe { init(vm, options, false) }
}

/// Called by the VM for dynamic attach (`VirtualMachine.loadAgentPath`, `openprofiler attach`).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Agent_OnAttach(
    vm: *mut JavaVM,
    options: *mut c_char,
    _reserved: *mut c_void,
) -> jint {
    // SAFETY: arguments come straight from the VM.
    unsafe { init(vm, options, true) }
}

#[unsafe(no_mangle)]
pub extern "system" fn Agent_OnUnload(_vm: *mut JavaVM) {
    if let Some(a) = agent() {
        a.shutting_down.store(true, Ordering::SeqCst);
    }
}
