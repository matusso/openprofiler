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

**Non-goals for v1:** Java < 11, IBM PHD import (later), Eclipse and NetBeans plugins.

## 2. Technology choices

| Layer | Choice | Why |
|---|---|---|
| In-JVM native agent | **Rust** `cdylib` with hand-written JVMTI bindings (`jvmti-sys`) | No runtime and no GC, signal-safe code, and static musl builds are possible. |
| In-JVM Java shim | Java 11 bytecode on the boot class path | Probe logic must touch application objects. Static callees keep the hot path cheap. |
| Bytecode rewriting | A Rust class-file rewriter in `ClassFileLoadHook` (`openprofiler-agent/src/instrument`) | No Java runs while a class loads (no class-loading recursion, no ASM dependency in the boot classes); injected code never changes the types at existing frames, so `StackMapTable`s are remapped rather than recomputed. |
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
  openprofiler-agent/      cdylib: Agent_OnLoad/OnAttach, samplers, events, triggers, instrumentation, agent-link server
  openprofiler-proto/      protobuf schemas (agent-link, triggers)
  openprofiler-model/      symbols, stack trie, samples; call tree, hot spots, flame, call graph, diff
  openprofiler-snapshot/   .opsnap container read/write
  openprofiler-hprof/      zero-copy HPROF parser (1.0.1/1.0.2, gzip)
  openprofiler-phd/        IBM Portable Heap Dump (OpenJ9) parser and PHD → HPROF conversion
  openprofiler-jfr/        metadata-driven JFR parser → openprofiler-model
  openprofiler-heap/       object index, CSR graphs, dominators, retained sizes, inspections, paths
  openprofiler-attach/     hsperfdata discovery, HotSpot attach, container NSpid handling
  openprofiler-link/       daemon side of the agent-link protocol, .oplink replay
  openprofiler-remote/     ssh, docker and kubectl transports
  openprofiler-relay/      static helper binary (relay, list, attach, dump, pty)
  openprofiler-dex/        DEX/APK and class-file/JAR readers, smali and bytecode views, decompiler
  openprofiler-daemon/     Query API server, sessions, exporters, insights, MCP, IDE bridge
  openprofiler-cli/        the `openprofiler` binary
java/openprofiler-boot/    boot classes: telemetry sampler, dumps, Controller, probe SPI and probes
java/openprofiler-api/     public Controller API and the @Probe annotation
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

**Platforms.** Linux (glibc and musl), macOS and Windows (`openprofiler_agent.dll`). On Windows
the agent uses the sync sampler, finds VMStructs through `jvm.dll`'s exports, and writes the info
file to `%TEMP%` without Unix permission bits (the user's profile directory is private). The
Windows port was written and cross-compiled on macOS; CI builds it and runs the workspace tests
(live agent and attach included) on `windows-2025`, which is its first real run.

**Capabilities** are requested in groups, and the agent records which ones were granted, so it
degrades gracefully when the JVM refuses one group.

**CPU sampling.**
1. **Async (default on Linux):** per-thread CPU timers deliver a signal; the handler calls
   `AsyncGetCallTrace` and copies frames into a lock-free ring buffer. A drainer thread aggregates
   samples into the call tree. VMStructs are used only for robustness checks and stack-walk repair.
2. **Sync fallback:** `GetThreadListStackTraces` from a sampler thread. Used on macOS, Windows and
   when async sampling is unavailable.
3. **JFR mode (planned):** stream JFR `ExecutionSample` events for locked-down environments.
4. **Virtual threads:** mount and unmount extension events, aggregated per carrier.

**Allocation:** `SampledObjectAlloc` with a configurable interval (default 512 KB), stack capture,
and object tags with ObjectFree for live versus collected counts.

**Monitors:** JVMTI contended-enter and wait events with thresholds; `java.util.concurrent` locks
through `LockSupport.park` hooks, with lock owners. Events above the threshold are recorded
(within a per-second budget), shorter contention is sampled by blocked time, and stacks are
resolved off the application thread, so heavy contention is not made worse.

**Heap dumps:** `HotSpotDiagnosticMXBean.dumpHeap` (HPROF), streamed back to the daemon. A native
dumper that writes directly into a snapshot is planned.

**Telemetry:** a once-per-second sampler in the boot classes (MXBeans and GC notifications) pushes
compact records into a native time-series ring.

**Instrumentation** (`instrument/`): `ClassFileLoadHook` rewrites classes as they load or are
retransformed. A method gets an entry sequence, an exit sequence before every return and a
catch-all handler that runs the exit hooks for exceptions and rethrows (in constructors only after
the `super(...)` call, since a handler cannot cover uninitialized `this`). Branch offsets, switch
padding, the exception table, line and local-variable tables and `StackMapTable` frames (including
`Uninitialized` offsets) are remapped; the handler's frame has no locals, which is valid at every
instruction it covers. Methods with `jsr`/`ret`, overflowing branches or code over 64 KB are left
alone. What gets injected:
- *Call counting* for classes matching `instrument=` patterns: native `Hooks.callEnter/callExit`
  keep a per-thread shadow stack and call trie in native thread-local storage, flushed about once a
  second as `calls` samples (self time, invocations), so the existing tree, hot spot and flame
  views show exact counts. Auto-tuning removes hot methods whose self time is close to the hook
  overhead (at least 50,000 calls in 5 s, mean self time under 400 ns).
- *Probe interceptions* (`Hooks.enter/exit/exitThrow` with `this`, boxed arguments and the return
  value), *custom probes* (`@Probe` methods) and *method triggers* (`Hooks.trigger`).

Interceptions match one class or every subtype of a class or interface (the type included). The
hook sees supertypes only by name, and a class loads before its supertypes, so subtype masks are
completed at `ClassPrepare` and late matches are retransformed there, before the class's code
runs. Superclasses that implement an intercepted method for their subtypes without being subtypes
themselves are instrumented with sites that check the receiver's type. Instrumented classes in named (JDK) modules get
a reads edge to the boot loader's unnamed module before their code runs. Classes loaded earlier
(bootstrap classes; everything on dynamic attach) are found by scanning the loaded classes
(subtypes through `IsAssignableFrom`) and retransformed. `SetInstrumentation` changes the patterns
and probes at run time. The rewriter is checked by rewriting a javac corpus with every hook and
running it under the verifier (`tests/rewrite.rs`).

**Offline mode:** writes an `.oplink` recording locally on triggers or at exit, using the same code
that serves live sessions.

### 3.3 Java shim (boot classes)

- Static callee classes. The hot path is a static flag check followed by a JNI call.
- Public API (`openprofiler-api`): `OpenProfiler.startCpuRecording()`, `saveSnapshot(path)`,
  `bookmark(text)`, `probeEvent(probe, payload, duration)`, and the `@Probe(value, payload)`
  annotation that turns a method into a custom probe (payload templates `{0}`, `{this}`). The API
  looks for the agent again while it is absent, so it also works after a dynamic attach.
- **Probe SPI** (`Probe`, `Interception`, `Call`): a probe lists its interceptions (a method of a
  class or of all its subtypes, optionally with boxed arguments, exit-only for constructors or
  entry-only for calls that need no exit) and
  fills a `Call` in `onEnter`/`onExit` (payload, detail, error, control object). Emitters open and
  close control objects (tracked in weak identity maps, so objects collected without close are
  reported) and set the thread's request label. The runtime (`Probes`) guards against re-entrancy,
  records only the outermost of nested calls of one probe, drops calls whose exit was missed, and
  never throws into application code. Probes reach library objects reflectively through public
  supertypes (`Reflect`), cached per class with `ClassValue`, so no helper classes have to be
  defined into application class loaders.
- Probes: JDBC (SQL, bind parameters, physical and pooled connections), JPA (javax and Jakarta),
  HTTP server (Servlet, also asynchronous; JDK `HttpServer`; Netty handlers; Reactor Netty, so
  WebFlux; Vert.x, so Quarkus) and client (JDK `HttpClient`, `HttpURLConnection`, OkHttp, Apache
  HttpClient 4/5), Kafka, gRPC, exceptions, files, sockets. Threads aggregate their calls per
  second, payload and stack, and the native side keeps individual events within a per-probe budget
  (slow calls always). Stacks of probe calls are sampled per (probe, request, payload) at most every
  10 ms (events always get their own), since walking deep stacks on every call would dominate the
  cost of the probes.
- **Request labels and async context.** HTTP and gRPC server probes set the thread's request label
  (`GET /owners/{id}`), which becomes the root frame of that thread's sampled and instrumented
  stacks (call trees split by request) and the context of its probe events. An internal probe
  carries the label across `FutureTask`, `ForkJoinTask` (`CompletableFuture`, parallel streams),
  `ThreadPoolExecutor.execute`, `Thread.start` (also virtual threads), Kotlin coroutine dispatch
  and JBoss Threads' `EnhancedQueueExecutor`. Asynchronous servers are labelled while they
  dispatch a request and record it when its response completes.
- **Version matrix** (`java/matrix/`): every probe runs against real libraries and versions
  (PostgreSQL, MySQL and Kafka in containers) and the recordings are checked for payloads, events,
  control objects, request context and call-tree links. The demo benchmark (`java/bench/`) measures
  the overhead on Spring Petclinic and the Quarkus Hibernate ORM quickstart.

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

**IBM Portable Heap Dumps** (`.phd`, optionally gzipped; OpenJ9 / IBM Semeru) are converted to an
HPROF file in the index directory first (`openprofiler-phd`), then indexed like any other dump. A
PHD records classes, object addresses, non-null references, array lengths and (from version 6)
object sizes, but no field names or values, no primitive data and no GC roots. The conversion
therefore:
- names references `ref0`, `ref1`, ... (instance fields per class, statics for class objects);
- writes primitive arrays with their length and a sparse run of zeros as data;
- carries each object's size from the dump (class instance size, PHD array size) in the record's
  stack trace serial, which the engine uses instead of the HotSpot layout rules;
- infers GC roots: classes are sticky-class roots, objects nothing references are unknown roots,
  and so is one object of each cycle still unreachable (OpenJ9 collects garbage before it writes a
  heap dump, so everything in it is live);
- marks the file with an `openprofiler:phd` string record, so a snapshot that embeds it reopens
  as a PHD.

Histogram, dominators, retained sizes, biggest objects, references and paths to roots work; string
values, primitive array contents and the inspections (which read field values) do not, and the UI
marks the session as PHD. The conversion holds the object graph in memory (about 60 bytes per
object and 8 per reference). Fixtures from OpenJ9 21 with and without compressed references are
in `crates/openprofiler-phd/tests/data` (`regenerate.sh` rebuilds them in Docker).

### 3.8 CPU and tree engine (`openprofiler-model`)

- Interned frames: method, line and frame type (interpreted, JIT, inlined, native, kernel).
- Aggregation by method, class or package; thread-state partitions.
- Derived views computed on demand: hot spots with back traces, inverted trees, collapsed recursion,
  flame layout, call graph, and diff trees for comparisons.

### 3.9 Remote and containers

- **Local discovery:** hsperfdata and `/proc`: JVM name, arguments, version, whether an agent is
  already loaded.
- **Attach:** HotSpot dynamic attach (`.attach_pid`, SIGQUIT, `.java_pid` socket). On Windows
  (x86-64 only) a named pipe is created and a small stub is injected into the JVM with
  `CreateRemoteThread`; it calls `jvm.dll`'s `JVM_EnqueueOperation`, and the JVM writes the reply
  to the pipe (the JDK's own mechanism). In containers:
  NSpid translation, `/proc/<pid>/root`, and copying the agent into the target's mount namespace.
- **`openprofiler-relay`** (static musl, about 1 MB) runs on the remote host or in the container:
  `relay <port>` (stdio ↔ TCP with a READY handshake), `list`, `attach`, `dump`, `pty-exec`.
- **Transports:** ssh (with jump hosts), `docker exec -i`, `kubectl exec -i`, and ephemeral debug
  containers (`kubectl debug --target --profile=general`, reused while running) for distroless
  images. The relay and agent bundle are uploaded through stdin with sha256 verification, so no
  port is ever exposed. The agent build (musl or glibc) follows the JVM's dynamic loader under
  `/proc/<pid>/root`, not the shell's. Running as root next to a non-root JVM, the relay takes the
  JVM's uid/gid for the attach handshake. Attaching again reuses an agent that is already loaded.
- **Clients:** the UI's Attach dialog (Local, SSH, Docker, Kubernetes tabs), CLI flags (`--ssh`,
  `--docker`, `--kube`, `--debug-container`) on `list`, `attach`, `record` and `dump`, and the MCP
  `list_jvms`/`attach` tools take the same target chain.
- **Bundle:** `scripts/build-bundle.sh` cross-builds the static relay and the musl and glibc
  (2.17+) agents for x86_64, aarch64 and armv7 with cargo-zigbuild; CI runs them under QEMU
  (`scripts/smoke-bundle.sh`) and attaches over ssh, docker and to a distroless pod in kind
  (`scripts/e2e-remote.sh`).

### 3.10 UI

- A left navigation (Overview, CPU, Memory, Heap, Threads, Locks, Probes, Events, MBeans, Code, Compare), a top
  session bar with recording controls and a timeline scrubber, and an inspector panel.
- The Code view (decompiler sessions only): a lazy package tree and search on the left, a
  virtualized code viewer with syntax highlighting, clickable references, find in file, Java and
  smali (or, for JVM classes, bytecode) tabs, back/forward history, and a usages panel; a Resources
  tab shows decoded resources (or a jar's other files).
- The Open dialog takes a path on the daemon's machine, recent paths, and a file browser over that
  machine's directories (`/fs/list`); on the desktop also the system file dialog.
- The Probes view: probes with totals; per probe, payload hot spots (selecting one shows its back
  traces and call tree), an events table (filter, errors only, request context), a timeline lane
  per probe, control objects with leak flags, and probe telemetry (calls per second, mean time).
  Live sessions edit the instrumentation (call-count patterns, enabled probes) there. Exact call
  counts appear as the `Calls` kind of the CPU views.
- The MBeans view (live sessions): a server picker (the platform MBean server, other MBean servers
  in the JVM, remote JMX servers the JVM connects to), the beans as a domain tree, attribute values
  (auto-refreshed, writable ones editable), operations with typed argument fields, notifications of
  subscribed beans, and numeric attributes plotted as telemetry series sampled by the agent.
- A global time range: brushing the timeline filters every view.
- A command palette (⌘K) for actions, views, classes and methods, with go-to-source through the IDE
  bridge.
- Flame graphs with search, zoom and differential colouring; a sunburst of the dominator tree; a
  reference graph; an Insights panel with severity and estimated waste.
- Web mode: `openprofiler daemon --web` serves the same UI with token auth. Read-only share links
  carry a token scoped to reads of one session.

### 3.11 MCP and automation

- `openprofiler mcp` runs an MCP server on stdio. Tools include `list_jvms`, `attach`,
  `prepare_profiling`, `load_snapshot`, `hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`,
  `compare`, `thread_dump` and the MBean tools (`list_mbean_servers`, `connect_jmx`,
  `list_mbeans`, `get_mbean`, `invoke_mbean`, `track_mbean`, `subscribe_mbean`,
  `mbean_notifications`), `probes` (list, hot spots, events, control objects, timeline, telemetry)
  and `instrumentation`; `decompile`, `search_code` and `find_usages` read decompiled apps. Responses carry stable node IDs and `suggested_next` hints.
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
  when debug info doesn't have one, and other locals get generic types from the program's
  fields, methods and the method's return type their values flow through (all must agree;
  `ArrayList` flowing into `List<String>` becomes `ArrayList<String>`); `new` of a generic class
  assigned to a generic target, or returned from a lambda or a method with a parameterized
  return type (constructors with only primitive arguments), prints a diamond. The low-level view of a JVM class is its
  original bytecode (`jvm/disasm.rs`) with offsets, resolved constants and linked member
  references. A directory of class files (`target/classes`) opens like a jar.
- **Recovered source constructs** (shared by both inputs, `structure.rs` and the printer):
  `finally` from the copies compilers inline before each exit plus the catch-all that rethrows
  (an exit whose value `F` could change is the shared code after the try, not the part);
  try-with-resources from javac's suppressing close handler (nested resources share one
  statement; `catch`/`finally` around it are hoisted); `switch` on strings from the
  `hashCode()` switch and the index switch (javac and d8 forms, hash collisions included);
  boxing and unboxing calls are dropped where the context converts implicitly, except for
  arguments of overloaded methods (`List.remove`) and `==` between two unboxed values;
  conditionals from branches assigning one variable, in source order (the fall-through arm comes
  first, under the negated branch condition) and chained through the else side
  (`a > 0 ? "pos" : a < 0 ? "neg" : "zero"`), keeping boxing explicit where a reference is
  expected and only one side boxes; casts of calls returning a type argument of a receiver with
  known generic type (`words.get(i)` of a `List<String>`; the program's signatures, or a table of
  JDK collection and functional-interface methods) are dropped, as are casts of
  `Stream.collect(Collectors...)` on a stream of known element type where the result is
  returned or assigned (the compiler infers it from the target).
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
| 3 Probes and instrumentation | Bytecode instrumentation, exact call counts, probe SPI, JDBC/HTTP/JPA/Kafka/gRPC probes, async context propagation, custom probes, version matrix against real libraries | Done; Quarkus CPU overhead open (#38) |
| 4 Remote and cloud | Relay, ssh/docker/kubectl, debug containers, verified upload, remote attach in UI/CLI/MCP, x86_64/aarch64/armhf bundle, web mode, read-only share links, CI gate | Done |
| 5 Polish and ecosystem | Native classfile rewriter (done in phase 3), native heap dumper, call tracer, MBean browser, IntelliJ plugin, Android and JVM decompiler, Windows agent, PHD import, Arrow IPC sections | Partial |

Open work is tracked in [GitHub issues](https://github.com/matusso/openprofiler/issues).

**Exit criteria**
- Phase 1: a 10 GB HPROF opens in under 3 min cold and under 5 s warm; retained sizes match MAT
  exactly on a reference corpus. *(met)*
- Phase 2: at most 2% overhead at 100 Hz async sampling on Renaissance/DaCapo; a 24 h soak without
  leaks or crashes.
- Phase 3: Spring Boot Petclinic and Quarkus demos show SQL and URL hot spots linked to call trees,
  at under 5% overhead. *(hot spots met; overhead met for throughput and latency, CPU per request
  met on Petclinic, not yet on Quarkus.)* `java/bench/linux.sh`, 8 closed-loop clients, 60 s,
  median of 3 rounds, Linux arm64 container on 18 cores, JDK 21, agent defaults ("probes") and
  with `instrument=` on the application packages:

  | app | config | req/s | p99 | CPU per request |
  |---|---|---|---|---|
  | Petclinic (Spring Boot 4.1, Hibernate 7, H2) | probes | −1.3% | +0.7% | −0.2% |
  | | + `instrument=` | −1.0% | +0.6% | +2.9% |
  | Quarkus 3.40 Hibernate ORM Panache quickstart (PostgreSQL) | probes | −3.4% | +4.9% | +24% |
  | | + `instrument=` | −2.4% | +3.2% | +25% |

  A Quarkus request costs about 0.1 ms of CPU, so the probes' fixed cost per request (HTTP,
  connection checkout, JPA, SQL, worker hand-off) shows: about 23 µs.
- Phase 4: attach to a JVM in a distroless pod on a remote cluster in three clicks or fewer, without
  exposing ports. *(met: Attach → Kubernetes → double-click the pod; `scripts/e2e-remote.sh`
  attaches to a non-root distroless pod in kind through a debug container.)*

## 5. Risks and mitigations

| Risk | Mitigation |
|---|---|
| `AsyncGetCallTrace` instability or removal | The sampler is abstracted; sync and JFR back ends exist or are planned. Track the JDK CPU-time JFR sampler (JDK 25+). |
| VMStructs fragility across JDK builds | Used only as an optional guard; feature-detected and disabled gracefully. JDK matrix in CI. |
| Bytecode instrumentation correctness | Injected code keeps the types at existing frames, so frames are remapped, not recomputed; a corpus test rewrites javac output with every hook and runs it under the verifier; methods with `jsr`/`ret`, overflowing branches or over 64 KB are skipped. |
| Probe breadth across library versions | Probe SPI, a version-matrix harness (`java/matrix/`, servers in containers), prioritise by usage. |
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
