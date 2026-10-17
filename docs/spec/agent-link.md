# OpenProfiler agent-link protocol v1

The agent-link protocol connects the in-JVM agent (`libopenprofiler_agent`, the **server**) to
`openprofiler-daemon` (the **client**). The schema lives in
[`crates/openprofiler-proto/proto/agent_link.proto`](../../crates/openprofiler-proto/proto/agent_link.proto);
the daemon-side implementation is `crates/openprofiler-link`.

`Hello.protocol_semver` is `1.1.0`. Minor versions only add fields and messages (1.1: call tracer,
call-site and complexity batches, `SetRecording` trace limits, complexity settings, native and
recorded-only heap dumps); clients ignore what they do not know.

## Transport

- TCP, one daemon connection at a time. The agent binds `bind` (default `127.0.0.1`) on `port`
  (default `0` = ephemeral).
- **Authentication is mandatory**: every connection must present the agent's token. If the
  `token=` option is not given, the agent generates a random 128-bit token.
- **TLS 1.3** with the `tls` option. The agent generates a self-signed certificate (ECDSA P-256)
  at start-up; the client pins its SHA-256 fingerprint (published in the info file). TLS 1.2 and
  other certificates are rejected.
- Over a relay (`openprofiler-relay`, `docker exec`, `kubectl exec`) the link runs in plain TCP on
  loopback inside the target namespace. The token still applies.

### Info file

After binding, the agent publishes `$TMPDIR/openprofiler-<pid>.json` (or the `infofile=` path),
mode `0600`, written atomically:

```json
{"pid": 4711, "port": 50123, "bind": "127.0.0.1", "token": "…", "tls": true,
 "fingerprint": "ab12…", "agent_version": "0.1.0", "instance_id": "1234567890"}
```

The file is removed at VM death.

## Framing

```
u32 len (LE) | u16 type (LE) | u8 flags | payload[len]
```

- `len` is the payload length as sent (after compression), ≤ 256 MiB.
- `flags` bit 0: payload is zstd-compressed. Senders compress payloads over 4 KiB when that makes
  them smaller.
- `payload` is one protobuf message, selected by `type`:

| type | message | direction |
|---:|---|---|
| 1 | `Hello` | agent → daemon |
| 2 | `Auth` | daemon → agent |
| 3 | `Welcome` | agent → daemon |
| 4 | `Resume` | daemon → agent |
| 5 | `DataBatch` | agent → daemon |
| 6 | `Ack` | daemon → agent |
| 7 | `Request` | daemon → agent |
| 8 | `Response` | agent → daemon |
| 9 | `BlobChunk` | agent → daemon |
| 10 | `Error` | either |
| 11 | `Ping` | daemon → agent |
| 12 | `Pong` (a `Ping` message) | agent → daemon |
| 13 | `RecordingHeader` | recordings only |

There is no CRC: TCP and TLS already guarantee integrity.

## Session lifecycle

```
agent                         daemon
  ── Hello ───────────────────▶   protocol version, JVM info, granted capabilities, instance id
  ◀────────────────── Auth ────   token (constant-time compared; mismatch → Error{fatal}, close)
  ◀──────────────── Resume ────   instance id + watermark the daemon already holds (zero if new)
  ── Welcome ─────────────────▶   current SessionConfig + resume_from
  ── DataBatch ───────────────▶   every 200 ms when something changed
  ◀─────────────────── Ack ────   after applying each batch
  ◀─────────────── Request ────   at any time
  ── Response [BlobChunk*] ───▶
  ◀────────────────── Ping ────   every 5 s (liveness)
```

The handshake must complete within 15 s.

## Data plane

The agent aggregates in-process and **pushes deltas**. The daemon owns the aggregate state and
serves clients lazily. The agent's state is a set of append-only tables:

| table | contents |
|---|---|
| strings, methods, frames | `openprofiler_model::Symbols` (string 0 is `""`) |
| stacks | global stack trie (node 0 = empty stack) |
| threads | `ThreadInfo` rows (index = thread id used by all other tables) |
| samples | per-second pre-aggregated rows `(time, kind, thread, state, stack, weight, count, class)` |
| telemetry | `(series, time, value)` rows; series are declared by the Java telemetry sampler |
| thread_states | closed state spans `(thread, start, end, state)`; open spans are cut once per second |
| monitors, gc, bookmarks | event rows |
| thread_dumps | dumps the agent took itself (trigger actions); dropped once acknowledged |
| probes | registered probes (`ProbeInfo`, positional; never dropped) |
| probe_calls | per-second aggregated probe calls `(time, probe, payload, thread, stack, count, duration, max, errors)` |
| probe_events, probe_controls | individual probe events and control-object open/close rows |

A `Watermark` holds one absolute position per table. A `DataBatch` carries everything in
`[from, to)`. The daemon applies tables in order (symbols, stacks, threads, then the rest) and
interns them in the same order, so the agent's ids equal the daemon's ids. `ThreadInfoBatch.updates`
carry later changes to existing threads (end time, rename).

Sample kinds (`openprofiler_model::SampleKind`): `0 cpu`, `1 wall`, `2 alloc`, `3 monitor`, `4 wait`,
`5 io`, `6 exception`, `7 calls`, `8 probe` (never sent; the daemon derives it from probe calls). Thread states: `0 runnable`, `1 waiting`, `2 blocked`, `3 net_io`,
`4 sleeping`, `5 new`, `6 terminated`, `7 unknown`.

Weights:
- `cpu`: nanoseconds of CPU time (async sampler: the timer interval; sync sampler: the thread's
  measured CPU-time delta, capped at 2× the interval).
- `wall`: the wall-clock interval.
- `alloc`: unbiased estimate of the allocated bytes a sample represents
  (`size / (1 - e^(-size/interval))`); `class_id` is the allocated class.
- `monitor` / `wait`: blocked/waiting nanoseconds; `class_id` is the monitor's class (for parks,
  the blocker's class).
- `calls`: self nanoseconds of instrumented methods; `count` is the number of invocations of the
  stack's leaf method along that path (see [Instrumentation](#instrumentation)).
- `probe` (derived): nanoseconds of probe calls; `count` is calls; the class column is the payload.

### Monitor events

`MonitorEvent.kind` is `0` enter (contended `synchronized`), `1` wait (`Object.wait`) or `2` park
(`java.util.concurrent`). Only events of at least `monitor_threshold` are sent individually, at
most 1000 per second each for monitors and for parks.

Contention can be very frequent (a hot `synchronized` cache in a web application contends tens of
thousands of times a second), so recording it must not make it worse:

- Stacks are only taken at the start of a contended enter or wait, before the thread blocks:
  when a sample is due (see below) and otherwise up to 5000 times a second in case the event turns
  out long. `MonitorContendedEntered` runs after the thread acquired the monitor, so nothing there
  walks the stack or takes a lock that other threads hold for long; raw frames are queued and
  resolved by the sampler thread.
- Events of at least `monitor_threshold` that have a stack are recorded with their duration. The
  time of the other events is sampled: once a thread's summed time of them reaches
  `monitor_interval`, its next event with a stack is recorded as a sample of that weight. Totals
  per stack are estimates for these, like allocation samples.
- Parks shorter than `monitor_threshold` are not recorded; longer ones beyond the budget are
  sampled the same way. Stacks for samples are taken at most 5000 times a second; time beyond
  carries over to the next sample.
- Even with nothing to do, JVMTI's delivery of contended-enter events costs a few percent of
  throughput at tens of thousands of contended enters a second. Above 20,000 a second the agent
  switches them on for one second in five, and contended-enter samples recorded meanwhile weigh
  five times their duration. Monitor waits and parks are not affected.

- `owner` (thread index + 1) is the thread holding the lock. For contended monitors it comes from
  JVMTI `GetObjectMonitorUsage`, which runs as a safepoint operation, so at most `owner_rate`
  lookups happen per second; later events in the same second have no owner. For parks it is the
  blocker's `AbstractOwnableSynchronizer.exclusiveOwnerThread` (e.g. a `ReentrantLock`'s holder),
  read just before the thread parks.
- `monitor_id` is the identity hash of the monitor or blocker object, so events on the same lock
  can be grouped.
- Parks come from a wrapper around the `Unsafe.park` native (installed through JNI
  `RegisterNatives` from a boot-class frame; the original entry is located through VMStructs) and,
  for virtual threads, from unmount/mount pairs. A park with an owner is recorded as `monitor`
  samples in state `blocked`; one without (conditions, queues, latches, idle pools) as `wait`
  samples in state `waiting`. Parks without a blocker object (bare `LockSupport.park()`, a virtual
  thread's sleep or I/O wait) are not reported.
- Owners only exist for exclusive `AbstractOwnableSynchronizer`s; `Semaphore`, `CountDownLatch` and
  read locks have none.

### Virtual threads

On JDK 21+ the agent requests `can_support_virtual_threads` and subscribes to HotSpot's
`VirtualThreadMount`/`VirtualThreadUnmount` extension events (`vthreads=0` turns this off).

- A virtual thread gets a thread row (`is_virtual`) on its first mount; after 4096 distinct
  virtual threads the rest share one row named `[other virtual threads]`. Unnamed virtual threads
  are called `virtual-<id>`.
- CPU samples taken on a carrier while a virtual thread is mounted belong to the virtual thread
  (async sampler). Work on the carrier outside a mount (scheduling, mount transitions) stays on
  the carrier.
- The wall-clock tick records each virtual thread's state: mounted = `runnable`,
  unmounted = `waiting`.
- An unmount on a blocker followed by a mount at least `monitor_threshold` later is a park event
  (see above).

Mount tracking and park recording run on every virtual-thread switch and long park. In a
microbenchmark where 500 virtual threads switched about 40,000 times per second, each cost about
5–8% of its throughput; `vthreads=0` and `monitors=0` turn them off.

### Acknowledgement, retention and reconnect

- In live mode, when a table holds more than 200 k rows, rows below the last acknowledged
  watermark are dropped. Symbols, stacks and threads are never dropped.
- On reconnect the daemon sends `Resume{instance_id, watermark}`. If the instance id matches, the
  agent resumes from that watermark (clamped to what it still retains); otherwise it starts from
  the oldest retained data and the daemon starts a fresh profile.

## Requests

| request | effect | response |
|---|---|---|
| `ThreadDump` | `ThreadMXBean.dumpAllThreads` (+ virtual threads via `HotSpotDiagnosticMXBean.dumpThreads` on JDK 21+) | `ThreadDump` |
| `HeapDump{format, live_only, local_path, dumper, recorded_only}` | `HotSpotDiagnosticMXBean.dumpHeap`; with `dumper: "native"` or `recorded_only` a JVMTI heap walk in the agent (capability `native_heap_dump`; reachable objects only; `recorded_only` keeps the objects sampled by allocation recording and their paths from the GC roots) | `HeapDumpResult`; without `local_path` the HPROF follows as `BlobChunk`s (1 MiB, `last` on the final one) |
| `SetRecording{kind, enabled, interval_us, duration_ms, max_events}` | toggles `cpu`, `alloc`, `monitor`, `calls`, `probes`; `trace` starts (or stops) the call tracer for `duration_ms` (0 = 10 s) or `max_events` (0 = `trace_events`) | `ok` |
| `UpdateConfig{config}` | applies a whole `SessionConfig` | `ok` |
| `SaveSnapshot{path}` | writes an `.oplink` recording on the agent's filesystem | `SnapshotResult` |
| `RunGc` | JVMTI `ForceGarbageCollection` | `ok` |
| `AddBookmark{label, color}` | adds a bookmark | `ok` |
| `SetTriggers{triggers}` | replaces the agent's triggers (see below) | `ok`, or an error for an invalid trigger |
| `SetInstrumentation{include, exclude, probes, complexity}` | replaces the call-counting patterns, enabled probes and complexity methods; retransforms affected classes | `InstrumentationInfo` (also the complexity specs and the call tracer's state) |
| `Instrumentation{}` | current instrumentation settings and statistics | `InstrumentationInfo` |
| `MBeanServers{}` | the MBean servers (platform, others in the JVM, connected remote JMX) | `MBeanServerList` |
| `MBeanConnect{url, username, password, enabled}` | connects the JVM to a remote JMX server, or disconnects it | `MBeanServerList` |
| `MBeanList{server, pattern}` | MBeans matching an ObjectName pattern (empty = all) | `MBeanList` (beans + tracked attributes) |
| `MBeanGet{server, name}` | `getMBeanInfo` plus every readable attribute's value | `MBeanInfo` |
| `MBeanInvoke{server, name, operation, signature, args}` | `MBeanServerConnection.invoke` | `MBeanInvokeResult` |
| `MBeanSet{server, name, attribute, value}` | `MBeanServerConnection.setAttribute` | `ok` |
| `MBeanTrack{server, name, attribute, item, label, unit, enabled}` | starts/stops sampling an attribute as telemetry | `MBeanList` (tracked only) |
| `MBeanSubscribe{server, name, enabled}` | adds/removes a notification listener on one MBean | `MBeanNotifications` (subscriptions only) |
| `MBeanNotifications{after}` | buffered notifications with `seq > after` | `MBeanNotifications` |

## MBeans

The `mbeans` capability exposes the JVM's MBean servers. Requests name one with `server`:

- empty: the platform MBean server (`java.lang`, `java.nio`, `com.sun.management`,
  `jdk.management.jfr`, and whatever the application registers there);
- the delegate's `MBeanServerId`: another MBean server created in the JVM through
  `MBeanServerFactory` (e.g. by an application server);
- a JMX service URL: a remote JMX server the JVM connected to with `MBeanConnect`. The connection
  is made from inside the profiled JVM (its network, its `java.management.rmi` module), times out
  after 15 s, and lives until `MBeanConnect{enabled: false}` or the JVM exits. Credentials are
  passed to the JMX connector and not stored otherwise; they travel over the agent link, so use TLS
  when the link leaves the host. A closed or failed connection is reported in
  `MBeanServerInfo.error`.

- Values (`MBeanAttribute.value_json`, `MBeanInvokeResult.value_json`) are JSON: numbers, booleans,
  strings and `null` as themselves; `CompositeData` and `Map`s as objects; `TabularData`,
  collections and arrays as arrays (at most 1000 items, nested at most 8 levels deep); `ObjectName`s
  as their canonical name; NaN/infinity and everything else as `toString()`.
- Invoke arguments and set values are strings converted to the declared type: primitives and their
  boxes, `String`, `Object` (passed as a string), `ObjectName`, and arrays of these written as a
  JSON array (`["a", "b"]`, `[1, 2]`) or a comma-separated list. Other types are rejected.
- Failures (unknown bean, conversion error, an exception thrown by the bean) come back as a failed
  `Response` with the root cause, e.g. `javax.management.InstanceNotFoundException: x:type=Y`.
- Requests run on a dedicated agent thread (`OpenProfiler MBeans`), one at a time in arrival
  order, so a slow operation or remote call delays other MBean requests but not data batches or
  other requests. Responses can therefore arrive out of order; they are matched by `id`.
- **Tracked attributes** are sampled once per second by the telemetry thread and published as
  ordinary telemetry series with key `mbean:<canonical name>#<attribute>` or
  `mbean:<canonical name>#<attribute>.<item>` for an item of a `CompositeData` attribute
  (e.g. `mbean:java.lang:type=Memory#HeapMemoryUsage.used`). Numbers and booleans (0/1) are
  plotted; other values produce no sample. Tracked series are recorded like the built-in ones
  (live data, `.oplink` recordings, snapshots) and can be used as threshold trigger metrics. The
  label defaults to `<attribute>[.<item>] (<name or type key property>)`; tracking an attribute
  again updates its label and unit. Stopping keeps the samples already taken. Attributes of other
  servers use the key `mbean:[<server>]<canonical name>#…`; remote ones are read by a separate
  poller thread so a slow connection never stalls the telemetry thread.
- **Notifications**: `MBeanSubscribe` adds a listener to one MBean (not a pattern) that is a
  `NotificationBroadcaster`; `MBeanInfo.notifications` lists what a bean declares. Received
  notifications go to a buffer of the last 1000, numbered by `seq` (increasing per agent). Poll
  with `MBeanNotifications{after}` and pass the returned `last_seq` as the next `after`; `missed`
  counts notifications after `after` that fell out of the buffer. `data_json` is the user data as
  JSON (e.g. the `GcInfo` of `com.sun.management.gc.notification`), or
  `{attribute, type, old, new}` for attribute change notifications. Notifications are not part of
  recordings or snapshots. Subscriptions to a remote server end when it is disconnected.

## Triggers

Triggers are conditions the agent evaluates itself, so they also work in offline mode and while
no daemon is connected. They are installed with the `triggers=<file>` option at start-up or with
`SetTriggers` (CLI: `openprofiler record|attach --triggers file.json`; HTTP: `POST .../triggers`; MCP:
`set_triggers`), which replaces the whole set.

```json
{"triggers": [
  {"id": "heap-90", "threshold": {"metric": "heap.used_pct", "above": 90, "for": "10s"},
   "actions": [{"heap_dump": {"path": "/tmp/heap-{pid}-{time}.hprof"}}, "thread_dump"],
   "cooldown": "5m", "max_fires": 1},
  {"id": "every-minute", "timer": {"delay": "30s", "period": "1m"},
   "actions": [{"save_snapshot": "/tmp/app-{n}.oplink"}]},
  {"id": "oom", "out_of_memory": {}, "actions": ["thread_dump", {"save_snapshot": "/tmp/oom.oplink"}]},
  {"id": "exit", "jvm_exit": {}, "actions": [{"save_snapshot": "/tmp/final.oplink"}]},
  {"id": "quiet-start", "jvm_start": {}, "actions": [{"stop_recording": "alloc"}]},
  {"id": "checkout", "method": {"class": "com.acme.Shop", "method": "checkout"},
   "actions": [{"bookmark": "checkout"}, "thread_dump"], "cooldown": "10s"}
]}
```

A bare array works too. Durations are numbers of milliseconds or strings with a unit (`ms`, `s`,
`m`, `h`).

| condition | fires |
|---|---|
| `timer {delay, period}` | `delay` after installation, then every `period` (0 or absent = once); missed periods are skipped |
| `threshold {metric, above, below, for}` | when a telemetry series stays above/below the value for `for` (evaluated once per second); fires once per excursion and re-arms when the condition stops holding |
| `out_of_memory` | JVMTI `ResourceExhausted` (Java heap, metaspace, native threads) |
| `jvm_start` | when the agent goes live, or immediately when installed on a live agent |
| `jvm_exit` | at VM death, before the offline recording is written or the final batch is sent |
| `method {class, method}` | when the method is called. `class` is a Java class name (`*` wildcards allowed), `method` a name, a name with descriptor (`place(Ljava/lang/String;)V`) or `*`. The method is instrumented with a trigger hook; without a `cooldown` it fires at most once per second |

Threshold metrics are the telemetry keys (`heap.used`, `heap.committed`, `nonheap.used`,
`gc.activity`, `threads.live`, `classes.loaded`, `cpu.process`, `cpu.system`, `alloc.rate`, …,
and tracked MBean attributes `mbean:…`) plus `heap.used_pct` (used heap as a percentage of the
maximum).

| action | effect |
|---|---|
| `start_recording` / `stop_recording` `"cpu"\|"alloc"\|"monitor"\|"calls"\|"probes"` | like `SetRecording` |
| `bookmark` `"label"` or `{label, color}` | adds a bookmark |
| `thread_dump` | takes a dump and adds it to the data plane (`DataBatch.thread_dumps`) |
| `heap_dump {path, live_only, dumper, recorded_only}` | writes an HPROF file on the agent's filesystem (`dumper`: `hotspot` or `native`, as in `HeapDump`) |
| `save_snapshot` `"path"` | writes an `.oplink` recording on the agent's filesystem |
| `run_gc` | `ForceGarbageCollection` |

Paths substitute `{pid}`, `{time}` (epoch seconds), `{trigger}` (the id) and `{n}` (how often the
trigger fired). `cooldown` is the minimum time between firings and `max_fires` caps them (0 =
unlimited). Each firing adds a bookmark `Trigger <id>` (with the threshold value or OOM
description), plus a bookmark per action that wrote a file or failed. Actions run one firing at a
time on the agent's trigger thread; `jvm_exit` actions run inside `VMDeath`.

## Instrumentation

The agent rewrites bytecode in its `ClassFileLoadHook` (Rust, `instrument/classfile.rs`; no Java
code runs while a class loads). It needs the `can_retransform_classes` capability (capability
group `instrument`; `noinstrument` turns it off). What a class gets:

- **Call counting** for classes matching `instrument=` (and not `exclude=`; JDK classes are never
  counted): `Hooks.callEnter(id)` at entry and `Hooks.callExit(id)` before every return and in a
  catch-all handler that rethrows. Each thread keeps a native shadow stack and a call trie; about
  once a second, at thread end and when its outermost frame returns, the trie becomes `calls`
  sample rows (self time, invocations) on stacks of the instrumented frames. Auto-tuning
  (`autotune`, on by default) removes methods called at least 50,000 times in 5 s with a mean
  self time under 400 ns: they are skipped at once and removed by retransformation.
- **Probe interceptions** registered by the boot classes' probes: `Hooks.enter(site, this, args)`
  (arguments boxed into an `Object[]` when the probe asks for them) and `Hooks.exit(result, site)`
  / `Hooks.exitThrow(throwable, site)`; constructors pass `null` for `this` at entry and `this` as
  the result.
- **Custom probes**: methods annotated with `@dev.openprofiler.api.Probe`.
- **Method triggers**: `Hooks.trigger(site)` at entry.
- **Complexity analysis** for methods matching `complexity=` specs: probe-style sites with boxed
  arguments, dispatched to the internal `ComplexityProbe` (see below).

The handler that runs exit hooks for exceptions covers the whole method, except in constructors,
where it starts after the `super(...)`/`this(...)` call. Branch offsets, switch padding, the
exception table, line and local-variable tables and `StackMapTable` frames are remapped; methods
with `jsr`/`ret`, branches that would overflow or code over 64 KB are left alone.

An interception names a method of one class or of every subtype of a class or interface (the type
included). The hook knows supertypes only by name, so it records per class a mask of intercepted
supertypes; a class loads before its supertypes, so masks are completed at `ClassPrepare` (a
parent's mask counts only once the parent is prepared) and late matches are retransformed right
there, before any code of the class runs. A subtype may inherit the method from a superclass that
is not a subtype (Hibernate's `AbstractSelectionQuery.getResultList`); such superclasses are
instrumented too, and their sites check the receiver's type at run time. Interceptions that only
act at entry (`Interception.entryOnly()`: JDBC setters and `close`, executor hand-offs, response
completions) get no exit hooks. Instrumented classes in named modules get a reads edge to the boot loader's
unnamed module (where `Hooks` lives) before their code runs. Classes loaded before the hook was
active (bootstrap classes, everything after a dynamic attach) are found by a scan of the loaded
classes and retransformed. Lambdas and other hidden classes cannot be instrumented.

## Call tracer, outliers and complexity

Three analyses of instrumented methods, sent as columnar batches (`DataBatch.call_trace`,
`call_sites`, `complexity`) with their own watermark positions:

- **Call tracer.** `SetRecording{kind: "trace"}` (or the `trace=<duration>` option, or a trigger's
  `start_recording: "trace"`) makes every `callEnter`/`callExit` of call-counted methods also
  append `(time, frame, exit)` to a per-thread buffer, until the deadline or the event cap
  (`trace_events`, default 1,000,000, shared by all threads). Buffers are flushed with the call
  counts (about once a second, at 4,096 events, at thread end), so a thread's events arrive in
  order. Frames popped by an exit that closes several (an exception past uninstrumented handlers)
  each get an exit. When the tracer is off the hooks check one flag. Readers rebuild calls per
  thread: an exit closes the innermost open call of its method and everything above it; exits
  without an entry belong to calls that began before the trace.
- **Call-site statistics** (outlier detection). Each node of a thread's call trie, i.e. each path
  of instrumented frames, also keeps its inclusive time and slowest call; every flush sends a
  `CallSiteBatch` row `(time, thread, stack, count, total, max)` next to the `calls` samples.
  Readers aggregate per stack and compare `max` with `total / count`.
- **Complexity analysis.** Methods named by `complexity=` (`SetInstrumentation.complexity`):
  `class.method[(descriptor)][#arg]`, the class with `*` wildcards. `arg` is a parameter index or
  `this`; by default the first argument that has a size. The `ComplexityProbe` takes the size at
  entry (a `Collection`'s or `Map`'s `size()`, an array's or `CharSequence`'s length, a
  non-negative `Number`'s value) and at a normal exit calls `Hooks.complexity0(site, size, nanos)`.
  The agent keeps at most `complexity_rate` points (default 100) per method and second. A method
  already intercepted by a probe is not analysed.

## Probes

Probes are Java classes in the boot classes implementing the SPI `dev.openprofiler.boot.Probe`:
`interceptions()` names the methods; `onEnter(call, this, args)` and `onExit(call, this, result,
error)` fill a `Call` (payload, detail, error, control object id); helpers emit control-object
opens and closes and set the thread's request label. The runtime guards against re-entrancy
(calls made by probe code are not intercepted), passes nested calls of the same probe to the
callbacks but records only the outermost one, and catches every failure
(`-Dopenprofiler.probes.debug=true` prints the first ones).

Each recorded call is aggregated into `probe_calls`; calls within the per-probe budget
(`probe_events` per second, default 100, only calls of at least `probe_threshold`) and calls of
100 ms or more are also sent as `ProbeEvent`s. Stacks end at the intercepted method (the boot
classes' frames are removed). Walking the stack is most of the cost of a probe call (tens of
nanoseconds per frame), so events always get their own stack, but a full stack for a (probe,
request label, payload) is taken at most once per `probe_stack_interval` (default 10 ms, any
thread) and the calls in between are attributed to it: probe call trees are exact in their totals
and sampled in their split across call paths. `probe_stack_interval=0` takes a stack for every
call. Threads aggregate their calls themselves; the sampler thread moves them into the batches.
A control object (`ControlEvent`) is opened and closed by the probe; one garbage collected without
being closed is reported as closed with the description `garbage collected without close`. Opens
carry a stack, closes do not.

| probe id | intercepts | payload | control objects |
|---|---|---|---|
| `jdbc` | `Statement.execute*`, `executeBatch`, `Connection.prepare*`, `PreparedStatement.set*`, `Driver.connect`, `DataSource.getConnection`, `Connection.close`, `Statement.close` (all subtypes) | SQL (prepared statements remember theirs); detail: bind parameters | connections (`physical` from drivers, `pooled` from data sources) |
| `jpa` | `EntityManager` `persist/merge/remove/find/refresh/flush`, `Query` `getResultList/getSingleResult/executeUpdate/getResultStream` (javax and Jakarta, also where a provider implements them in a superclass) | operation and entity, or the JPQL (Hibernate's query string, EclipseLink's `getJPQLString`) | — |
| `http_server` | Servlet `FilterChain.doFilter` and `HttpServlet.service` (javax and Jakarta; asynchronous requests until their `AsyncListener` reports completion), JDK `Filter$Chain.doFilter`, Netty `SimpleChannelInboundHandler.channelRead` with an `HttpRequest`, Reactor Netty (`HttpServerHandle.onStateChange` to `HttpServerOperations.onOutboundComplete`/`onOutboundError`, so Spring WebFlux), Vert.x HTTP/1.x (`Http1xServerConnection.handleMessage` to `responseComplete`) and HTTP/2 (`Http2ServerStream.onHeaders` to the response's last write, also after an `h2c` upgrade), so Quarkus | method and path with ids replaced (`GET /owners/{id}`); detail: status; also the request label | — |
| `http_client` | JDK `HttpClient` (`send`, `sendAsync`), `HttpURLConnection`, OkHttp 3/4/5 `RealCall.execute`, Apache HttpClient 4/5 classic `doExecute` (relative request URIs get the target host) | method and URL (ids replaced, no query); detail: status | — |
| `kafka` | `KafkaProducer.send`, `KafkaConsumer.poll` (empty polls ignored), `commitSync` | `send <topic>`, `poll <subscription>` | — |
| `grpc` | `ClientCalls` unary calls (blocking, future, async; the method of a `ClientCall` is read from the call), unary server calls | full method name; server calls set the request label | — |
| `mongodb` | `InternalStreamConnection.sendAndReceive` (sync driver), `sendAndReceiveAsync` until `getCommandResult` decodes the reply with that message id (reactive streams and async drivers); handshakes and heartbeats skipped | `<command> <db>.<collection>` (`find matrix.orders`) | — |
| `cassandra` | DataStax 4.x `Session.execute(Request, GenericType)` (sync and async, until the `CompletionStage` completes), 3.x `AbstractSession.execute(Statement)` and `executeAsync` (until the future's listener runs) | CQL (bound statements: the prepared CQL; batches: `BATCH <n>: <first CQL>`); detail: rows | — |
| `jms` | `MessageProducer.send`, `JMSProducer.send`, `MessageConsumer.receive`, `JMSConsumer.receive`/`receiveBody` (receives that time out ignored), `MessageListener.onMessage` (javax and Jakarta, all subtypes) | `send <destination>`, `receive <destination>`, `process <destination>`; listeners set the request label `JMS <destination>` and record what they do | — |
| `rmi` | `UnicastRef.invoke` (stubs and dynamic proxies), `UnicastServerRef.dispatch` named at `logCall` (skeleton calls such as the registry and DGC skipped) | `call <Interface>.<method>`, `serve <Interface>.<method>`; served calls set the request label `RMI <Interface>.<method>` | — |
| `webservices` | JAX-WS clients (Metro `SEIStub.invoke`, CXF `JaxWsClientProxy.invoke`, `Dispatch.invoke`/`invokeOneWay`, javax and Jakarta) and endpoints (Metro `server.sei.Invoker.invoke`, CXF `AbstractInvoker.performInvocation`) | `call <Service>.<operation>` (detail: endpoint address), `dispatch <address>`, `serve <Service>.<operation>`; served operations set the request label `SOAP <Service>.<operation>` | — |
| `llm` | OpenAI Java SDK chat completions, responses and embeddings (blocking and async), Anthropic Java SDK messages (blocking and async), LangChain4j `ChatModel.chat(ChatRequest)` and 0.x `ChatLanguageModel` `chat`/`generate(List)`, Spring AI `ChatModel.call(Prompt)`; async calls until the future completes; streaming not recorded | `<operation> <model>` (`chat.completions gpt-4o-mini`, `messages claude-sonnet-4-5`, `chat <model>`); detail: `tokens <in> in, <out> out` | — |
| `exceptions` | `Throwable` constructors | exception class; detail: message | — |
| `files` | `FileInputStream`/`FileOutputStream`/`RandomAccessFile` (open, bulk read/write, close) | `read <path>`, `write <path>`; detail: bytes | open files |
| `sockets` | `Socket.connect`, `ServerSocket.accept`, `Socket.close` | `connect <address>`, `accept <address>` | open sockets |
| `custom.<name>` | `@Probe(value = "<name>", payload = "…")` methods, `OpenProfiler.probeEvent` | the formatted template (`{0}`, `{this}`) | — |

**Request labels.** A probe serving a request (`http_server`, `grpc`, `rmi` and `webservices`
servers, `jms` listeners) sets the thread's label for the duration of the call. The label is the
root frame of the thread's CPU, wall,
allocation, monitor, `calls` and probe stacks (`[GET /owners/{id}]`), so call trees split by
request, and the `context` of its probe events. An internal `async` probe carries the label across
hand-offs: `FutureTask` (executor `submit`, scheduled executors, Reactor schedulers),
`ForkJoinTask` (fork/join, parallel streams, `CompletableFuture` stages),
`ThreadPoolExecutor.execute` of plain runnables, `Thread.start` (platform and virtual threads),
Kotlin coroutine dispatch and JBoss Threads' `EnhancedQueueExecutor` (Quarkus' and WildFly's
worker pools).

Asynchronous servers (Reactor Netty, Vert.x, asynchronous servlets) label the thread while the
request's handler is dispatched and subscribed, so work it schedules inherits the label, and
record the request when its response completes, with the time from receipt to completion, the
request as context and its label as the stack's root. Pipeline stages that resume on an event loop
from I/O callbacks (not from a scheduler or executor the request handed work to) are not labelled.

**Version matrix.** `java/matrix/run.sh` runs the probes against real libraries (see
`java/matrix/matrix.txt`): JDBC drivers (H2, PostgreSQL, MySQL) behind HikariCP; Hibernate 5–7
and EclipseLink 2.7/4; Tomcat 9–11, Jetty 9.4–12 and Undertow 2.2/2.3 (javax and Jakarta); Netty
4.1/4.2; Reactor Netty 1.0–1.2 and Spring WebFlux 5.3–6.2; Vert.x 4.5/5; OkHttp 3–5 and Apache
HttpClient 4/5; Kafka clients 2.8–4.0; grpc-java 1.40–1.69; Reactor core 3.4–3.7 and Kotlin
coroutines 1.6–1.10; the MongoDB sync driver 3.12–5.2 and reactive streams driver 4.11/5.2; the
DataStax Cassandra driver 3.11 and 4.11–4.18; JMS with Artemis 2.31/2.38 and ActiveMQ 5.18/6.1
clients (against an Artemis broker); JAX-WS with Metro 2.3/4.0 and CXF 3.6/4.0; the OpenAI
and Anthropic Java SDKs, LangChain4j 0.36/1.2 and Spring AI 1.0 (against a local fake API). Each case records with the agent in offline mode and checks payloads,
events, control objects, request context and call-tree links
(`crates/openprofiler-link/examples/oplink_check.rs`).

## Offline recordings (`.oplink`)

With `mode=offline,file=<path>` (or the Controller API's `OpenProfiler.saveSnapshot`) the agent writes
the frames a daemon would have received:

```
"OPRFLINK" | frame(RecordingHeader{hello, created}) | frame(DataBatch)*
```

`openprofiler_link::replay_recording` turns this into an `openprofiler_model::Profile`. The `.opsnap` snapshot
format (see the snapshot spec) supersedes this as the long-term archive format.

## Agent options

`-agentpath:/path/libopenprofiler_agent.so=<k=v,…>`, the attach argument, or `OPENPROFILER_AGENT_OPTIONS`
(applied first).

| option | default | meaning |
|---|---|---|
| `port` | `0` | listen port (0 = ephemeral) |
| `bind` / `address` | `127.0.0.1` | listen address (`loopback` is a shortcut) |
| `token` | random | required client token |
| `tls` | off | TLS 1.3 with a pinned self-signed certificate |
| `mode` | `live` | `live` or `offline` (`offline` is a shortcut) |
| `file` | `openprofiler-<pid>.oplink` | offline recording path |
| `infofile` | `$TMPDIR/openprofiler-<pid>.json` | where to publish port/token |
| `interval` | `10ms` | CPU sampling interval |
| `wall` | `50ms` | wall-clock / thread-state sampling interval |
| `sampler` | `auto` | `auto`, `async` (Linux), `sync` |
| `cpu`, `alloc`, `monitors` | on | initial recording state |
| `alloc_interval` | `512k` | mean bytes between allocation samples |
| `monitor_threshold` | `100us` | longer monitor events and parks are sent individually; shorter monitor events are sampled, shorter parks not recorded |
| `monitor_interval` | `1ms` | blocked or waiting time per thread that yields one sample of short monitor events (see [Monitor events](#monitor-events)) |
| `owner_rate` | `50` | owner lookups for contended monitors per second (each is a safepoint); `0` disables them |
| `vthreads` | on | virtual-thread mount tracking (JDK 21+) |
| `triggers` | — | JSON trigger file loaded at start-up |
| `recover` | on | repair failed `AsyncGetCallTrace` walks (Linux async sampler) |
| `max_frames` | `256` | maximum stack depth |
| `wait` / `nowait` | `nowait` | block VM start until a daemon connected |
| `bootjar` | embedded | use an external `openprofiler-boot.jar` instead of the embedded classes |
| `instrument` / `include` | — | `:`-separated class patterns (`com.acme.*:org.x.Y`) that get exact call counts |
| `exclude` | — | `:`-separated class patterns excluded from call counting |
| `probes` | `all` | `:`-separated probe ids to enable, `all` or `none` |
| `autotune` | on | remove hot, trivial methods from call counting |
| `probe_events` | `100` | individual probe events per probe and second (slower calls are always kept) |
| `probe_threshold` | `0` | probe calls faster than this are aggregated but not sent as events |
| `probe_stack_interval` | `10ms` | minimum time between full stacks for probe calls of one (probe, request, payload); `0`: every call |
| `complexity` | — | `:`-separated complexity specs (`com.acme.Sorter.sort#0:com.acme.Index.find`) |
| `complexity_rate` | `100` | complexity points kept per method and second |
| `trace` | — | run the call tracer for this long once the agent is live (`5s`) |
| `trace_events` | `1000000` | event cap of traces started without one |
| `instrumentation` / `noinstrument` | on | bytecode instrumentation (probes, call counts, method triggers) |
| `debug` | off | log to stderr |

## Samplers

- **Async (Linux, default):** a POSIX timer per thread on its CPU-time clock
  (`timer_create(MAKE_THREAD_CPUCLOCK(tid), SIGEV_THREAD_ID)`) delivers `SIGPROF` to the thread
  that consumed CPU. The handler calls `AsyncGetCallTrace` and copies frames into a lock-free
  ring buffer. A drainer thread resolves the frames. Foreign `SIGPROF` signals are chained to the
  previous handler. ClassLoad/ClassPrepare events are enabled so `jmethodID`s exist. The agent's
  own threads are never armed.
  - `AsyncGetCallTrace` fails in two common situations, which the handler repairs and retries
    (`recover=0` turns this off). (1) The PC is in a frameless stub (array zeroing/copying,
    itable/vtable stubs) or a method prologue/epilogue (`[unknown Java]`): the handler unwinds one
    frame on a copy of the signal context (x86-64 and AArch64), reading memory with
    `process_vm_readv` so a bad guess cannot fault. (2) The thread is in the VM, called through a
    path that left its last-Java-frame anchor without a PC, or the anchor points into a C2 runtime
    stub that HotSpot marks as never walkable (`[not walkable, in VM]`): the handler fills in the
    anchor's PC the way `JavaFrameAnchor::make_walkable()` does, and marks the stub's frame as
    complete. Both use VMStructs offsets (`JavaThread::_anchor`, `CodeHeap`, `CodeBlob`); without
    them the repair is skipped. In tests on JDK 21 this cut failed walks from 31% to 0% of samples
    on an allocation-heavy workload, and from 79% to 3–8% on a stress test.
  - Samples without Java frames are labelled by thread role: `[JIT compiler]`, `[GC]`,
    `[VM operation]`, `[VM periodic task]`, otherwise `[no Java frames]` / `[non-Java thread]`.
    Remaining failures keep a synthetic frame (`[unknown Java]`, `[GC active]`, `[deoptimization]`,
    …).
  - A sample's return address can only be mapped to the nearest debug-info scope, so time in a
    stub called from inlined code may land on a neighbouring inlined method.
- **Sync (macOS, fallback):** `GetThreadListStackTraces` on all threads each tick. RUNNABLE
  threads count as CPU only if their thread CPU time advanced.
- Wall-clock samples and thread-state history always come from the sync sampler (default 20 Hz).

## Attach

`openprofiler_attach::attach_agent(pid, agent, options)` uses the HotSpot attach mechanism. It creates
`.attach_pid<nspid>`, sends `SIGQUIT`, connects to `.java_pid<nspid>` and sends
`1\0load\0<path>\0true\0<options>\0`. It passes a fresh token and an `infofile` and then reads
that file. For containers on Linux it uses `NSpid` and `/proc/<pid>/root/tmp`, and copies the
agent into the target's `/tmp/openprofiler/`.
