//! Agent option parsing.
//!
//! Options come from `-agentpath:libopenprofiler_agent.so=<opts>`, the attach argument, and the
//! `OPENPROFILER_AGENT_OPTIONS` environment variable (applied first, so explicit options win). The syntax
//! is `key=value` pairs separated by commas; bare keys are booleans.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Serve the agent-link protocol.
    Live,
    /// No listener; write a recording file at exit (`file=`).
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerChoice {
    Auto,
    Async,
    Sync,
    /// Stream JFR execution samples (no signals, no `AsyncGetCallTrace`).
    Jfr,
}

/// JFR event the `jfr` sampler streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JfrEvent {
    /// `jdk.CPUTimeSample` where the JVM has it (JDK 25+ on Linux), else `jdk.ExecutionSample`.
    Auto,
    /// `jdk.ExecutionSample`: threads running Java code, at a fixed wall-clock period.
    Execution,
    /// `jdk.CPUTimeSample`: one sample per interval of CPU time a thread consumed.
    CpuTime,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub port: u16,
    pub bind: String,
    pub token: Option<String>,
    pub mode: Mode,
    pub file: Option<PathBuf>,
    pub interval: Duration,
    /// Interval of wall-clock / thread-state sampling (sync sampler).
    pub wall_interval: Duration,
    /// Block VM startup until a daemon connected (`wait`); default is not to block (`nowait`).
    pub wait: bool,
    pub tls: bool,
    pub sampler: SamplerChoice,
    pub jfr_event: JfrEvent,
    /// Repair and retry failed `AsyncGetCallTrace` walks (Linux async sampler).
    pub recover: bool,
    pub cpu: bool,
    pub alloc: bool,
    pub alloc_interval: u32,
    pub monitors: bool,
    pub monitor_threshold: Duration,
    /// Minimum time between full stack captures for probe calls of one (thread, probe, request,
    /// payload); calls in between reuse the last stack. Zero: every call.
    pub probe_stack_interval: Duration,
    /// Blocked or waiting time of shorter monitor events per thread that yields one sample.
    pub monitor_interval: Duration,
    /// Owner lookups for contended monitors per second (each is a safepoint); 0 disables them.
    pub owner_rate: u32,
    /// Virtual-thread mount/unmount tracking (JDK 21+).
    pub vthreads: bool,
    /// JSON trigger file loaded at start-up.
    pub triggers: Option<PathBuf>,
    pub max_frames: u32,
    pub bootjar: Option<PathBuf>,
    /// Where to publish `{pid, port, token, fingerprint}`; default `$TMPDIR/openprofiler-<pid>.json`.
    pub infofile: Option<PathBuf>,
    pub debug: bool,
    /// Bytecode instrumentation (probes, call counts, method triggers); `noinstrument` turns it off.
    pub instrumentation: bool,
    /// Class patterns (`com.acme.*`) whose methods get exact call counts; `:`-separated.
    pub instrument: Vec<String>,
    pub instrument_exclude: Vec<String>,
    /// Enabled probe ids (`jdbc:http_server`); `None` = all, empty = none.
    pub probes: Option<Vec<String>>,
    /// Remove hot, trivial methods from call counting.
    pub autotune: bool,
    /// Individual probe events recorded per probe and second (slower calls are always kept).
    pub probe_events: u32,
    /// Probe calls faster than this are aggregated but never recorded as events.
    pub probe_threshold: Duration,
    /// Methods recorded for complexity analysis (`com.acme.Sorter.sort#0`); `:`-separated.
    pub complexity: Vec<String>,
    /// Complexity points kept per method and second.
    pub complexity_rate: u32,
    /// Run the call tracer for this long once the agent is live.
    pub trace: Option<Duration>,
    /// Event cap of call traces started without one.
    pub trace_events: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            mode: Mode::Live,
            file: None,
            interval: Duration::from_millis(10),
            wall_interval: Duration::from_millis(50),
            wait: false,
            tls: false,
            sampler: SamplerChoice::Auto,
            jfr_event: JfrEvent::Auto,
            recover: true,
            cpu: true,
            alloc: true,
            alloc_interval: 512 * 1024,
            monitors: true,
            monitor_threshold: Duration::from_micros(100),
            monitor_interval: Duration::from_millis(1),
            probe_stack_interval: Duration::from_millis(10),
            owner_rate: 50,
            vthreads: true,
            triggers: None,
            max_frames: 256,
            bootjar: None,
            infofile: None,
            debug: false,
            instrumentation: true,
            instrument: vec![],
            instrument_exclude: vec![],
            probes: None,
            autotune: true,
            probe_events: 100,
            probe_threshold: Duration::ZERO,
            complexity: vec![],
            complexity_rate: 100,
            trace: None,
            trace_events: crate::instrument::calls::TRACE_EVENTS,
        }
    }
}

/// Parses `10ms`, `250us`, `1s`, `5000ns`; a bare number is milliseconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "ms"),
    };
    let v: f64 = num.parse().ok()?;
    let nanos = match unit {
        "ns" => v,
        "us" => v * 1e3,
        "ms" => v * 1e6,
        "s" => v * 1e9,
        _ => return None,
    };
    Some(Duration::from_nanos(nanos as u64))
}

fn parse_size(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(n) = s.strip_suffix('k') {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1024 * 1024)
    } else {
        (s.as_str(), 1)
    };
    num.parse::<u32>().ok().map(|n| n.saturating_mul(mul))
}

/// A `:`-separated list (commas separate options).
fn list(v: &str) -> Vec<String> {
    v.split(':').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn flag(v: &str) -> bool {
    !matches!(v, "0" | "false" | "no" | "off")
}

impl Options {
    /// Parses the env var first and then `args`. Unknown keys produce warnings, not errors.
    pub fn parse(args: &str, env: Option<&str>) -> (Self, Vec<String>) {
        let mut o = Options::default();
        let mut warnings = vec![];
        for src in [env.unwrap_or(""), args] {
            for part in src.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let (k, v) = part.split_once('=').unwrap_or((part, "1"));
                let ok = match k {
                    "port" => v.parse().map(|p| o.port = p).is_ok(),
                    "bind" | "address" => {
                        o.bind = v.into();
                        true
                    }
                    "loopback" => {
                        o.bind = "127.0.0.1".into();
                        true
                    }
                    "token" => {
                        o.token = Some(v.into());
                        true
                    }
                    "mode" => match v {
                        "live" => {
                            o.mode = Mode::Live;
                            true
                        }
                        "offline" => {
                            o.mode = Mode::Offline;
                            true
                        }
                        _ => false,
                    },
                    "offline" => {
                        o.mode = Mode::Offline;
                        true
                    }
                    "file" => {
                        o.file = Some(v.into());
                        true
                    }
                    "interval" => {
                        parse_duration(v).map(|d| o.interval = d.max(Duration::from_micros(100))).is_some()
                    }
                    "wall" | "wall_interval" => {
                        parse_duration(v).map(|d| o.wall_interval = d.max(Duration::from_millis(1))).is_some()
                    }
                    "wait" => {
                        o.wait = flag(v);
                        true
                    }
                    "nowait" => {
                        o.wait = !flag(v);
                        true
                    }
                    "tls" => {
                        o.tls = flag(v);
                        true
                    }
                    "sampler" => match v {
                        "auto" => {
                            o.sampler = SamplerChoice::Auto;
                            true
                        }
                        "async" => {
                            o.sampler = SamplerChoice::Async;
                            true
                        }
                        "sync" => {
                            o.sampler = SamplerChoice::Sync;
                            true
                        }
                        "jfr" => {
                            o.sampler = SamplerChoice::Jfr;
                            true
                        }
                        _ => false,
                    },
                    "jfr_event" => match v {
                        "auto" => {
                            o.jfr_event = JfrEvent::Auto;
                            true
                        }
                        "execution" => {
                            o.jfr_event = JfrEvent::Execution;
                            true
                        }
                        "cputime" => {
                            o.jfr_event = JfrEvent::CpuTime;
                            true
                        }
                        _ => false,
                    },
                    "recover" => {
                        o.recover = flag(v);
                        true
                    }
                    "cpu" => {
                        o.cpu = flag(v);
                        true
                    }
                    "alloc" => {
                        o.alloc = flag(v);
                        true
                    }
                    "alloc_interval" => parse_size(v).map(|n| o.alloc_interval = n.max(1)).is_some(),
                    "monitors" => {
                        o.monitors = flag(v);
                        true
                    }
                    "monitor_threshold" => parse_duration(v).map(|d| o.monitor_threshold = d).is_some(),
                    "monitor_interval" => parse_duration(v).map(|d| o.monitor_interval = d).is_some(),
                    "probe_stack_interval" => parse_duration(v).map(|d| o.probe_stack_interval = d).is_some(),
                    "owner_rate" => v.parse().map(|n| o.owner_rate = n).is_ok(),
                    "vthreads" => {
                        o.vthreads = flag(v);
                        true
                    }
                    "triggers" => {
                        o.triggers = Some(v.into());
                        true
                    }
                    "max_frames" | "depth" => v.parse().map(|n: u32| o.max_frames = n.clamp(1, 2048)).is_ok(),
                    "bootjar" => {
                        o.bootjar = Some(v.into());
                        true
                    }
                    "infofile" => {
                        o.infofile = Some(v.into());
                        true
                    }
                    "debug" => {
                        o.debug = flag(v);
                        true
                    }
                    "instrumentation" => {
                        o.instrumentation = flag(v);
                        true
                    }
                    "noinstrument" => {
                        o.instrumentation = !flag(v);
                        true
                    }
                    "instrument" | "include" => {
                        o.instrument = list(v);
                        true
                    }
                    "exclude" => {
                        o.instrument_exclude = list(v);
                        true
                    }
                    "probes" => {
                        o.probes = match v {
                            "all" | "1" => None,
                            "none" | "0" | "off" => Some(vec![]),
                            _ => Some(list(v)),
                        };
                        true
                    }
                    "autotune" => {
                        o.autotune = flag(v);
                        true
                    }
                    "probe_events" => v.parse().map(|n| o.probe_events = n).is_ok(),
                    "probe_threshold" => parse_duration(v).map(|d| o.probe_threshold = d).is_some(),
                    "complexity" => {
                        o.complexity = list(v);
                        true
                    }
                    "complexity_rate" => v.parse().map(|n| o.complexity_rate = n).is_ok(),
                    "trace" => parse_duration(v).map(|d| o.trace = Some(d)).is_some(),
                    "trace_events" => v.parse().map(|n| o.trace_events = n).is_ok(),
                    _ => false,
                };
                if !ok {
                    warnings.push(format!("ignoring option `{part}`"));
                }
            }
        }
        (o, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options() {
        let (o, w) = Options::parse(
            "port=9000,token=abc,interval=5ms,tls,mode=offline,file=/tmp/x",
            Some("bind=0.0.0.0"),
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(o.port, 9000);
        assert_eq!(o.bind, "0.0.0.0");
        assert_eq!(o.token.as_deref(), Some("abc"));
        assert_eq!(o.interval, Duration::from_millis(5));
        assert!(o.tls);
        assert_eq!(o.mode, Mode::Offline);
        let (o, w) =
            Options::parse("bogus,alloc_interval=1m,nowait,owner_rate=0,vthreads=off,recover=0", None);
        assert_eq!(w.len(), 1);
        assert_eq!(o.alloc_interval, 1 << 20);
        assert!(!o.wait);
        assert_eq!(o.owner_rate, 0);
        assert!(!o.vthreads && !o.recover);
        assert_eq!(parse_duration("250us"), Some(Duration::from_micros(250)));
        let (o, w) = Options::parse(
            "instrument=com.acme.*:org.x.Y,exclude=com.acme.gen.*,probes=jdbc:http_server",
            None,
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(o.instrument, vec!["com.acme.*", "org.x.Y"]);
        assert_eq!(o.instrument_exclude, vec!["com.acme.gen.*"]);
        assert_eq!(o.probes, Some(vec!["jdbc".to_string(), "http_server".to_string()]));
        assert_eq!(Options::parse("probes=none", None).0.probes, Some(vec![]));
        let (o, bad) = Options::parse("complexity=a.B.sort#0:a.C.run,trace=5s,trace_events=1000", None);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(o.complexity, vec!["a.B.sort#0", "a.C.run"]);
        assert_eq!((o.trace, o.trace_events), (Some(Duration::from_secs(5)), 1000));
        let (o, bad) = Options::parse("sampler=jfr,jfr_event=execution", None);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!((o.sampler, o.jfr_event), (SamplerChoice::Jfr, JfrEvent::Execution));
        assert_eq!(Options::parse("jfr_event=bogus", None).1.len(), 1);
    }
}
