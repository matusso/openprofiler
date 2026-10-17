# `.opsnap` snapshot format, version 1.1

An `.opsnap` file stores one profiling session: symbols, stacks, samples, threads, telemetry,
GC, monitor events, bookmarks, thread dumps and optionally an embedded HPROF heap dump. It is
written by `openprofiler convert`, the daemon's `POST .../snapshot` and the agent's offline mode.
Reference implementation: `crates/openprofiler-snapshot`.

## Layout

All integers are little-endian.

```
Header
  magic        8 bytes   "OPRFSNAP"
  major        u16       1 (readers reject other majors)
  minor        u16       1 (0: no Arrow sections, no Calls section)
  flags        u32       0, reserved
  created      i64       epoch nanoseconds
  jvm_len      u32
  jvm          jvm_len bytes, protobuf openprofiler.snapshot.v1.JvmInfo

Section (repeated, each optionally preceded by zero padding)
  kind         u32       see below
  len          u64       stored payload length
  codec        u8        0 = none, 1 = zstd (one frame), 2 = zstd frames, 3 = Arrow IPC file
  payload      len bytes

Index           protobuf openprofiler.snapshot.v1.Index
Trailer
  index_offset u64
  index_len    u64
  magic        8 bytes   "OPRFSEND"
```

Readers start at the trailer, read the index, and load only the sections they need. Every index
entry has the section's offset, stored length, codec, uncompressed length and an XXH3-64
checksum of the stored payload. Readers must verify the checksum. Sections are located only
through the index; writers may insert zero padding before a section (they do so to align Arrow
payloads to 64 bytes).

## Section kinds

| kind | name | payload (protobuf, `crates/openprofiler-snapshot/proto/snapshot.proto`) |
|---:|---|---|
| 1 | Meta | `Meta`: source, JVM info, time range, CPU sampling interval, properties |
| 2 | Symbols | `Symbols`: string table plus columnar method table (class, name, descriptor string ids) and frame table (method, line, kind) |
| 3 | Stacks | `Stacks`: prefix-trie nodes. Node `i+1` = (`parent[i]`, `frame[i]`), node 0 is the empty stack |
| 4 | Samples | One section per sample kind (cpu, wall, alloc, monitor, wait, io, exception, calls), named after the kind. 1.1: Arrow table `samples`; 1.0: `Samples`, columnar, time delta-encoded. Kind 8 (probe) is not stored; readers derive it from the probe calls |
| 5 | Threads | `Threads` |
| 6 | ThreadStates | State intervals. 1.1: Arrow table `thread_states`; 1.0: `ThreadStates`, columnar |
| 7 | Telemetry | Named series (`heap.used`, `cpu.process`, …). 1.1: Arrow table `telemetry`; 1.0: `Telemetry`, time delta-encoded |
| 8 | Gc | 1.1: Arrow table `gc`; 1.0: `Gc` |
| 9 | Monitors | 1.1: Arrow table `monitors`; 1.0: `Monitors` |
| 10 | Bookmarks | `Bookmarks` |
| 11 | ThreadDumps | `ThreadDumps` |
| 12 | HeapDump | Raw HPROF, codec 2 |
| 13 | Config | Reserved: recording configuration |
| 14 | Extension | Reserved: vendor or plugin data, identified by the index entry `name` |
| 15 | Probes | 1.1: `Probes` with the registered probes only (section name `probes`), plus Arrow tables `probe_calls`, `probe_events` and `probe_controls` as further kind-15 sections named after the table. 1.0: one `Probes` with the probes, aggregated calls (columnar, time delta-encoded, payloads are string ids), probe events and control-object events |
| 16 | Calls | `Calls` (since 1.1): call tracer entries/exits, per-call-site statistics (count, total, max) and complexity points (frame, size, duration), each columnar with delta-encoded times |

Unknown section kinds must be ignored. A reader picks the protobuf section of a kind by its codec
(0 or 1) and the Arrow sections by codec 3 and name.

## Codec 2: independent zstd frames

```
frame_raw_size u32      uncompressed size of every frame except the last (8 MiB)
n              u32      number of frames
lengths        n × u64  compressed length of each frame
frames                  concatenated zstd frames
```

Frame `i` decompresses to raw bytes `[i × frame_raw_size, …)`. This allows parallel extraction
and random access without decompressing the whole dump.

## Codec 3: Arrow IPC (1.1)

The payload is a complete [Arrow IPC file](https://arrow.apache.org/docs/format/Columnar.html#ipc-file-format)
(`ARROW1` magic, schema, record batches of up to 2^20 rows, footer) whose buffers are compressed
with Arrow's own zstd codec, so `len` equals the uncompressed length in the index. Payloads start
64-byte aligned in the file, so a reader can memory-map the file and hand the byte range
`[offset + 13, offset + 13 + len)` of a section's index entry to any Arrow reader without copying.
`openprofiler arrow --list file.opsnap` prints those offsets and lengths.

Conventions for every table:
- Times and durations are `int64` nanoseconds; times are absolute (the same clock as `Meta.start`).
- `thread` is an index into the Threads section, `stack` an id in the Stacks section
  (0 = empty stack).
- Strings are `utf8_view` with deduplicated data buffers: enumerations (thread state, monitor and
  control kinds) as their names, probes by id, and allocated classes and payloads as text.
- Schema metadata: `openprofiler.table` (table name) and `openprofiler.format` (`opsnap 1.1`).

| table | columns (nullable marked `?`) |
|---|---|
| `samples` | `time`, `thread` u32, `state` (`runnable`, `waiting`, `blocked`, `net_io`, `sleeping`, `new`, `terminated`, `unknown`), `stack` u32, `weight` u64, `count` u32, `class?` (alloc, exception and monitor samples). Metadata `openprofiler.sample_kind` |
| `thread_states` | `thread` u32, `start`, `end`, `state` |
| `telemetry` | `key`, `time`, `value` f64: one row per point, grouped by series. Metadata `openprofiler.series` (count) and `openprofiler.series.<i>.key`, `.label`, `.unit` (`nanos`, `bytes`, `count`, `percent`, `per_second`) list every series in order, including series without points |
| `gc` | `start`, `duration`, `name`, `cause`, `heap_before?` u64, `heap_after?` u64, `pause?` |
| `monitors` | `start`, `duration`, `kind` (`enter`, `wait`, `park`), `thread` u32, `owner?` u32, `monitor_class`, `monitor_id?` u64, `stack` u32 |
| `probe_calls` | `time`, `probe` (id), `payload`, `thread` u32, `stack` u32, `count` u32, `duration`, `max`, `errors` u32: calls aggregated per second, payload and stack |
| `probe_events` | `start`, `duration`, `probe`, `payload`, `thread` u32, `stack` u32, `error?`, `control?` u64, `detail?`, `context?` |
| `probe_controls` | `time`, `probe`, `id` u64, `kind` (`open`, `close`), `description`, `thread` u32, `stack` u32 |

A 1.0 reader rejects codec 3, so 1.1 files need openprofiler newer than 0.4; the reference
implementation still reads 1.0 files and can write them (`write_snapshot_v1_0`).

### Querying snapshots with other tools

`openprofiler arrow file.opsnap -o tables/` writes every table above as a standalone `.arrow`
file (samples as `samples_<kind>.arrow`), with plain `utf8` strings for readers without string
view support, plus tables that resolve ids: `threads` (`thread`, `java_id`, `os_id?`, `name`,
`group?`, `is_virtual`, `is_daemon`, `start?`, `end?`), `stacks` (`stack`, `parent`, `frame`:
the leaf frame and the rest of the stack), `frames` (`frame`, `class`, `method`, `signature`,
`line`, `kind`) and `probes` (`probe`, `name`, `payload_label`, `control_label?`). It accepts
anything openprofiler opens (`.opsnap` 1.0/1.1, `.jfr`, `.oplink`).

```sql
-- DuckDB (community extension: INSTALL arrow FROM community; LOAD arrow;)
SELECT payload, sum(count) AS calls, sum(duration) / 1e6 AS ms
FROM read_arrow('tables/probe_calls.arrow') WHERE probe = 'jdbc'
GROUP BY ALL ORDER BY ms DESC LIMIT 10;

SELECT t.name, count(*) AS samples
FROM read_arrow('tables/samples_cpu.arrow') s JOIN read_arrow('tables/threads.arrow') t USING (thread)
GROUP BY ALL ORDER BY samples DESC;
```

```python
import polars as pl
pl.read_ipc("tables/samples_alloc.arrow").group_by("class").agg(pl.col("weight").sum()).sort("weight")

# A section in place, without exporting (pyarrow; offset and length from `openprofiler arrow --list`)
import pyarrow as pa, pyarrow.ipc as ipc
buf = pa.memory_map("app.opsnap").read_buffer()
cpu = ipc.open_file(buf.slice(offset, length)).read_all()   # DuckDB can query `cpu` directly
```

## Derived data

Heap analysis indexes (object index, CSR graphs, dominator tree, retained sizes) are not part of
the file. They are cached next to it in `<file>.openprofiler-idx/`, or under `~/.cache/openprofiler/` when that
directory is not writable. The cache is keyed by file identity and engine version, and can always
be deleted.
