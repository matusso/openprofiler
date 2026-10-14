# OpenProfiler design

This document describes the goals, architecture and roadmap of OpenProfiler. The wire and file
formats are specified separately:

- [Agent-link protocol](spec/agent-link.md) (agent ↔ daemon)
- [Query API v1](spec/query-api.md) (daemon ↔ UI, CLI, MCP, IDE)
- [`.opsnap` snapshot format](spec/snapshot-format.md)

## 1. Goals

**Product goals**
- A complete JVM profiler: CPU, memory, heap analysis, threads, locks, probes, telemetry, snapshots,
  comparisons, remote and container attach, and an MCP server for AI agents.
- **Fast.** Open a 10 GB HPROF in under 3 minutes cold on a laptop with 16 GB RAM, and in under
  5 s on reopen. The agent should add at most 2% overhead with async sampling.
- **Modern UX.** Dark and light themes, a command palette, keyboard-first navigation, fast flame
  graphs and timelines, the same UI on the desktop and in a browser.
- **Open formats.** A documented snapshot format and protobuf schemas. HPROF and JFR import are
  first-class.
- **Secure by default.** The agent port uses TLS with a pinned fingerprint and a mandatory token.
- Licence: Apache-2.0.

**Non-goals for v1:** Java < 11, IBM PHD import (later), a Windows agent (v1.x), Eclipse and
NetBeans plugins.

## 2. Technology choices

| Layer | Choice | Why |
|---|---|---|
| In-JVM native agent | **Rust** `cdylib` with hand-written JVMTI bindings (`jvmti-sys`) | No runtime and no GC, signal-safe code, and static musl builds are possible. |
| In-JVM Java shim | Java 11 bytecode on the boot class path | Probe logic must touch application objects. Static callees keep the hot path cheap. |
| Bytecode rewriting | **First:** ASM inside the shim. **Later:** a Rust classfile rewriter in ClassFileLoadHook. | ASM gets correct StackMapTables quickly. The Rust rewriter avoids class-loading recursion at boot. |
| Analysis engines | **Rust** (`rayon`, `memmap2`, `zstd`, `prost`) | Heap graphs with over 100M nodes, tree aggregation, diffs. |
| Daemon / query server | Rust (`tokio`, `axum` HTTP + WebSocket) | One process serves the desktop UI, the web UI, the CLI and MCP. |
| UI | **Tauri 2 + React + TypeScript**, canvas renderers | A native desktop shell, and the same UI works in a browser. |
| Graph layout | `elkjs` in a web worker | Call graphs and reference graphs. |
| Tables and trees | Virtualized lazy tree-table | Millions of rows with lazy expansion. |
| Relay / attach helper | Static musl Rust binary `openprofiler-relay` (x86_64, aarch64, armhf) | Runs inside remote hosts and containers without a JVM. |
| CLI | Rust `clap` binary `openprofiler` | open, export, compare, analyze, attach, record, dump, mcp. |

A single Rust workspace covers everything except the Java shim and the TypeScript UI, so the
analysis crates are shared by the daemon, the CLI and the agent.

## 3. Architecture

```
                 ┌──────────── openprofiler-ui (Tauri / browser) ────────────┐
                 │ views · command palette · flame/sunburst · graph layouts  │
                 └───────────────────────▲───────────────────────────────────┘
                                         │  Query API v1 (HTTP + WebSocket)
 ┌───────────────── openprofiler-daemon ─┴─────────────────────────────────────┐
 │ Engine ─┬─ live session  ◄── agent-link (TLS + token, zstd frames) ──────────┼──► target JVM
 │         ├─ .opsnap / .oplink session ─┐                                      │
 │         ├─ HPROF session ─────────────┼─► openprofiler-heap (mmap CSR, dominators)
 │         └─ JFR session ───────────────┘   openprofiler-model (trees, hot spots, diff)
 │ Exporters (HTML/CSV/JSON/SVG) · Compare · MCP server · IDE bridge             │
 │ Remote: ssh · docker/kubectl exec · openprofiler-relay upload + stdio tunnel  │
 └──────────────────────────────────────────────────────────────────────────────┘

 Target JVM:  libopenprofiler_agent (Rust) ◄─JNI─► boot classes (Java: telemetry, dumps, Controller)
```

Design principles:

1. **One query API for live data and saved data.** Live sessions, snapshots, HPROF dumps and JFR
   recordings are all served through the same Query API, so every view works on every source.
2. **Analysis stays where the data is; the UI fetches lazily.** Trees are expanded by node ID, and
   the daemon pushes invalidations instead of data.
3. **Heap analysis is out-of-core.** mmap'd CSR graphs and a persistent analysis index.
4. **The in-JVM part is native plus thin boot classes.** The agent is lean; the daemon owns the
   aggregate state.
5. **Containers are reached through a tiny static relay binary,** not by shipping a JVM launcher
   into the target.

### 3.1 Repository layout

```
crates/
  jvmti-sys/               JVMTI/JNI bindings with version-sized callback structs
  openprofiler-agent/      cdylib: Agent_OnLoad/OnAttach, samplers, events, triggers, agent-link server
  openprofiler-proto/      protobuf schemas (agent-link, triggers)
  openprofiler-model/      symbols, stack trie, samples; call tree, hot spots, flame, call graph, diff
  openprofiler-snapshot/   .opsnap container read/write
  openprofiler-hprof/      zero-copy HPROF parser (1.0.1/1.0.2, gzip)
  openprofiler-jfr/        metadata-driven JFR parser → openprofiler-model
  openprofiler-heap/       object index, CSR graphs, dominators, retained sizes, inspections, paths
  openprofiler-attach/     hsperfdata discovery, HotSpot attach, container NSpid handling
  openprofiler-link/       daemon side of the agent-link protocol, .oplink replay
  openprofiler-remote/     ssh, docker and kubectl transports
  openprofiler-relay/      static helper binary (relay, list, attach, dump, pty)
  openprofiler-daemon/     Query API server, sessions, exporters, insights, MCP, IDE bridge
  openprofiler-cli/        the `openprofiler` binary
java/openprofiler-boot/    boot classes: telemetry sampler, dumps, Controller
java/openprofiler-api/     public Controller API
ui/                        React UI and Tauri desktop shell
ide/intellij/              IntelliJ IDEA plugin
ide/vscode/                VS Code / VSCodium extension
docs/spec/                 agent-link, Query API, snapshot format
```

### 3.2 Native agent (`openprofiler-agent`)

**Options.** `-agentpath:libopenprofiler_agent.so=port=0,bind=127.0.0.1,token=…,mode=live|offline,…`,
the `OPENPROFILER_AGENT_OPTIONS` env var, or the attach argument. With `port=0` the agent picks an
ephemeral port and publishes `{pid, port, token, fingerprint}` in `$TMPDIR/openprofiler-<pid>.json`
(mode 0600).

**Capabilities** are requested in groups, and the agent records which ones were granted, so it
degrades gracefully when the JVM refuses one group.

**CPU sampling.**
1. **Async (default on Linux):** per-thread CPU timers deliver a signal; the handler calls
   `AsyncGetCallTrace` and copies frames into a lock-free ring buffer. A drainer thread aggregates
   samples into the call tree. VMStructs are used only for robustness checks and stack-walk repair.
2. **Sync fallback:** `GetThreadListStackTraces` from a sampler thread. Used on macOS and when async
   sampling is unavailable.
3. **JFR mode (planned):** stream JFR `ExecutionSample` events for locked-down environments.
4. **Virtual threads:** mount and unmount extension events, aggregated per carrier.

**Allocation:** `SampledObjectAlloc` with a configurable interval (default 512 KB), stack capture,
and object tags with ObjectFree for live versus collected counts.

**Monitors:** JVMTI contended-enter and wait events with thresholds; `java.util.concurrent` locks
through `LockSupport.park` hooks, with lock owners.

**Heap dumps:** `HotSpotDiagnosticMXBean.dumpHeap` (HPROF), streamed back to the daemon. A native
dumper that writes directly into a snapshot is planned.

**Telemetry:** a once-per-second sampler in the boot classes (MXBeans and GC notifications) pushes
compact records into a native time-series ring.

**Offline mode:** writes an `.oplink` recording locally on triggers or at exit, using the same code
that serves live sessions.

### 3.3 Java shim (boot classes)

- Static callee classes. The hot path is a static flag check followed by a JNI call.
- Public API (`openprofiler-api`): `OpenProfiler.startCpuRecording()`, `saveSnapshot(path)`,
  `bookmark(text)`, and later a custom-probe API.
- Planned for probes: a `Probe` SPI with interceptions, `onEnter`/`onExit`, and emitters for
  payloads, events, control objects and telemetry. Library-specific helpers are defined into the
  application's class loader, sharded by library version. A context-propagation layer covers
  executors, `CompletableFuture`, Reactor, Kotlin coroutines and virtual threads.

### 3.4 Agent-link protocol

See [spec/agent-link.md](spec/agent-link.md). In short:

- TCP with **TLS 1.3 by default** (self-signed, pinned fingerprint) or plain TCP over loopback or a
  relay, always with a **mandatory random token**.
- Framing: `u32 len | u16 type | u8 flags | payload (protobuf)`, optionally zstd-compressed.
- The agent pushes deltas (symbols, samples, telemetry, events); the daemon sends requests
  (thread dump, heap dump, recording control, config updates).
- Reconnect: the agent keeps its aggregates and resends from the last acknowledged watermark.

### 3.5 Query API v1

See [spec/query-api.md](spec/query-api.md). It is the single contract used by the UI, the CLI
exporter, MCP and the IDE bridge, and it works the same for live, `.opsnap`, HPROF and JFR sessions:
sessions, telemetry (LTTB-downsampled), lazy trees (call tree, hot spots, back traces, flame),
heap (classes, biggest objects, references, paths to roots, inspections), threads, monitors,
compare and export. The UI subscribes per visible view and the daemon pushes invalidations.

### 3.6 Snapshot format (`.opsnap`)

See [spec/snapshot-format.md](spec/snapshot-format.md). A header, independently zstd-compressed
sections (symbols, stacks, samples, threads, telemetry, monitors, bookmarks, thread dumps,
embedded HPROF), an index with xxh3 checksums, and a trailer. The heap analysis index lives next to
the file in `<file>.openprofiler-idx/`.

### 3.7 Heap analysis engine (`openprofiler-heap`)

1. Parallel streaming pass over the mmap'd HPROF: object addresses, classes, shallow sizes and
   references.
2. Dense `u32` object IDs over a sorted address array.
3. Outgoing and incoming CSR graphs, built in parallel, stored as mmap'd files.
4. A synthetic super-root over GC roots. The referent field of `java.lang.ref.Reference` subclasses
   is excluded.
5. **Semi-NCA dominators** with retained sizes accumulated in reverse postorder.
6. A persistent index; reopening is an mmap plus a header check.
7. Inspections: duplicate strings and arrays, sparse and empty collections, bad hash distribution,
   ThreadLocal leaks, inner-class retention, class-loader leaks.
8. Paths to GC roots by BFS on the incoming graph, with an option to exclude weak references.

Correctness is checked against Eclipse MAT, which is used as an oracle for retained sizes
(`crates/openprofiler-heap/tests/mat_parity.rs`). The memory budget is about 32 bytes per object
across all indexes, on disk and mmap'd.

### 3.8 CPU and tree engine (`openprofiler-model`)

- Interned frames: method, line and frame type (interpreted, JIT, inlined, native, kernel).
- Aggregation by method, class or package; thread-state partitions.
- Derived views computed on demand: hot spots with back traces, inverted trees, collapsed recursion,
  flame layout, call graph, and diff trees for comparisons.

### 3.9 Remote and containers

- **Local discovery:** hsperfdata and `/proc`: JVM name, arguments, version, whether an agent is
  already loaded.
- **Attach:** HotSpot dynamic attach (`.attach_pid`, SIGQUIT, `.java_pid` socket). In containers:
  NSpid translation, `/proc/<pid>/root`, and copying the agent into the target's mount namespace.
- **`openprofiler-relay`** (static musl, about 1 MB) runs on the remote host or in the container:
  `relay <port>` (stdio ↔ TCP with a READY handshake), `list`, `attach`, `dump`, `pty-exec`.
- **Transports:** ssh, `docker exec -i`, `kubectl exec -i`, and ephemeral debug containers
  (`kubectl debug --target`) for distroless images. The relay and agent bundle are uploaded through
  stdin with sha256 verification, so no port is ever exposed.

### 3.10 UI

- A left navigation (Overview, CPU, Memory, Heap, Threads, Locks, Events, Compare), a top session
  bar with recording controls and a timeline scrubber, and an inspector panel.
- A global time range: brushing the timeline filters every view.
- A command palette (⌘K) for actions, views, classes and methods, with go-to-source through the IDE
  bridge.
- Flame graphs with search, zoom and differential colouring; a sunburst of the dominator tree; a
  reference graph; an Insights panel with severity and estimated waste.
- Web mode: `openprofiler daemon --web` serves the same UI with token auth.

### 3.11 MCP and automation

- `openprofiler mcp` runs an MCP server on stdio. Tools include `list_jvms`, `attach`,
  `prepare_profiling`, `load_snapshot`, `hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`,
  `compare` and `thread_dump`. Responses carry stable node IDs and `suggested_next` hints.
- CI gate: `openprofiler compare base.opsnap head.opsnap --fail-if "cpu.total > +10%"`.

### 3.12 IDE bridge

A WebSocket JSON-RPC endpoint on localhost with a token: `navigate`, `gutterData`, `hello`, `agent`,
`attachLaunched`. The daemon writes `~/.openprofiler/daemon.json` so plugins can find it. The
IntelliJ plugin and the VS Code / VSCodium extension show CPU heat, navigate to source, and launch
Java programs with `-agentpath:<agent>=tls,infofile=<tmp file>`; `attachLaunched` waits for the
agent to publish that file and opens a live session.

## 4. Roadmap

| Phase | Scope | State |
|---|---|---|
| 0 Foundations | Workspace, CI, JVMTI bindings, protobuf specs, `.opsnap` spec | Done |
| 1 Offline analyzer | HPROF/heap engine, JFR import, Query API, UI, CLI, MCP | Done |
| 2 Live agent | Sampling, allocation and monitor events, `j.u.c` locks, virtual threads, telemetry, dumps, triggers, TLS link, local attach, offline recording | Done |
| 3 Probes and instrumentation | Bytecode instrumentation, exact call counts, probe SPI, JDBC/HTTP/JPA/Kafka/gRPC probes, async context propagation, custom probes | Not started |
| 4 Remote and cloud | Relay, ssh/docker/kubectl, debug containers, verified upload, web mode, CI gate | Mostly done |
| 5 Polish and ecosystem | Native classfile rewriter, native heap dumper, call tracer, MBean browser, IntelliJ plugin, Windows agent, PHD import, Arrow IPC sections | Partial |

Open work is tracked in [GitHub issues](https://github.com/matusso/openprofiler/issues).

**Exit criteria**
- Phase 1: a 10 GB HPROF opens in under 3 min cold and under 5 s warm; retained sizes match MAT
  exactly on a reference corpus. *(met)*
- Phase 2: at most 2% overhead at 100 Hz async sampling on Renaissance/DaCapo; a 24 h soak without
  leaks or crashes.
- Phase 3: Spring Boot Petclinic and Quarkus demos show SQL and URL hot spots linked to call trees,
  at under 5% overhead.
- Phase 4: attach to a JVM in a distroless pod on a remote cluster in three clicks or fewer, without
  exposing ports.

## 5. Risks and mitigations

| Risk | Mitigation |
|---|---|
| `AsyncGetCallTrace` instability or removal | The sampler is abstracted; sync and JFR back ends exist or are planned. Track the JDK CPU-time JFR sampler (JDK 25+). |
| VMStructs fragility across JDK builds | Used only as an optional guard; feature-detected and disabled gracefully. JDK matrix in CI. |
| Bytecode instrumentation correctness | Start with ASM (`COMPUTE_FRAMES` with a class-loader-aware `getCommonSuperClass`); fuzz with the JDK class corpus; verify with `-Xverify`; skip methods over 64 KB. |
| Probe breadth across library versions | Probe SPI, a version-matrix harness with Testcontainers, prioritise by usage. |
| Memory on huge heap dumps | Out-of-core design, disk-backed CSR, benchmark gates. |
| Signal handler conflicts | Chain prior handlers, make the signal configurable, detect other profilers and warn. |
| Restrictive containers (no ptrace, read-only /tmp, non-root) | Ephemeral debug containers, configurable writable dir, JFR-only mode. |
| UI performance with millions of nodes | Server-side lazy trees, virtualized rendering, LTTB downsampling. |

## 6. Prior art

- **async-profiler** (Apache-2.0): AsyncGetCallTrace, perf_events, VMStructs, wall-clock sampling.
- **jattach** (Apache-2.0): HotSpot and OpenJ9 attach, including in containers.
- **Eclipse MAT** (EPL): dominator and inspection semantics; the correctness oracle.
- **JDK Mission Control** (UPL): JFR semantics. **hprof-slurp**: streaming HPROF parsing.
- **Pyroscope, speedscope**: flame graph UX. **elkjs**: graph layout.
