# OpenProfiler agent-link protocol v1

The agent-link protocol connects the in-JVM agent (`libopenprofiler_agent`, the **server**) to
`openprofiler-daemon` (the **client**). The schema lives in
[`crates/openprofiler-proto/proto/agent_link.proto`](../../crates/openprofiler-proto/proto/agent_link.proto);
the daemon-side implementation is `crates/openprofiler-link`.

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

A `Watermark` holds one absolute position per table. A `DataBatch` carries everything in
`[from, to)`. The daemon applies tables in order (symbols, stacks, threads, then the rest) and
interns them in the same order, so the agent's ids equal the daemon's ids. `ThreadInfoBatch.updates`
carry later changes to existing threads (end time, rename).

Sample kinds (`openprofiler_model::SampleKind`): `0 cpu`, `1 wall`, `2 alloc`, `3 monitor`, `4 wait`,
`5 io`, `6 exception`. Thread states: `0 runnable`, `1 waiting`, `2 blocked`, `3 net_io`,
`4 sleeping`, `5 new`, `6 terminated`, `7 unknown`.

Weights:
- `cpu`: nanoseconds of CPU time (async sampler: the timer interval; sync sampler: the thread's
  measured CPU-time delta, capped at 2× the interval).
- `wall`: the wall-clock interval.
- `alloc`: unbiased estimate of the allocated bytes a sample represents
  (`size / (1 - e^(-size/interval))`); `class_id` is the allocated class.
- `monitor` / `wait`: blocked/waiting nanoseconds; `class_id` is the monitor's class (for parks,
  the blocker's class).

### Monitor events

`MonitorEvent.kind` is `0` enter (contended `synchronized`), `1` wait (`Object.wait`) or `2` park
(`java.util.concurrent`). Only events of at least `monitor_threshold` are sent individually.

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
| `HeapDump{format, live_only, local_path}` | `HotSpotDiagnosticMXBean.dumpHeap` | `HeapDumpResult`; without `local_path` the HPROF follows as `BlobChunk`s (1 MiB, `last` on the final one) |
| `SetRecording{kind, enabled, interval_us}` | toggles `cpu`, `alloc`, `monitor` | `ok` |
| `UpdateConfig{config}` | applies a whole `SessionConfig` | `ok` |
| `SaveSnapshot{path}` | writes an `.oplink` recording on the agent's filesystem | `SnapshotResult` |
| `RunGc` | JVMTI `ForceGarbageCollection` | `ok` |
| `AddBookmark{label, color}` | adds a bookmark | `ok` |
| `SetTriggers{triggers}` | replaces the agent's triggers (see below) | `ok`, or an error for an invalid trigger |

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
  {"id": "quiet-start", "jvm_start": {}, "actions": [{"stop_recording": "alloc"}]}
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

Threshold metrics are the telemetry keys (`heap.used`, `heap.committed`, `nonheap.used`,
`gc.activity`, `threads.live`, `classes.loaded`, `cpu.process`, `cpu.system`, `alloc.rate`, …)
plus `heap.used_pct` (used heap as a percentage of the maximum).

| action | effect |
|---|---|
| `start_recording` / `stop_recording` `"cpu"\|"alloc"\|"monitor"` | like `SetRecording` |
| `bookmark` `"label"` or `{label, color}` | adds a bookmark |
| `thread_dump` | takes a dump and adds it to the data plane (`DataBatch.thread_dumps`) |
| `heap_dump {path, live_only}` | writes an HPROF file on the agent's filesystem |
| `save_snapshot` `"path"` | writes an `.oplink` recording on the agent's filesystem |
| `run_gc` | `ForceGarbageCollection` |

Paths substitute `{pid}`, `{time}` (epoch seconds), `{trigger}` (the id) and `{n}` (how often the
trigger fired). `cooldown` is the minimum time between firings and `max_fires` caps them (0 =
unlimited). Each firing adds a bookmark `Trigger <id>` (with the threshold value or OOM
description), plus a bookmark per action that wrote a file or failed. Actions run one firing at a
time on the agent's trigger thread; `jvm_exit` actions run inside `VMDeath`.

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
| `monitor_threshold` | `100us` | shorter monitor events are only aggregated; shorter parks are not recorded |
| `owner_rate` | `50` | owner lookups for contended monitors per second (each is a safepoint); `0` disables them |
| `vthreads` | on | virtual-thread mount tracking (JDK 21+) |
| `triggers` | — | JSON trigger file loaded at start-up |
| `recover` | on | repair failed `AsyncGetCallTrace` walks (Linux async sampler) |
| `max_frames` | `256` | maximum stack depth |
| `wait` / `nowait` | `nowait` | block VM start until a daemon connected |
| `bootjar` | embedded | use an external `openprofiler-boot.jar` instead of the embedded classes |
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
