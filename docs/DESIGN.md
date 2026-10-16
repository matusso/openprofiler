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
- A native decompiler for Android apps (`.apk`, `.dex`) and JVM bytecode (`.jar`, `.class`) in the
  same UI, so the code behind a hot spot or a third-party library can be read without another tool.
- **Fast.** Open a 10 GB HPROF in under 3 minutes cold on a laptop with 16 GB RAM, and in under
  5 s on reopen. The agent should add at most 2% overhead with async sampling.
- **Modern UX.** Dark and light themes, a command palette, keyboard-first navigation, fast flame
  graphs and timelines, the same UI on the desktop and in a browser.
- **Open formats.** A documented snapshot format and protobuf schemas. HPROF and JFR import are
  first-class.
- **Secure by default.** The agent port uses TLS with a pinned fingerprint and a mandatory token.
- Licence: Apache-2.0; the native agent is MPL-2.0.

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
| CLI | Rust `clap` binary `openprofiler` | open, export, compare, analyze, attach, record, dump, decompile, mcp. |
| Decompiler | **Rust** (`openprofiler-dex`) | No JVM or external tool; lazy per-class decompilation in process, with navigation metadata from the code generator. |

A single Rust workspace covers everything except the Java shim and the TypeScript UI, so the analysis crates are shared by the daemon, the CLI and the agent.

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
 │         ├─ JFR session ───────────────┘   openprofiler-model (trees, hot spots, diff)
 │         └─ dex session (.apk/.dex/.jar) ─► openprofiler-dex (decompiler, resources)
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
  openprofiler-dex/        DEX/APK and class-file/JAR readers, smali and bytecode views, decompiler
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

- A left navigation (Overview, CPU, Memory, Heap, Threads, Locks, Events, MBeans, Code, Compare), a top
  session bar with recording controls and a timeline scrubber, and an inspector panel.
- The Code view (decompiler sessions only): a lazy package tree and search on the left, a
  virtualized code viewer with syntax highlighting, clickable references, find in file, Java and
  smali (or, for JVM classes, bytecode) tabs, back/forward history, and a usages panel; a Resources
  tab shows decoded resources (or a jar's other files).
- The MBeans view (live sessions): a server picker (the platform MBean server, other MBean servers
  in the JVM, remote JMX servers the JVM connects to), the beans as a domain tree, attribute values
  (auto-refreshed, writable ones editable), operations with typed argument fields, notifications of
  subscribed beans, and numeric attributes plotted as telemetry series sampled by the agent.
- A global time range: brushing the timeline filters every view.
- A command palette (⌘K) for actions, views, classes and methods, with go-to-source through the IDE
  bridge.
- Flame graphs with search, zoom and differential colouring; a sunburst of the dominator tree; a
  reference graph; an Insights panel with severity and estimated waste.
- Web mode: `openprofiler daemon --web` serves the same UI with token auth.

### 3.11 MCP and automation

- `openprofiler mcp` runs an MCP server on stdio. Tools include `list_jvms`, `attach`,
  `prepare_profiling`, `load_snapshot`, `hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`,
  `compare`, `thread_dump` and the MBean tools (`list_mbean_servers`, `connect_jmx`,
  `list_mbeans`, `get_mbean`, `invoke_mbean`, `track_mbean`, `subscribe_mbean`,
  `mbean_notifications`); `decompile`, `search_code` and `find_usages` read decompiled apps. Responses carry stable node IDs and `suggested_next` hints.
- CI gate: `openprofiler compare base.opsnap head.opsnap --fail-if "cpu.total > +10%"`.

### 3.12 IDE bridge

A WebSocket JSON-RPC endpoint on localhost with a token: `navigate`, `gutterData`, `hello`, `agent`,
`attachLaunched`. The daemon writes `~/.openprofiler/daemon.json` so plugins can find it. The
IntelliJ plugin and the VS Code / VSCodium extension show CPU heat, navigate to source, and launch
Java programs with `-agentpath:<agent>=tls,infofile=<tmp file>`; `attachLaunched` waits for the
agent to publish that file and opens a live session.

### 3.13 Decompiler (`openprofiler-dex`, daemon `decompile` module)

A native decompiler for Dalvik and JVM bytecode. The daemon holds one `Program` per `dex` session
and serves it through the `/code/*` Query API; the CLI (`openprofiler decompile`) and MCP use the
same calls.

- **Input.** `.dex` files (`dex\n` magic) and zip containers (`.apk`, `.xapk`, `.apks`, `.apkm`,
  `.aab`): `classes*.dex` at the root, `*/dex/classes*.dex` in bundles, or inside nested APKs.
  Zip entries are found through the central directory (APK protectors corrupt local headers).
  Every read is bounds-checked; malformed input produces errors, never panics.
- **JVM class files** (`jvm/`). `.class` files (`CAFEBABE` magic with a class-file major version,
  which tells them from Mach-O universal binaries) and zips of them (`.jar`, `.war`, `.ear`,
  `.aar`, and `.jmod` behind its `JM` header); archives without class files load the jars inside
  them. Class files become the same program model as dex files: constant pools are interned into
  shared string/type/proto/field/method tables, attributes become the system annotations the
  printer reads (`Signature`, `InnerClass`, `EnclosingClass`/`EnclosingMethod`, `Throws`), and
  `ConstantValue`s become static values. Indexing parses every class (in parallel) but translates
  no code; a method's code is translated when it is first decompiled:
  - The operand stack becomes registers: local slot `n` is register `n`, stack word `d` is
    register `max_locals + d`, then parameters are rotated to the top of the frame as in Dalvik.
    A forward data-flow pass gives the stack shape at every instruction (one per instruction, as
    the verifier requires); `dup*`/`swap` become parallel moves.
  - `new` + `dup` + `<init>` construct the allocation's own register, so the shared pass forms
    `new T(...)`. `invokedynamic` with `StringConcatFactory` becomes a `StringBuilder` chain
    (printed as `a + b`); with `LambdaMetafactory` it becomes a lambda expression carrying the
    implementation method and captured values. `multianewarray` becomes `Array.newInstance`,
    printed back as `new T[a][b]`. A handler starting with `astore` catches straight into the
    local.
  - The exception table becomes non-overlapping try ranges whose handler lists keep table order,
    including handlers shadowed by an earlier one of the same type: an enclosing `try`'s handlers
    then stay a suffix of the inner list, which is how regions nest.
  - Line numbers and `LocalVariableTable`/`LocalVariableTypeTable` (or `MethodParameters`) become
    debug info, so names and generic local types survive.
  - `jsr`/`ret` subroutines (`finally` in class files for Java 1.4 and earlier) are not inlined:
    each `jsr` stores a key for its call site as the return address and jumps, and `ret` becomes
    a switch over the call sites' return points. The code is correct but usually not fully
    structured.
  The printer adds what dex code doesn't need: lambda bodies (synthetic `lambda$` methods) are
  printed in place and the methods hidden (the class is printed again with a method shown if one of
  its lambdas can't be inlined); other targets print as method references; records print as
  `record R(...)` without their generated members; enum constants with class bodies print the
  body inline; locals holding a lambda of a `java.util.function` interface get its generic type
  when debug info doesn't have one. The low-level view of a JVM class is its original bytecode
  (`jvm/disasm.rs`) with offsets, resolved constants and linked member references.
- **Pipeline per method** (`method.rs`, `structure.rs`, `java.rs`):
  1. Decode instructions (`insn.rs`), split basic blocks at branches, switch targets and try
     boundaries; exception edges only leave blocks that can throw.
  2. **Variables.** Reaching definitions over registers (wide pairs included) give def-use webs;
     each web is one local. Definitions inside one debug-info local's range are merged so source
     variables keep one name. Uses that read the same constant on every path become literals,
     because d8 shares constant registers between unrelated values.
  3. **IR** with expressions over those variables; `new-instance` + `<init>` become `new T(...)`,
     `new-array` + element stores become array literals, `StringBuilder` chains become `+`.
  4. **Types** from definitions, uses and debug info; constants print by the type expected where
     they are used (`false`, `'a'`, `1.5f`, `null`).
  5. **Inlining.** Single-use temporaries are substituted into their user when evaluation order
     allows: decisions run last statement first and iterate to a fixpoint; a move is valid when
     every conflicting statement in between still runs after it in the final evaluation order.
     `StringBuilder` appends of strings and primitives are treated as reorderable.
  6. **Conditions.** `cmp` + `if` become comparisons; `if` chains merge into `&&`/`||`; jumps
     through empty blocks are threaded.
  7. **Structuring.** Dominators (over normal and exception edges) and post-dominators drive a
     region walk that emits `if`/`else`, `while`/`do-while`/`for`, labeled `break`/`continue`,
     `switch` (cases ordered by key, fallthrough chains kept), nested `try`/`catch`
     (regions nest when a handler list is a suffix of another), and `synchronized`. Joins fall
     back to the block where branches really meet when one branch returns early; small shared
     exit blocks may be duplicated. Cleanups invert early exits, build ternaries, merge boolean
     returns and turn index and iterator loops into for-each.
  8. **Java.** Imports, generics from `Signature` annotations, enums (constants recovered from
     `<clinit>`), member, anonymous and inner classes (the outer instance becomes `Outer.this`),
     annotations, variable declarations placed in the innermost scope that covers all uses.
  Methods that can't be structured are still printed, with a warning comment; a method that
  fails to decompile (or panics) falls back to a comment and an `UnsupportedOperationException`.
- **Navigation.** The printer records links (reference or declaration, byte offset → UTF-16) to
  class, method and field nodes; nodes get small ids per session. Usages come from a cross
  reference index built by scanning bytecode, then decompiling only the classes that use a node.
- **Resources** (`res/`). Binary XML (manifest, layouts) and `resources.arsc` (values files per
  configuration, `public.xml`), with names resolved through the table and a framework name list.
- **Quality checks.** `tests/corpus.rs` decompiles a corpus built with javac + d8 (with and
  without debug info) and the same classes as jars, recompiles the output with javac and runs a
  harness that must print the same as the original classes. `tests/jvm.rs` does the same for a
  `--release 21` corpus of modern constructs (records, sealed types, switch expressions, patterns,
  lambdas, streams, try-with-resources, local and anonymous classes). The `decompile` example
  with `--stats` reports fallbacks over a whole archive (on the JDK's `java.base.jmod`: 3,347
  classes, no failed methods, about 1.2% printed with a restructure note; on Guava, 0.2%).

## 4. Roadmap

| Phase | Scope | State |
|---|---|---|
| 0 Foundations | Workspace, CI, JVMTI bindings, protobuf specs, `.opsnap` spec | Done |
| 1 Offline analyzer | HPROF/heap engine, JFR import, Query API, UI, CLI, MCP | Done |
| 2 Live agent | Sampling, allocation and monitor events, `j.u.c` locks, virtual threads, telemetry, dumps, triggers, TLS link, local attach, offline recording | Done |
| 3 Probes and instrumentation | Bytecode instrumentation, exact call counts, probe SPI, JDBC/HTTP/JPA/Kafka/gRPC probes, async context propagation, custom probes | Not started |
| 4 Remote and cloud | Relay, ssh/docker/kubectl, debug containers, verified upload, web mode, CI gate | Mostly done |
| 5 Polish and ecosystem | Native classfile rewriter, native heap dumper, call tracer, MBean browser, IntelliJ plugin, Android and JVM decompiler, Windows agent, PHD import, Arrow IPC sections | Partial |

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
- **jadx** (Apache-2.0): the reference Dex decompiler; its output conventions (`AnonymousClass1`,
  restructure warnings) and jadx-gui's navigation model informed the Code view. No code is shared.
- **apktool**: the layout of decoded `res/values*` files.
