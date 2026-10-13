# Ember agent-link protocol v1

The agent-link protocol connects the in-JVM agent (`libember_agent`, the **server**) to
`ember-daemon` (the **client**). The schema lives in
[`crates/ember-proto/proto/agent_link.proto`](../../crates/ember-proto/proto/agent_link.proto);
the daemon-side implementation is `crates/ember-link`.

## Transport

- TCP, one daemon connection at a time. The agent binds `bind` (default `127.0.0.1`) on `port`
  (default `0` = ephemeral).
- **Authentication is mandatory**: every connection must present the agent's token. If the
  `token=` option is not given, the agent generates a random 128-bit token.
- **TLS 1.3** with the `tls` option. The agent generates a self-signed certificate (ECDSA P-256)
  at start-up; the client pins its SHA-256 fingerprint (published in the info file). TLS 1.2 and
  other certificates are rejected.
- Over a relay (`ember-relay`, `docker exec`, `kubectl exec`) the link runs in plain TCP on
  loopback inside the target namespace. The token still applies.

### Info file

After binding, the agent publishes `$TMPDIR/ember-<pid>.json` (or the `infofile=` path),
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
| strings, methods, frames | `ember_model::Symbols` (string 0 is `""`) |
| stacks | global stack trie (node 0 = empty stack) |
| threads | `ThreadInfo` rows (index = thread id used by all other tables) |
| samples | per-second pre-aggregated rows `(time, kind, thread, state, stack, weight, count, class)` |
| telemetry | `(series, time, value)` rows; series are declared by the Java telemetry sampler |
| thread_states | closed state spans `(thread, start, end, state)`; open spans are cut once per second |
| monitors, gc, bookmarks | event rows |

A `Watermark` holds one absolute position per table. A `DataBatch` carries everything in
`[from, to)`. The daemon applies tables in order (symbols, stacks, threads, then the rest) and
interns them in the same order, so the agent's ids equal the daemon's ids. `ThreadInfoBatch.updates`
carry later changes to existing threads (end time, rename).

Sample kinds (`ember_model::SampleKind`): `0 cpu`, `1 wall`, `2 alloc`, `3 monitor`, `4 wait`,
`5 io`, `6 exception`. Thread states: `0 runnable`, `1 waiting`, `2 blocked`, `3 net_io`,
`4 sleeping`, `5 new`, `6 terminated`, `7 unknown`.

Weights:
- `cpu`: nanoseconds of CPU time (async sampler: the timer interval; sync sampler: the thread's
  measured CPU-time delta, capped at 2× the interval).
- `wall`: the wall-clock interval.
- `alloc`: unbiased estimate of the allocated bytes a sample represents
  (`size / (1 - e^(-size/interval))`); `class_id` is the allocated class.
- `monitor` / `wait`: blocked/waiting nanoseconds; `class_id` is the monitor's class.

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
| `SaveSnapshot{path}` | writes an `.emberlink` recording on the agent's filesystem | `SnapshotResult` |
| `RunGc` | JVMTI `ForceGarbageCollection` | `ok` |
| `AddBookmark{label, color}` | adds a bookmark | `ok` |

## Offline recordings (`.emberlink`)

With `mode=offline,file=<path>` (or the Controller API's `Ember.saveSnapshot`) the agent writes
the frames a daemon would have received:

```
"EMBRLINK" | frame(RecordingHeader{hello, created}) | frame(DataBatch)*
```

`ember_link::replay_recording` turns this into an `ember_model::Profile`. The `.ember` snapshot
format (see the snapshot spec) supersedes this as the long-term archive format.

## Agent options

`-agentpath:/path/libember_agent.so=<k=v,…>`, the attach argument, or `EMBER_AGENT_OPTIONS`
(applied first).

| option | default | meaning |
|---|---|---|
| `port` | `0` | listen port (0 = ephemeral) |
| `bind` / `address` | `127.0.0.1` | listen address (`loopback` is a shortcut) |
| `token` | random | required client token |
| `tls` | off | TLS 1.3 with a pinned self-signed certificate |
| `mode` | `live` | `live` or `offline` (`offline` is a shortcut) |
| `file` | `ember-<pid>.emberlink` | offline recording path |
| `infofile` | `$TMPDIR/ember-<pid>.json` | where to publish port/token |
| `interval` | `10ms` | CPU sampling interval |
| `wall` | `50ms` | wall-clock / thread-state sampling interval |
| `sampler` | `auto` | `auto`, `async` (Linux), `sync` |
| `cpu`, `alloc`, `monitors` | on | initial recording state |
| `alloc_interval` | `512k` | mean bytes between allocation samples |
| `monitor_threshold` | `100us` | shorter monitor events are only aggregated |
| `max_frames` | `256` | maximum stack depth |
| `wait` / `nowait` | `nowait` | block VM start until a daemon connected |
| `bootjar` | embedded | use an external `ember-boot.jar` instead of the embedded classes |
| `debug` | off | log to stderr |

## Samplers

- **Async (Linux, default):** a POSIX timer per thread on its CPU-time clock
  (`timer_create(MAKE_THREAD_CPUCLOCK(tid), SIGEV_THREAD_ID)`) delivers `SIGPROF` to the thread
  that consumed CPU. The handler calls `AsyncGetCallTrace` and copies frames into a lock-free
  ring buffer. A drainer thread resolves the frames. Failed walks are recorded as synthetic
  frames (`[GC active]`, `[unknown Java]`, `[non-Java thread]`, …). Foreign `SIGPROF` signals are
  chained to the previous handler. ClassLoad/ClassPrepare events are enabled so `jmethodID`s exist.
- **Sync (macOS, fallback):** `GetThreadListStackTraces` on all threads each tick. RUNNABLE
  threads count as CPU only if their thread CPU time advanced.
- Wall-clock samples and thread-state history always come from the sync sampler (default 20 Hz).

## Attach

`ember_attach::attach_agent(pid, agent, options)` uses the HotSpot attach mechanism. It creates
`.attach_pid<nspid>`, sends `SIGQUIT`, connects to `.java_pid<nspid>` and sends
`1\0load\0<path>\0true\0<options>\0`. It passes a fresh token and an `infofile` and then reads
that file. For containers on Linux it uses `NSpid` and `/proc/<pid>/root/tmp`, and copies the
agent into the target's `/tmp/ember/`.
