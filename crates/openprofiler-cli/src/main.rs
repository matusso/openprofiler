//! `openprofiler`: open, export, compare and analyze profiles; run the daemon and the MCP server.

mod gate;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use openprofiler_daemon::export::{ExportRequest, Format};
use openprofiler_daemon::query::*;
use openprofiler_daemon::{DaemonConfig, Engine};
use openprofiler_model::SampleKind;
use openprofiler_model::views::Aggregation;
use rayon::prelude::*;
use serde_json::json;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "openprofiler", version, about = "OpenProfiler: an open-source JVM profiler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the query server (desktop/web UI backend).
    Daemon {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// API token (random if omitted).
        #[arg(long, env = "OPENPROFILER_TOKEN")]
        token: Option<String>,
        /// Serve the web UI.
        #[arg(long)]
        web: bool,
        /// Directory containing the built UI (default: `ui/` next to the executable, else ./ui/dist).
        #[arg(long)]
        ui_dir: Option<PathBuf>,
        /// Add the synthetic demo session.
        #[arg(long)]
        demo: bool,
        /// Files to open at startup.
        files: Vec<PathBuf>,
    },
    /// Print a summary of a profile or heap dump.
    Open { file: PathBuf },
    /// Show hot spots of a profile.
    Hotspots {
        file: PathBuf,
        #[arg(long, default_value = "cpu")]
        kind: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, value_enum, default_value = "method")]
        aggregation: AggArg,
    },
    /// Export a view to CSV/JSON/HTML/SVG/collapsed stacks.
    Export {
        file: PathBuf,
        /// calltree, backtraces, hotspots, flame, telemetry, threads, insights, classes, biggest
        #[arg(long, default_value = "hotspots")]
        view: String,
        /// csv, json, html, svg, collapsed
        #[arg(long, default_value = "csv")]
        format: String,
        #[arg(long, default_value = "cpu")]
        kind: String,
        #[arg(long, value_enum, default_value = "method")]
        aggregation: AggArg,
        /// Extra request JSON merged into the view request.
        #[arg(long)]
        request: Option<String>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Compare two profiles; optionally fail (exit 1) when a regression gate trips.
    Compare {
        base: PathBuf,
        head: PathBuf,
        #[arg(long, default_value = "cpu")]
        kind: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Gate expressions such as `cpu.total > +10%` or `method:com.acme.Foo.bar > +5%`.
        #[arg(long = "fail-if")]
        fail_if: Vec<String>,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Precompute analysis (heap indexes, dominators) and print insights.
    Analyze { file: PathBuf },
    /// Convert a profile (.jfr, .hprof, .opsnap) to an .opsnap snapshot.
    Convert {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Write the synthetic demo profile as an .opsnap snapshot.
    Demo {
        #[arg(short, long, default_value = "demo.opsnap")]
        output: PathBuf,
    },
    /// Decompile an Android app (.apk, .dex, .xapk, .apks, .aab) or JVM bytecode (.jar, .war,
    /// .ear, .jmod, .class) to Java.
    ///
    /// Without CLASS, lists all classes; with --output, writes every class to a directory.
    Decompile {
        file: PathBuf,
        /// Write all classes (`.java`, or `.smali` / `.bytecode` with --smali) under this
        /// directory.
        #[arg(short, long, conflicts_with_all = ["class", "search"])]
        output: Option<PathBuf>,
        /// Fully qualified class name to print.
        class: Option<String>,
        /// Print the low-level view instead of Java: smali for Android code, JVM bytecode for
        /// class files.
        #[arg(long, visible_alias = "bytecode")]
        smali: bool,
        /// Search class, method and field names instead.
        #[arg(long, conflicts_with = "class")]
        search: Option<String>,
        /// With --search: search the decompiled source text (slower).
        #[arg(long, requires = "search")]
        code: bool,
    },
    /// Run the MCP server on stdio.
    Mcp,
    /// List local JVMs.
    List,
    /// Attach to a running JVM, sample for a while and print hot spots.
    Attach {
        pid: u32,
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        duration: std::time::Duration,
        #[arg(long, default_value = "cpu")]
        kind: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// JSON trigger file to install in the agent (see docs/spec/agent-link.md).
        #[arg(long)]
        triggers: Option<PathBuf>,
    },
    /// Attach to a running JVM, record for a duration and write an .opsnap snapshot.
    Record {
        #[arg(long)]
        pid: u32,
        #[arg(long, default_value = "60s", value_parser = parse_duration)]
        duration: std::time::Duration,
        /// CPU sampling interval, e.g. 10ms.
        #[arg(long)]
        interval: Option<String>,
        /// Also embed a heap dump taken at the end.
        #[arg(long)]
        heap: bool,
        /// JSON trigger file to install in the agent (see docs/spec/agent-link.md).
        #[arg(long)]
        triggers: Option<PathBuf>,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Write an HPROF heap dump of a running JVM.
    Dump {
        pid: u32,
        #[arg(short, long)]
        output: PathBuf,
        /// Include unreachable objects.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AggArg {
    Method,
    Class,
    Package,
}

impl From<AggArg> for Aggregation {
    fn from(a: AggArg) -> Self {
        match a {
            AggArg::Method => Aggregation::Method,
            AggArg::Class => Aggregation::Class,
            AggArg::Package => Aggregation::Package,
        }
    }
}

fn kind(s: &str) -> Result<SampleKind> {
    SampleKind::parse(s)
        .with_context(|| format!("unknown kind {s} (cpu, wall, alloc, monitor, wait, io, exception)"))
}

fn fmt_nanos(n: i64) -> String {
    let s = n / 1_000_000_000;
    format!("{}", s)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPENPROFILER_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Daemon { bind, port, token, web, ui_dir, demo, files } => {
            let ui_dir = ui_dir.or_else(|| web.then(default_ui_dir));
            if web && !ui_dir.as_ref().is_some_and(|d| d.join("index.html").exists()) {
                bail!("--web needs a built UI (run `npm run build` in ui/ or pass --ui-dir)");
            }
            let cfg = DaemonConfig { bind: format!("{bind}:{port}").parse()?, token, ui_dir, web };
            let d = openprofiler_daemon::start(cfg).await?;
            if demo {
                d.engine.add_profile(
                    openprofiler_daemon::SessionKind::Snapshot,
                    "Demo shop (synthetic)",
                    openprofiler_daemon::demo::demo_profile(),
                );
            }
            for f in files {
                d.engine.open(&f)?;
            }
            if let Err(e) = publish_discovery(&d) {
                tracing::warn!("could not write ~/.openprofiler/daemon.json: {e}");
            }
            println!("OpenProfiler daemon listening on {}", d.url());
            println!("API token: {}", d.token);
            if web {
                println!("Open {}/?token={}", d.url(), d.token);
            }
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Cmd::Mcp => openprofiler_daemon::mcp::McpServer::new(Engine::new()).serve_stdio().await,
        Cmd::Attach { pid, duration, kind: k, limit, triggers } => {
            let engine = Engine::new();
            let s = attach_live(&engine, pid, None).await?;
            install_triggers(&engine, &s, triggers.as_deref()).await?;
            eprintln!("Attached to {}; sampling for {duration:?}…", s.name);
            tokio::time::sleep(duration).await;
            let r = hotspots(
                &s,
                &HotSpotsRequest {
                    kind: kind(&k)?,
                    filter: Default::default(),
                    aggregation: Aggregation::Method,
                    compact_prefixes: vec![],
                    limit: Some(limit),
                },
            )?;
            println!("{:>7} {:>7}  method", "self%", "total%");
            for h in &r.hotspots {
                println!(
                    "{:>6.2}% {:>6.2}%  {}",
                    100.0 * h.self_ as f64 / r.total.max(1) as f64,
                    100.0 * h.total as f64 / r.total.max(1) as f64,
                    h.label
                );
            }
            engine.close(&s.id)?;
            Ok(())
        }
        Cmd::Record { pid, duration, interval, heap, triggers, output } => {
            let engine = Engine::new();
            let s = attach_live(&engine, pid, interval).await?;
            install_triggers(&engine, &s, triggers.as_deref()).await?;
            eprintln!("Recording {} for {duration:?}…", s.name);
            tokio::time::sleep(duration).await;
            let heap_path = if heap {
                eprintln!("Taking heap dump…");
                let r = openprofiler_daemon::live::control(
                    &engine,
                    &s,
                    "heap-dump",
                    json!({ "live_only": true }),
                )
                .await?;
                Some(PathBuf::from(r["path"].as_str().unwrap_or_default()))
            } else {
                None
            };
            let prof = s.require_profile()?;
            let out = output.clone();
            tokio::task::spawn_blocking(move || {
                let p = prof.read();
                openprofiler_snapshot::write_snapshot(&out, &p, heap_path.as_deref())
            })
            .await??;
            engine.close(&s.id)?;
            println!("Wrote {}", output.display());
            Ok(())
        }
        Cmd::Dump { pid, output, all } => {
            let out = std::path::absolute(&output)?;
            let arg = if all { "-all" } else { "" };
            let cmd = format!("GC.heap_dump {arg} {}", out.display());
            let text = tokio::task::spawn_blocking(move || openprofiler_attach::jcmd(pid, &cmd)).await??;
            print!("{text}");
            Ok(())
        }
        other => tokio::task::spawn_blocking(move || run_blocking(other)).await?,
    }
}

fn run_blocking(cmd: Cmd) -> Result<()> {
    let engine = Engine::new();
    match cmd {
        Cmd::Open { file } => {
            let s = engine.open_blocking(&file)?;
            let sum = summary(&s)?;
            println!("{} ({:?})", s.name, s.kind);
            if let Some(j) = &sum.jvm {
                if !j.vm_name.is_empty() {
                    println!("  JVM        {} {} ({})", j.vm_name, j.vm_version, j.vm_vendor);
                }
                if !j.main_class.is_empty() {
                    println!("  Main       {}", j.main_class);
                }
            }
            if sum.end > sum.start {
                println!("  Duration   {} s", fmt_nanos(sum.end - sum.start));
            }
            for (k, v) in &sum.sample_counts {
                println!("  {:<10} {} samples", k, v);
            }
            if sum.threads > 0 {
                println!("  Threads    {}", sum.threads);
            }
            if let Some(h) = &sum.heap {
                println!("  Heap       {}", serde_json::to_string(h)?);
            }
            if let Some(d) = s.decompiler() {
                let sum = d.program.summary();
                println!(
                    "  Code       {} classes in {} packages, {} resources, {} dex files",
                    sum["classes"],
                    sum["packages"],
                    sum["resources"],
                    sum["dex_files"].as_array().map_or(0, Vec::len)
                );
            }
            println!("  Capabilities: {}", s.capabilities().join(", "));
            Ok(())
        }
        Cmd::Hotspots { file, kind: k, limit, aggregation } => {
            let s = engine.open_blocking(&file)?;
            let r = hotspots(
                &s,
                &HotSpotsRequest {
                    kind: kind(&k)?,
                    filter: Default::default(),
                    aggregation: aggregation.into(),
                    compact_prefixes: vec![],
                    limit: Some(limit),
                },
            )?;
            println!("{:>7} {:>7}  method", "self%", "total%");
            for h in &r.hotspots {
                println!(
                    "{:>6.2}% {:>6.2}%  {}",
                    100.0 * h.self_ as f64 / r.total.max(1) as f64,
                    100.0 * h.total as f64 / r.total.max(1) as f64,
                    h.label
                );
            }
            Ok(())
        }
        Cmd::Export { file, view, format, kind: k, aggregation, request, output } => {
            let s = engine.open_blocking(&file)?;
            let f = Format::parse(&format).with_context(|| format!("unknown format {format}"))?;
            let agg = match Aggregation::from(aggregation) {
                Aggregation::Method => "method",
                Aggregation::Class => "class",
                Aggregation::Package => "package",
            };
            let mut req = json!({ "kind": kind(&k)?.name(), "aggregation": agg });
            if let Some(extra) = request {
                let extra: serde_json::Value =
                    serde_json::from_str(&extra).context("--request must be JSON")?;
                if let (Some(a), Some(b)) = (req.as_object_mut(), extra.as_object()) {
                    a.extend(b.clone());
                }
            }
            let out =
                openprofiler_daemon::export::export(&s, &ExportRequest { view, format: f, request: req })?;
            match output {
                Some(p) => {
                    std::fs::write(&p, &out.body).with_context(|| format!("writing {}", p.display()))?
                }
                None => {
                    use std::io::Write;
                    std::io::stdout().write_all(&out.body)?;
                }
            }
            Ok(())
        }
        Cmd::Compare { base, head, kind: k, limit, fail_if, json: as_json } => {
            let a = engine.open_blocking(&base)?;
            let b = engine.open_blocking(&head)?;
            let report = gate::compare_report(&a, &b, kind(&k)?, limit)?;
            let failures = gate::evaluate(&report, &fail_if)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "report": report, "failures": failures }))?
                );
            } else {
                gate::print_report(&report);
                for f in &failures {
                    eprintln!("GATE FAILED: {f}");
                }
            }
            if !failures.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Analyze { file } => {
            let started = std::time::Instant::now();
            let s = engine.open_blocking(&file)?;
            println!("Analyzed {} in {:.2?}", s.name, started.elapsed());
            for i in openprofiler_daemon::insights::session_insights(&s)? {
                println!("[{:?}] {} — {}", i.severity, i.title, i.summary);
            }
            Ok(())
        }
        Cmd::Convert { input, output } => {
            let s = engine.open_blocking(&input)?;
            openprofiler_daemon::save::save_snapshot(&s, &output)?;
            println!("Wrote {}", output.display());
            Ok(())
        }
        Cmd::Demo { output } => {
            openprofiler_snapshot_write(&output)?;
            println!("Wrote {}", output.display());
            Ok(())
        }
        Cmd::Decompile { file, output, class, smali, search, code } => {
            let s = engine.open_blocking(&file)?;
            let q = |op: &str, req: serde_json::Value| openprofiler_daemon::decompile::handle(&s, op, req);
            if let Some(dir) = output {
                let program = s.decompiler().context("not a decompiler session")?.program;
                let view = if smali { openprofiler_dex::View::Smali } else { openprofiler_dex::View::Java };
                let ext = match (smali, program.is_jvm()) {
                    (false, _) => "java",
                    (true, false) => "smali",
                    (true, true) => "bytecode",
                };
                let started = std::time::Instant::now();
                let written: Result<Vec<usize>> = program
                    .top_levels
                    .par_iter()
                    .map(|top| {
                        let code = program.class_code_desc(top, view)?;
                        let rel = top.trim_start_matches('L').trim_end_matches(';');
                        let path = dir.join(format!("{rel}.{ext}"));
                        std::fs::create_dir_all(path.parent().unwrap_or(&dir))?;
                        std::fs::write(&path, &code.code)?;
                        Ok(1)
                    })
                    .collect();
                let n = written?.len();
                eprintln!("Wrote {n} classes to {} in {:.1?}", dir.display(), started.elapsed());
                return Ok(());
            }
            if let Some(query) = search {
                let scope = if code { "code" } else { "names" };
                let r = q(
                    "search",
                    json!({ "query": query, "scope": scope, "limit": 1000, "budget_ms": 600_000 }),
                )?;
                for row in r["results"].as_array().into_iter().flatten() {
                    match row["kind"].as_str() {
                        Some("code") => println!(
                            "{}:{}  {}",
                            row["cls"].as_str().unwrap_or_default(),
                            row["line"],
                            row["snippet"].as_str().unwrap_or_default()
                        ),
                        k => println!(
                            "{:<7} {}",
                            k.unwrap_or_default(),
                            row["full"].as_str().unwrap_or_default()
                        ),
                    }
                }
                if r["truncated"] == true {
                    eprintln!("(results truncated)");
                }
            } else if let Some(class) = class {
                let view = if smali { "smali" } else { "java" };
                let r = q("class", json!({ "name": class, "view": view }))?;
                print!("{}", r["code"].as_str().unwrap_or_default());
            } else {
                // Depth-first walk of the package tree.
                let mut stack = vec![String::new()];
                while let Some(pkg) = stack.pop() {
                    let r = q("packages", json!({ "parent": pkg }))?;
                    for c in r["classes"].as_array().into_iter().flatten() {
                        println!("{}", c["full"].as_str().unwrap_or_default());
                    }
                    let subs = r["packages"].as_array().into_iter().flatten();
                    let mut subs: Vec<String> =
                        subs.filter_map(|p| p["full"].as_str().map(str::to_owned)).collect();
                    subs.reverse();
                    stack.extend(subs);
                }
            }
            Ok(())
        }
        Cmd::List => {
            let jvms = openprofiler_daemon::live::list_jvms()?;
            if jvms.is_empty() {
                println!("No JVMs found.");
            }
            for j in jvms {
                println!("{}", serde_json::to_string(&j)?);
            }
            Ok(())
        }
        Cmd::Daemon { .. } | Cmd::Mcp | Cmd::Attach { .. } | Cmd::Record { .. } | Cmd::Dump { .. } => {
            unreachable!()
        }
    }
}

async fn attach_live(
    engine: &std::sync::Arc<Engine>,
    pid: u32,
    interval: Option<String>,
) -> Result<std::sync::Arc<openprofiler_daemon::Session>> {
    let options = openprofiler_daemon::live::AttachOptions { interval, ..Default::default() };
    openprofiler_daemon::live::attach(
        engine,
        openprofiler_daemon::live::AttachRequest { pid: Some(pid), options: Some(options), infofile: None },
    )
    .await
}

async fn install_triggers(
    engine: &std::sync::Arc<Engine>,
    s: &std::sync::Arc<openprofiler_daemon::Session>,
    file: Option<&std::path::Path>,
) -> Result<()> {
    let Some(file) = file else { return Ok(()) };
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let req: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    let r = openprofiler_daemon::live::control(engine, s, "triggers", req).await?;
    eprintln!("Installed {} trigger(s)", r["installed"]);
    Ok(())
}

fn parse_duration(s: &str) -> std::result::Result<std::time::Duration, String> {
    let (num, unit) =
        s.find(|c: char| !c.is_ascii_digit() && c != '.').map(|i| s.split_at(i)).unwrap_or((s, "s"));
    let n: f64 = num.parse().map_err(|_| format!("bad duration {s}"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" | "" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(format!("bad duration unit in {s} (ms, s, m, h)")),
    };
    Ok(std::time::Duration::from_secs_f64(secs))
}

fn openprofiler_snapshot_write(output: &std::path::Path) -> Result<()> {
    let engine = Engine::new();
    let s = engine.add_profile(
        openprofiler_daemon::SessionKind::Snapshot,
        "demo",
        openprofiler_daemon::demo::demo_profile(),
    );
    openprofiler_daemon::save::save_snapshot(&s, output)
}

/// The UI shipped in release archives (`<exe dir>/ui`), else the development build (`ui/dist`).
fn default_ui_dir() -> PathBuf {
    std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ui")))
        .filter(|d| d.join("index.html").exists())
        .unwrap_or_else(|| PathBuf::from("ui/dist"))
}

/// Writes `~/.openprofiler/daemon.json` (mode 0600) so IDE plugins can find the daemon.
fn publish_discovery(d: &openprofiler_daemon::RunningDaemon) -> Result<()> {
    use std::io::Write;
    let home =
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).context("HOME not set")?;
    let dir = PathBuf::from(home).join(".openprofiler");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon.json");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&path)?;
    let body = json!({ "url": d.url(), "token": d.token, "pid": std::process::id() });
    f.write_all(serde_json::to_string_pretty(&body)?.as_bytes())?;
    Ok(())
}
