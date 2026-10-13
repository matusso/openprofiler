# Plan for an open-source JVM profiler modeled on JProfiler 16.2.1, written in Rust with a modern UI

Working name: **Ember**, a placeholder. This plan has two parts:

- **Part A** is a reverse-engineering summary of how JProfiler 16.2.1 is built.
- **Parts B–F** are a clean-room plan for an open-source reimplementation in Rust with a modern UI.

> **Clean-room rule.** Use this document only for architectural and behavioural knowledge. Do not copy JProfiler code, class names (`__ejt_*`, `PM<N>Callee`, …), the `.jps` format, wire constants, icons or text. The spec in Parts B–F is new. Where we need prior art, use open projects: async-profiler, jattach, Eclipse MAT, JDK Mission Control, VisualVM, hprof-slurp.

---

## Part A: How JProfiler works

### A.1 Component map

```
┌─────────────────────────── Target JVM ────────────────────────────┐
│ libjprofilerti.so (C++, ~3.6 MB, static libstdc++, zstd, zlib, expat)│
│  • JVMTI agent: Agent_OnLoad / Agent_OnAttach                       │
│  • native bytecode rewriter (C++ port of jclasslib) in CFLH         │
│  • CPU sampler (SIGPROF+ITIMER_PROF+AsyncGetCallTrace / sync)       │
│  • call-tree store, heap dumper, snapshot (.jps) writer             │
│  • TCP server :8849 (SocketCommunicator), zstd streaming            │
│        ▲ JNI RegisterNatives (numeric table IDs)                    │
│        ▼ upcalls (vmInit, doSampling, getEventData, …)              │
│ agent.jar on the boot class path (Java, obfuscated)                 │
│  • "callee" classes: static entry points that injected code calls   │
│  • ~25 probes (JDBC, JPA, HTTP, Kafka, gRPC, Mongo, AI, …)          │
│  • triggers/actions, scripts, MBean browser, Controller API, JFR GC │
└─────────────────────────────▲──────────────────────────────────────┘
                              │ TCP (direct / SSH tunnel / jpnetcat over kubectl|docker exec)
┌─────────────────────────────┴──── GUI JVM (jprofiler.jar) ─────────┐
│ Swing + FlatLaf + JIDE grids + embedded yFiles (graphs)             │
│ comm client (~120 commands), lazy tree expansion, polling timers    │
│ heap-dump analysis engine (off-heap arrays, Lengauer–Tarjan)        │
│ HPROF / PHD / JFR (JMC fork) importers; .jps opened by loading      │
│   libjprofilerti *into the GUI* and talking to it over localhost    │
│ SSH (sshj), Docker/K8s attach, MCP server, IDE WebSocket server     │
└─────────────────────────────────────────────────────────────────────┘
CLI tools: jpenable, jpdump, jpcontroller (attach.jar) · jpexport, jpcompare, jpanalyze, jpmcp, jpintegrate (jprofiler.jar)
Helpers: jpnetcat (stdio↔TCP relay), jpspawnhelper (pty exec), libjphelper (X11 window focus), jpchromehost (Chrome native messaging)
```

### A.2 The native agent (`libjprofilerti.so`)

**Bootstrap.**
- The library locates `agent.jar` next to itself (`dladdr` + `realpath`) and adds it with `AddToBootstrapClassLoaderSearch`.
- It exports only one JNI symbol: `NativeAgentMethods.register0(Class,int)`. All other natives are bound with `RegisterNatives` using numeric class-table IDs, so the binding is independent of obfuscated names.

**Options.**
- Sources: `-agentpath:…=opts`, the `JPROFILER_AGENT_OPTIONS` env var, or the attach argument.
- Key options: `port=` (default 8849), `address=`, `loopback`, `forceIPv6`, `nowait`, `offline`, `config=`, `id=`, `sysprop=`, `debug=`, plus about 40 tuning flags.
- The session config is XML and is parsed natively with expat.

**JVMTI.**
- Capabilities are requested in independent groups, so the agent degrades gracefully when one group is refused: tagging, monitor info, thread CPU time, bytecodes/constant pool, all class hooks, VMObjectAlloc, SampledObjectAlloc, CompiledMethodLoad, native-method prefix, retransform, early vmstart, virtual threads.
- Events used: VMInit/Death, ThreadStart/End, ClassFileLoadHook, ClassLoad/Prepare, NativeMethodBind, ObjectFree, Monitor{Wait,Waited,ContendedEnter,ContendedEntered}, CompiledMethodLoad, DynamicCodeGenerated, VMObjectAlloc, SampledObjectAlloc, VirtualThreadStart/End.
- HotSpot extension events: `VirtualThreadMount/Unmount`.
- **Not used:** JVMTI GC events and MethodEntry/Exit. GC data comes from MXBeans and JFR `RecordingStream`. Method timing is done by bytecode injection.

**CPU sampling.**
- *Async mode:* `sigaction(SIGPROF)` + `setitimer(ITIMER_PROF)` + `AsyncGetCallTrace`, resolved with `dlsym`.
- Async mode also parses `gHotSpotVMStructs`, reading JavaThread state, frame anchors, CodeCache/CodeHeap bounds and `CollectedHeap::_is_gc_active`.
- It has its own ELF reader. That reader follows `.gnu_debuglink` and build-id to find non-exported libjvm symbols such as `JvmtiEnv::GetStackTrace(JavaThread*,…)`.
- Failed samples are counted by reason: GC active, not walkable, deopt, safepoint, and so on.
- *Sync mode:* `GetThreadListStackTraces` from a sampler thread.
- *Native stacks:* symbolized with the same ELF parser. `NativeLibraries.load` is intercepted so newly loaded libraries get re-symbolized.

**Instrumentation.**
- Bytecode is rewritten natively inside ClassFileLoadHook.
- Injected calls: `enterMethod(classId,methodIdx)` and `exitMethod` (with line-number variants), allocation hooks, exception hooks, `LockSupport.park` hooks, and probe interceptions.
- Native methods are wrapped with `SetNativeMethodPrefix`.
- Auto-tuning removes methods whose average or per-mille overhead is too high. This runs entirely natively.

**Allocation and heap.**
- Allocations come from three sources: injected allocation calls, VMObjectAlloc, and optionally SampledObjectAlloc.
- Recorded objects are tagged. ObjectFree provides the "live vs GC'd" counts.
- Heap snapshots use FollowReferences/IterateThroughHeap with class tags and are streamed to temp files. HPROF is the fallback.

**Communication.**
- One native comm thread runs a TCP server.
- Handshake: magic `0xCAFED00D`, then a protocol version (70), then ID size and endianness.
- Commands are framed with the length repeated three times, a CRC32 and ACK booleans. Streams use zstd. Large blobs travel via `sendfile` or local temp files.
- Updates are differential (incremental class/method/thread tables piggybacked on every reply).
- The agent supports reconnects. **There is no authentication**; security relies on the bind address and on tunnels.

**Snapshot (`.jps`).**
- The file has sections: header, monitors, live heap, allocation tree, payloads, CPU trees, classes, sampler data, config, appended heap dump, and a trailer.
- The trailer holds the section-table offset, the compressed-data offset and the heap-dump offset. The heap dump section is zstd-compressed.

**Container plumbing.**
- HotSpot attach: `.attach_pid` + SIGQUIT + the `.java_pid` UNIX socket.
- OpenJ9 attach: `ftok(…,0xa1)` semaphores.
- Containers: NSpid translation, `/proc/<pid>/root`, and an euid/egid switch.
- pty natives are used to spawn ssh, kubectl and docker.

### A.3 The Java agent (`agent.jar`)

- It is a boot-classpath library of upcall targets, not a `java.lang.instrument` agent.
- Hot-path pattern: injected code does `getstatic recordingFlag` and then calls a static callee. The callee passes a per-thread native "thread manager" pointer cached in a ThreadLocal.
- **Probes** (about 25 built-in): JDBC (drivers, pools, bind params, connection leaks), JPA/Hibernate/EclipseLink/OpenJPA, JMS, JNDI, Kafka producer/consumer, AI (Spring AI, LangChain4j, Jlama), HTTP client (JDK, OkHttp, Apache 4/5, Reactor Netty, WebFlux, Jersey, CXF), HTTP server (Servlet/Jakarta, Jetty, Netty HTTP/1+2), RMI, gRPC, JAX-WS, MongoDB, Cassandra, File, Socket, Process, ClassLoader, Exception, GC (JFR-based).
- **Async tracking** covers executors, ForkJoin, CompletableFuture, Vert.x, Kotlin coroutines and virtual threads. It works through generated wrapper classes that carry payload context.
- **Probe data model:**
  - *payloads*: strings such as SQL or URLs, interned as descriptors and attached to call-tree nodes, giving payload hot spots;
  - *events*: spooled to compressed temp files with a size cap;
  - *control objects*: e.g. connections, with an open/close lifecycle;
  - *telemetries*: summed per second;
  - *tracking*: plots selected hotspots or control objects.
- Library-specific helpers (`*ContextImpl`) are **defined into the application's class loader** so they can link against Hibernate, Kafka and other library types.
- **Custom probes:** three kinds.
  - Embedded API: empty stub methods that the agent intercepts.
  - Injected API: annotation-driven (`@Probe`, `@PayloadInterception`, `@MethodSpec`, `@Parameter`…).
  - Script probes: Java snippets compiled **in the GUI** and shipped as bytecode.
- **Triggers:** method, timer, CPU/heap threshold, OOM, JVM start, JVM stop. **Actions** (about 25): start/stop recordings, save snapshot, heap dump, thread dump, bookmark, run script, and others.
- **Remote control:** a Controller API (static no-op stubs that the agent intercepts) and a JMX `RemoteControllerMBean`, which is what `jpcontroller` drives.
- Thread dumps for virtual threads walk `jdk.internal.vm.ThreadContainers`. `j.u.c` lock owners are resolved through `AbstractOwnableSynchronizer.exclusiveOwnerThread`.

### A.4 The GUI and tools (`jprofiler.jar`, `platform.jar`, `attach.jar`)

**Views.**
- Telemetries: overview, heap, recorded objects, throughput, GC, classes, threads, CPU, plus custom/MBean telemetries and bookmarks.
- Live Memory: all objects, recorded objects, allocation tree and hot spots, class tracker.
- Heap Walker:
  - views: classes, allocations, biggest objects (dominator tree or sunburst), references (incoming, outgoing, merged), time, graph;
  - about 20 inspections: duplicate strings and arrays, sparse arrays, bad hash distribution, ThreadLocal, inner-class retention, and others.
- CPU: call tree, hot spots, call graph, outlier detection, complexity, call tracer, flame graph, async call tree, collapse recursion, cumulated outgoing calls.
- Threads: history, monitor, dumps with diff.
- Monitors & Locks: locking graphs, history, statistics.
- Probes: each has a timeline, control objects, call tree, hot spots, telemetries, events and leaks.
- Also: MBeans, JFR event browser, and comparisons (CPU, objects, allocations, probes, telemetry).

**Data flow.**
- The GUI fetches tree roots and expands nodes lazily, by node ID.
- Timers poll only the data kinds that open views subscribe to (CPU every N seconds, tables every M seconds). Duplicate pending commands are coalesced.

**Snapshots.**
- `.jps` files are opened by loading the native agent *inside the GUI JVM*. The GUI then talks to it over localhost with the live protocol, so live and offline sessions share one code path.
- HPROF, PHD and JFR are parsed in Java. JFR uses a JMC fork and is processed chunk by chunk.

**Heap engine.**
- The GUI runs with only `-Xmx1536m`, so the work is off-heap:
  - dense int object index over an address map;
  - outgoing CSR (`outgoing.dat/.off`) and incoming CSR (`backref.dat`) arrays;
  - iterative Lengauer–Tarjan over a synthetic super-root;
  - native retained-size computation and sort/binary search;
  - results persisted in a `<snapshot>.analysis/` directory, which `jpanalyze` can precompute.

**CLI tools.**

| Tool | Purpose |
|---|---|
| `jpenable` | Attach the agent. Has a hidden machine mode that prints XML for remote and container use. |
| `jpdump` | HPROF or PHD dump. |
| `jpcontroller` | JMX client. |
| `jpexport` | View export to HTML/CSV/XML. |
| `jpcompare` | Snapshot comparisons. |
| `jpanalyze` | Heap index precompute. |
| `jpmcp` | MCP server over stdio. |

**MCP tools:** `prepare_profiling`, `attach`, `list_jvms`, `check_status`, `load_snapshot` (with baseline diff), `get_performance_hotspots`, `expand_performance_hotspot`, `create_heap_dump`, `get_heap_data`.

**Remote.**
- SSH via sshj, multi-hop or OpenSSH.
- Zero-config remote attach: detect the platform, then upload or download the agent bundle, then run `jpenable` remotely, then tunnel.
- Docker and Kubernetes: `docker exec -i` / `kubectl exec -i`, stream the agent tarball in, and relay the agent socket through `jpnetcat` over the exec's stdio (`JPNETCAT_READY` handshake). The agent port is never exposed.

**IDE integration.** A WebSocket or framed-socket server (Kryo or protobuf) carrying navigation, profiled packages, session start and gutter call-graph requests.

### A.5 Takeaways that shape our design

1. **One query API for live data and saved data.** This is JProfiler's best architectural idea. We will implement it as a library and service boundary instead of loading the agent into the GUI.
2. **Analysis stays where the data is, and the UI fetches lazily.**
3. **Heap analysis must be out-of-core.** That means mmap'd CSR graphs and a persistent analysis cache.
4. **The in-JVM part must be native plus a thin boot jar.** Go is not suitable inside the JVM because of cgo overhead and SIGPROF conflicts with the Go runtime.
5. **The container story comes from a tiny static relay binary,** not from shipping a JVM launcher into the target.
6. **Gaps to improve on:**
   - no authentication or TLS on the agent port;
   - process-wide `ITIMER_PROF` instead of per-thread timers;
   - triple-length/CRC framing over TCP;
   - a proprietary snapshot format;
   - a Swing UI.

---

## Part B: Goals and scope

**Product goals**
- Feature-competitive with JProfiler's core: CPU, memory, heap walker, threads, locks, top probes, telemetries, snapshots, comparisons, remote, containers and MCP.
- **Faster:** open a 10 GB HPROF in under 3 minutes on a laptop with 16 GB RAM (cold), and in under 5 s on reopen (from the cache). The agent should add ≤ 2% overhead with async sampling.
- **Modern UX:** dark and light themes, a command palette, keyboard-first navigation, GPU-rendered flame graphs and timelines, a web-accessible UI, and shareable snapshots.
- **Open formats:** a documented snapshot format and protobuf schemas. HPROF and JFR import are first-class.
- Licence: Apache-2.0 (or MPL-2.0 for the agent).

**Non-goals (v1):** JRockit, Java < 11, IBM PHD (later), Windows agent (v1.x), JavaScript XHR via a browser extension, Eclipse/NetBeans plugins.

---

## Part C: Technology choices

| Layer | Choice | Why |
|---|---|---|
| In-JVM native agent | **Rust** `cdylib` (`jni-sys`, hand-written JVMTI bindings via `bindgen` from `jvmti.h`) | No runtime and no GC, signal-safe code is possible, and a static musl build is possible. Go is ruled out here. |
| In-JVM Java shim | Java 11 bytecode, about 5–10k LOC, boot classpath | Probe logic must touch app objects. Static callees keep the hot path cheap. |
| Bytecode rewriting | **Phase 1:** Java ASM inside the shim (`Instrumentation` via `Launcher-Agent`/JVMTI-loaded `instrument`). **Phase 2:** Rust classfile rewriter in CFLH. | ASM gets us correct StackMapTables quickly. The Rust rewriter removes the boot-time class-loading recursion and speeds up startup later. |
| Core analysis engine | **Rust** (`rayon`, `memmap2`, `zstd`, `prost`) | Heap graphs over 100M nodes, tree aggregation, diffs. |
| Daemon / query server | Rust (`tokio`, `tonic` gRPC + `axum` WebSocket) | One process serves the desktop UI, the web UI, the CLI and MCP. |
| UI | **Tauri 2 + SvelteKit or React + TypeScript**, canvas/WebGL renderers | Modern design, a native desktop shell, and the same UI works in a browser. |
| Graph layout | ELK (`elkjs`) in a web worker | Replaces yFiles. |
| Tables and trees | Custom virtualized tree-table (TanStack Virtual) | Millions of rows with lazy expansion. |
| Charts | uPlot (time series) + custom WebGL flame graph and sunburst | Very large sample counts. |
| Relay / attach helper | Rust static musl binary `ember-relay` (x64, aarch64, armhf) | Replaces jpenable machine mode, jpnetcat and jpspawnhelper. |
| CLI | Rust `clap` binary `ember` | attach, dump, record, export, compare, analyze, mcp. |

*Alternative:* write the daemon and CLI in Go. This is viable, but a single Rust workspace avoids two toolchains and lets the analysis crates be shared, so **we choose Rust everywhere except the Java shim and the TS UI.**

---

## Part D: Target architecture

```
                         ┌────────────── ember-ui (Tauri / browser) ──────────────┐
                         │ views · command palette · flame/sunburst · ELK graphs │
                         └───────────────▲──────────── WebSocket/gRPC-web ────────┘
                                         │  Query API v1 (protobuf)
 ┌──────────── ember-daemon (Rust) ──────┴──────────────────────────────────────┐
 │ SessionManager ─┬─ LiveSession  ◄── agent-link (TLS+token, zstd frames) ──────┼──► target JVM
 │                 ├─ SnapshotSession (.ember) ─┐                                │
 │                 ├─ HprofSession ─────────────┼─► ember-heap (mmap CSR, dom tree)│
 │                 └─ JfrSession ───────────────┘   ember-cpu (trees, hotspots)   │
 │ Exporters (HTML/CSV/JSON/SVG) · Compare · MCP server (stdio/HTTP) · IDE bridge │
 │ Remote: SSH (russh) · docker/kubectl exec · ember-relay upload & stdio tunnel  │
 └───────────────────────────────────────────────────────────────────────────────┘

 Target JVM:  libember_agent.so (Rust) ◄─JNI─► ember-boot.jar (Java callees + probes)
```

### D.1 Repository layout (Cargo workspace + Gradle + pnpm)

```
ember/
  crates/
    jvmti-sys/          # bindgen JVMTI/JNI + version-sized callback structs
    ember-agent/        # cdylib: Agent_OnLoad/OnAttach, sampler, CFLH, heap dump, agent-link
    ember-classfile/    # (phase 2) classfile parse/rewrite + StackMapTable regen
    ember-proto/        # protobuf schemas (agent-link + query API + snapshot sections)
    ember-model/        # call trees, symbol tables, telemetry series, probe events
    ember-snapshot/     # .ember container read/write
    ember-hprof/        # streaming HPROF parser (1.0.1/1.0.2, gz)
    ember-jfr/          # JFR chunk parser → ember-model
    ember-heap/         # object index, CSR graphs, dominators, retained, inspections, paths
    ember-attach/       # hsperfdata, HotSpot attach, OpenJ9 attach, ns/NSpid handling
    ember-remote/       # SSH, docker/kubectl exec, relay protocol
    ember-daemon/       # query server, session manager, exporters, MCP
    ember-cli/          # `ember` binary
    ember-relay/        # static musl helper (relay + attach machine mode + pty)
  java/ember-boot/      # boot jar: callees, probes, context propagation, controller API
  java/ember-api/       # public Controller + custom-probe annotations (Maven Central)
  ui/                   # Tauri app + web build
  docs/spec/            # agent-link, query API, snapshot format, config schema
```

### D.2 Native agent design (`ember-agent`)

**Options.** `-agentpath:libember_agent.so=port=0,bind=127.0.0.1,token=…,mode=live|offline,config=…,nowait`. Also the `EMBER_AGENT_OPTIONS` env var and attach arguments.
- `port=0` means an ephemeral port. The agent publishes the actual port through a hsperfdata counter (`ember.port`) and a `$TMPDIR/ember-<pid>.json` file.

**Capabilities.** Request them in groups and record which were granted. Size the callback struct per JDK.

**CPU sampling.**
1. **Async (default, Linux):** a per-thread `timer_create(CLOCK_THREAD_CPUTIME_ID, SIGEV_THREAD_ID)` for CPU time, plus a wall-clock mode that sends a signal to sampled threads at intervals.
   - In the handler, call `AsyncGetCallTrace` and copy frames into a lock-free per-CPU ring buffer.
   - A drainer thread converts the samples to `(threadId, state, stack)` and aggregates them into the call tree.
   - Use VMStructs only for robustness checks (GC active, walkable). Start with the minimum set; async-profiler is the reference.
2. **Sync fallback:** `GetThreadListStackTraces`, used on macOS and when async sampling is unavailable.
3. **JFR mode (zero-native fallback):** start JFR `ExecutionSample` and stream it. Useful for locked-down environments.
4. Native frames: symbolize with the `object` + `gimli` crates; JIT frames come through jmethodID preload on CompiledMethodLoad.
5. Virtual threads: use the mount/unmount extension events and aggregate per carrier, with a "mounted-only" mode.

**Instrumentation mode** (exact call counts): enter/exit hooks injected into filtered classes only, with auto-tune that disables hot trivial methods. This is phase 2. Async sampling covers about 90% of use cases.

**Allocation:** `SampledObjectAlloc` + `SetHeapSamplingInterval` (default 512 KB), stack capture, and tags with ObjectFree for live and GC'd counts. Exact recording through instrumentation is optional and comes later.

**Monitors:** JVMTI contended-enter and wait events with thresholds. `j.u.c` locks come from Java shim hooks on `LockSupport.park`.

**Heap dump:** the default is `HotSpotDiagnosticMXBean.dumpHeap` (HPROF) streamed back to the daemon. Phase 3 adds a native FollowReferences dumper that writes directly to the `.ember` heap section, which allows "recorded objects only" dumps.

**Telemetry:** a once-per-second sampler on the Java side (MXBeans and JFR GC events) pushes compact records over JNI into a native time-series ring.

**Aggregation in-agent:** a call-tree trie with interned frames, plus incremental symbol tables. The agent sends deltas only: new nodes and changed counters since a watermark.

**Offline mode:** write the `.ember` snapshot locally on triggers or at exit. This is the same code that serializes the live state.

### D.3 Java shim (`ember-boot.jar`)

- Static callee classes. The hot path is a `getstatic` flag check followed by a JNI call with a cached thread handle.
- Probe SPI: a `Probe` interface with `interceptions()`, `onEnter/onExit`, payload/event/control-object/telemetry emitters. Built-in probes are ordinary SPI implementations, which keeps the core small.
- Class-loader bridging: define library-specific helpers into the app's loader (MethodHandles `Lookup.defineClass` or the JVMTI-assisted path). Keep them version-sharded: Hibernate 5/6, Jakarta vs javax, and so on.
- Context propagation library for executors, CompletableFuture, Reactor, Kotlin coroutines and virtual threads.
- Public API (`ember-api`): `Ember.startCpuRecording()`, `saveSnapshot(Path)`, `bookmark(String)`, an annotation-driven custom-probe API (our own names), and `@Telemetry` methods.
- Triggers and actions are evaluated in-agent. Script triggers use **compiled-on-daemon** Java via `javac` in a sandboxed subprocess and are shipped as bytecode. An optional JEXL-free expression language covers simple conditions.

### D.4 Agent-link protocol (agent ↔ daemon)

- Transport: TCP. **TLS 1.3 by default** (rustls, self-signed with pinned fingerprint), or plain TCP over loopback/relay with a **mandatory random token**. Unix domain sockets are also supported locally.
- Framing: `u32 len | u16 type | u8 flags(zstd?) | payload(protobuf)`. There is no CRC; TCP and TLS already guarantee integrity.
- Handshake: `Hello{protocol_semver, agent_version, jvm_info, capabilities[], token}`, then `Welcome{session_cfg}`.
- Data plane:
  - *Push streams with backpressure:* `TelemetryBatch`, `SymbolDelta`, `CallTreeDelta`, `AllocTreeDelta`, `ProbeEventBatch`, `ThreadStateBatch`, `MonitorEventBatch`.
  - *Request/response:* `ThreadDump`, `HeapDump{format}`, `MBeanList/Get/Invoke`, `SetRecording{kind,on,opts}`, `UpdateConfig`, `SaveSnapshot`.
  - Push replaces JProfiler's GUI polling, and the daemon owns the aggregate state. This is the key difference: the agent stays lean, and the daemon serves the UI lazily.
- Bulk transfer: chunked `BlobChunk` frames, with a local fast path through a shared temp file when the daemon and the JVM share a filesystem namespace.
- Reconnect: the agent keeps aggregates and resends from the last acknowledged watermark.

### D.5 Query API v1 (daemon ↔ UI/CLI/MCP)

This API is the **single contract** used by the UI, the CLI exporter, MCP and the IDE bridge. It works identically for live, `.ember`, HPROF and JFR sessions.

- `ListSessions / OpenFile / Attach / Launch / Close`
- `Telemetry{kinds[], range, resolution}` returns downsampled series (LTTB).
- `Tree{kind: cpu|alloc|probe_ct, view: calltree|hotspots|backtraces|flame, filters, thread_sel, state_sel, aggregation: method|class|package, node_id?, depth}` returns lazily expandable nodes with stable IDs.
- `Flame{…}` returns a pre-laid-out rectangle buffer, as a binary column format for WebGL.
- `Heap.*`:
  - `Classes`, `Biggest{node}`, `Refs{objset, dir, merged}`, `PathToRoots`, `Inspect{kind}`, `Histogram{diff_with?}`
  - `ObjectSet` ops: select by class, retained, instances, field value
  - `Instance{fields}`, `StringValue`
- `Threads{history range}`, `ThreadDumps`, `Monitors{current|history|stats}`
- `Probe{id, view: events|hotspots|timeline|control_objects|telemetry}`
- `Compare{a, b, kind}` returns diff trees and histograms.
- `Export{view, format: html|csv|json|svg|png}`
- Subscriptions: the UI subscribes per visible view, and the daemon pushes invalidations (the "something changed" signal) rather than data. The UI then refetches only the visible window.

### D.6 Snapshot format (`.ember`), our own and documented

```
[Header]  magic "EMBRSNAP" | u16 major | u16 minor | flags | created | jvm_info(protobuf)
[Section]* each: u32 kind | u64 len | u8 codec(zstd) | payload
   kinds: Symbols, CpuTree(per thread-group/state), AllocTree, Telemetry(columnar),
          ThreadHistory, Monitors, ProbeMeta, ProbeEvents(columnar, chunked),
          Bookmarks, Config, HeapDump(HPROF embedded, zstd seekable frames), Extensions
[Index]   protobuf: section table with offsets, checksums (xxh3)
[Trailer] u64 index_offset | magic
```

- Every section is independently compressed and seekable (zstd seekable format), so files open without a full decompress.
- Columnar event and telemetry sections use Arrow IPC, which allows DuckDB/Polars to query snapshots externally. That is an exclusive feature.
- The heap analysis cache lives next to the file (`<file>.ember-idx/`), keyed by content hash and engine version.

### D.7 Heap analysis engine (`ember-heap`): the performance centrepiece

1. **Pass 1 (parallel streaming over mmap'd HPROF):** split HEAP_DUMP_SEGMENTs across rayon workers. Emit `(addr, class, shallow_size)` and the raw reference lists into spill files.
2. **Index:** radix-sort the addresses into a dense `u32` id with a sorted `addr[]` array (binary search) or an Elias-Fano structure. This takes about 4–8 B per object.
3. **Graph:** outgoing CSR (`offsets: u64[]`, `targets: u32[]`), built in parallel. The incoming CSR is built by counting sort. Both are mmap'd files.
4. **Roots:** a synthetic super-root over GC roots (JNI globals and locals, stack locals, sticky classes, thread objects…). Weak, soft and phantom handling is configurable: exclude the referent field of `java.lang.ref.Reference` subclasses.
5. **Dominators:** **Semi-NCA** (iterative, cache-friendly) with Lengauer–Tarjan as reference and test oracle. Memory is about 5 × 4 B per node.
6. **Retained sizes:** reverse-postorder accumulation; parallelizable by subtree.
7. **Persist:** `dom.idom`, `dom.children` CSR, `retained`, and class aggregates. Reopen is an mmap plus a header check.
8. **Inspections:** duplicate strings (Latin-1/UTF16 aware hashing), duplicate arrays, sparse and empty collections, bad HashMap distribution, ThreadLocal leaks, inner-class retention, and classloader leaks (classes retained by a dead loader).
9. **Paths:** BFS to roots on the incoming CSR with root-type preference and an exclude-weak option. A path between two objects uses bidirectional BFS.
10. **Benchmarks** (CI-tracked): 1 GB, 10 GB and 50 GB synthetic plus real dumps. Compare against Eclipse MAT and JProfiler timings.

Memory budget: about 32 B per object across all indexes. That means 200M objects is about 6.4 GB on disk, mmap'd, so RAM is not required.

### D.8 CPU and tree engine (`ember-cpu` in `ember-model`)

- An interned frame table: method, line and frame type (interpreted, JIT, inlined, native, kernel).
- The per-thread trie is stored in arena vectors. Aggregation levels are method, class and package. Thread-state partitions are runnable, waiting, blocked and net-I/O.
- Derived views are computed on demand and memoized per (filter, selection):
  - hot spots (self time, with back traces);
  - inverted trees;
  - collapse recursion;
  - cumulated outgoing calls;
  - flame layout;
  - call graph (method-level edge weights);
  - outlier detection (per-call-site max vs average; this needs instrumentation or call-tracer data).
- Diff trees for comparisons: align by frame path and report Δ self and Δ total.

### D.9 Remote and containers (`ember-attach`, `ember-remote`, `ember-relay`)

- **Local discovery:** parse hsperfdata (the format is public via jvmstat) and `/proc`. Show the JVM name, args, version, whether it is already profiled, and whether dynamic agent loading is disabled.
- **Attach:**
  - HotSpot: `.attach_pid` + SIGQUIT + `.java_pid` socket, `load <abs> false <opts>`.
  - OpenJ9: file-and-semaphore protocol.
  - Containers: NSpid translation, `/proc/<pid>/root/tmp`, euid/egid switch, and copying the agent into the target's mount namespace when needed. jattach is the reference.
- **ember-relay** (static musl, about 1 MB) runs in the remote host or container. It provides:
  - `relay <port>`: stdio↔TCP relay with a READY handshake;
  - `list`: JSON list of JVMs;
  - `attach <pid> <opts>`;
  - `dump <pid>`: streams HPROF to stdout;
  - `pty-exec`.
- **Transport chains:**
  - SSH (`russh`, ProxyJump, agent forwarding, or an external OpenSSH);
  - `docker exec -i` and `kubectl exec -i` (local or over SSH);
  - Upload the relay and agent bundle through stdin (`cat > /tmp/ember/…`), with sha256 verification.
- Kubernetes UX: a context, namespace and pod picker, ephemeral debug containers (`kubectl debug --target`) for distroless images, and a container picker.

### D.10 UI and UX (modern design)

- **Shell:** a left nav (Overview, CPU, Memory, Heap, Threads, Locks, Probes, Events, MBeans, Compare), a top session bar with recording toggles and a timeline scrubber, and a right inspector panel.
- **Global time range:** brushing the telemetry timeline filters every view to that window. JProfiler only does this partially; we make it universal.
- **Command palette** (⌘K) for all actions, views and classes or methods, with "go to source" (IDE bridge).
- **Flame graphs:** WebGL rendering, icicle or flame orientation, differential flame (red/blue), search highlight, zoom with breadcrumb, and a sandwich view (callers and callees of a selected frame).
- **Heap:**
  - sunburst and treemap of the dominator tree;
  - object-set "selection steps" shown as a breadcrumb trail;
  - reference graph with ELK layout, filterable by root type;
  - inspections presented as an "Insights" panel with severity and estimated wasted bytes.
- **Insights engine:** rule-based findings shown first. Examples: "N duplicate strings waste X MB", "Thread pool starvation", "N+1 JDBC queries in /orders", "Lock contention hot spot".
- **Probes:** a unified events table with a columnar back end (filter, group, histogram), a timeline lane per probe, and payload hot spots linked to call trees.
- **Theming and accessibility:** light, dark and high-contrast themes; keyboard navigation everywhere; screen-reader labels on tables; colour-blind-safe palettes.
- **Web mode:** `ember daemon --web` serves the same UI with token auth. Useful on headless servers and in CI.

### D.11 MCP and automation

- `ember mcp` (stdio) and an HTTP/SSE transport.
- Tools at parity with JProfiler plus extras:
  - parity: `list_jvms`, `attach`, `prepare_profiling`, `status`, `load_snapshot`, `hotspots`, `expand`, `heap_dump`, `heap_data`;
  - extras: `insights`, `compare`, `probe_events`, `thread_dump`.
- The JSON responses carry stable node IDs and `suggested_next` hints.
- CLI: `ember record --pid 1234 --duration 60s --cpu --alloc --probes jdbc,http -o out.ember` and `ember export out.ember --view hotspots --format csv`.
- CI mode: `ember compare base.ember head.ember --fail-if "cpu.total > +10%"` (a performance regression gate).

### D.12 IDE bridge

- A WebSocket JSON-RPC on localhost with a token. Messages: `navigate(class, method, line)`, `profiledPackages`, `startSession(launchConfig)`, `gutterData(file)`.
- Plugins: an IntelliJ plugin (Kotlin) and a VS Code extension (TS). Both are thin wrappers that launch `ember` with `-agentpath`.

---

## Part E: Phased roadmap

Estimates assume about 3 experienced engineers (Rust/JVM, Rust/data, TS/UI).

### Phase 0: Foundations (weeks 1–4)
- Workspace, CI (Linux x64/aarch64, macOS), `jvmti-sys`, protobuf specs v0, and the `.ember` container spec.
- Test-JVM matrix harness: JDK 11, 17, 21 and 25; HotSpot, OpenJ9, GraalVM JIT; Alpine (musl).
- **Exit:** the agent loads, prints a JVM inventory and sends a Hello to a stub daemon.

### Phase 1: Offline analyzer MVP, no agent yet (weeks 3–12)
*Ships value early and exercises the heaviest engine first.*
- `ember-hprof` + `ember-heap`: index, CSR, Semi-NCA, retained sizes, persistent cache, classes, biggest objects, refs, path-to-root, duplicate strings, histogram diff.
- `ember-jfr`: CPU samples, allocation samples, GC, threads and telemetry, mapped into `ember-model`.
- Daemon Query API (subset), Tauri UI (heap views, flame graph, call tree, hot spots, telemetry charts), CLI `ember open|export|analyze`.
- MCP v1 over files: `load_snapshot`, `heap_data`, `hotspots`, `expand`, `compare`.
- **Exit:** a 10 GB HPROF opens in under 3 min cold and under 5 s warm. Results match MAT on retained sizes for a reference dump corpus (exact match).

### Phase 2: Live CPU, memory and telemetry agent (weeks 10–24)
- `ember-agent`: async and sync sampling, thread states, telemetry, SampledObjectAlloc, monitor events, heap dump via MXBean, offline mode with `.ember` writing.
- `ember-boot.jar`: Controller API, telemetry sampler, `j.u.c` lock hooks, virtual-thread dumps.
- Agent-link with TLS and token, the daemon's live session, delta streams, reconnect.
- Local attach (`ember-attach`), launch sessions, recording profiles, triggers (timer, threshold, OOM, exit).
- UI: live telemetry overview, recording controls, threads (history, dumps, diff), monitors and locks, live memory (all objects and allocation hot spots).
- **Exit:** overhead ≤ 2% at 100 Hz async sampling on Renaissance/DaCapo benchmarks. A 24 h soak shows no leaks or crashes.

### Phase 3: Probes and instrumentation (weeks 20–36)
- Instrumentation framework: ASM-based in the shim, filters, and exact call counts with auto-tune.
- Probe SPI plus the top probes: JDBC (with bind params and leak detection), HTTP server (Servlet/Jakarta, Netty), HTTP client (JDK, OkHttp, Apache), JPA/Hibernate, Kafka, gRPC, exceptions, files and sockets. Async context propagation (executors, CF, coroutines, Reactor).
- Probe UI: events table, timeline, payload hot spots ↔ call tree, control objects, telemetry.
- Method triggers, custom-probe annotation API, split methods (URL splitting), outlier detection, complexity analysis.
- **Exit:** a Spring Boot Petclinic and Quarkus demo show SQL and URL hotspots that link to call trees, at under 5% overhead.

### Phase 4: Remote and cloud (weeks 30–42)
- `ember-relay` for musl x64, aarch64 and armhf; SSH (multi-hop); Docker and Kubernetes attach with ephemeral debug containers; zero-config agent upload; remote heap dump streaming.
- Web mode for the daemon, shareable read-only snapshot links, and a CI regression gate.
- **Exit:** attach to a JVM in a distroless pod on a remote cluster in 3 clicks or fewer, without exposing ports.

### Phase 5: Polish and ecosystem (weeks 40–52, ongoing)
- Native Rust classfile rewriter (removes the ASM dependency at boot), native heap dumper with recorded-objects-only dumps, call tracer, MBean browser, insights engine v2, IDE plugins, Windows and macOS agent parity, PHD import, more probes (Mongo, Cassandra, JMS, AI/LLM, RMI, WS).

---

## Part F: Risks and mitigations

| Risk | Mitigation |
|---|---|
| AsyncGetCallTrace instability or removal (JEP 435 discussions) | Abstract the sampler. Keep a JFR-streaming back end and a sync fallback. Track the JDK `jdk.internal`/JFR CPU-time sampler (JDK 25+) as a future primary. |
| VMStructs fragility across JDK builds | Use VMStructs only as an optional guard. Feature-detect and disable gracefully. Run the JDK matrix in CI nightly. |
| Bytecode instrumentation correctness (StackMapTable, JSR/RET, huge methods) | Start with ASM (`COMPUTE_FRAMES` with a safe class-loader-aware `getCommonSuperClass`). Fuzz with the JDK class corpus and verify with `-Xverify`. Skip methods over 64 KB. |
| Probe breadth (many library versions) | Probe SPI plus a community contribution path. A version-matrix test harness using Testcontainers. Prioritize by usage. |
| Heap analysis memory on huge dumps | Out-of-core design from day 1. Disk-backed CSR. Benchmark gates in CI. |
| Signal handler conflicts with app or native libs | Chain prior handlers. Make the signal configurable. Detect other profilers (async-profiler) and warn. |
| Container security policies (no ptrace, read-only /tmp, non-root) | Ephemeral debug containers, configurable writable dir, impersonation flow, JFR-only mode. |
| Legal: proximity to a commercial product | Clean-room process: this RE summary is the only input. Implementers do not consult decompiled code. All formats, protocols and names are new. Do not ship the vendor's hardcoded keys, assets or text. |
| UI performance with millions of nodes | Server-side lazy trees, virtualized rendering, WebGL for flame and timeline, LTTB downsampling. |

---

## Appendix 1: Feature parity checklist

| Area | JProfiler feature | Ember phase |
|---|---|---|
| Telemetry | heap, GC, classes, threads, CPU, throughput, custom/MBean, bookmarks | P1 (JFR) / P2 |
| CPU | call tree, hot spots, back traces, flame, call graph | P1 / P2 |
| CPU | async call tree, collapse recursion, cumulated outgoing | P2 |
| CPU | instrumentation counts, outlier detection, complexity, call tracer | P3 / P5 |
| Memory | all objects, recorded objects, allocation tree and hot spots, class tracker | P2 |
| Heap | classes, biggest/dominators, refs (merged), path to root, inspections, graph, time | P1 |
| Heap | heap-walker scripts and filters | P5 (expression language) |
| Threads | history, monitor, dumps and diff, virtual threads | P2 |
| Locks | current and history locking graph, monitor stats | P2 |
| Probes | JDBC, JPA, HTTP server and client, Kafka, gRPC, exceptions, file, socket | P3 |
| Probes | JMS, JNDI, RMI, WS, Mongo, Cassandra, AI, process, classloader, GC | P5 |
| Custom | embedded and injected probe APIs, script probes, splits, triggers and actions | P3 |
| Snapshots | save, open, compare (CPU, alloc, objects, probes, telemetry), export HTML/CSV/XML | P1–P3 |
| Imports | HPROF, JFR, PHD | P1 / P5 |
| Remote | SSH, zero-config upload, Docker, Kubernetes | P4 |
| Automation | offline mode, Controller API, CLI tools, MCP server | P1–P2 |
| IDE | IntelliJ, VS Code, Eclipse | P5 (Eclipse: community) |

## Appendix 2: Open prior art to study (licence-compatible)
- **async-profiler** (Apache-2.0): AsyncGetCallTrace, perf_events, VMStructs, wall-clock sampling.
- **jattach** (Apache-2.0): HotSpot and OpenJ9 attach, including in containers.
- **Eclipse MAT** (EPL): dominator and inspection semantics, used as a correctness oracle.
- **JDK Mission Control** (UPL): JFR semantics. **hprof-slurp** (Rust): streaming HPROF parsing.
- **Pyroscope / speedscope**: flame graph UX. **elkjs**: graph layout.
