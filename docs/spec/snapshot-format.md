# `.opsnap` snapshot format, version 1.0

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
  minor        u16       0 (additive changes only)
  flags        u32       0, reserved
  created      i64       epoch nanoseconds
  jvm_len      u32
  jvm          jvm_len bytes, protobuf openprofiler.snapshot.v1.JvmInfo

Section (repeated)
  kind         u32       see below
  len          u64       stored payload length
  codec        u8        0 = none, 1 = zstd (one frame), 2 = zstd frames
  payload      len bytes

Index           protobuf openprofiler.snapshot.v1.Index
Trailer
  index_offset u64
  index_len    u64
  magic        8 bytes   "OPRFSEND"
```

Readers start at the trailer, read the index, and load only the sections they need. Every index
entry has the section's offset, stored length, codec, uncompressed length and an XXH3-64
checksum of the stored payload. Readers must verify the checksum.

## Section kinds

| kind | name | payload (protobuf, `crates/openprofiler-snapshot/proto/snapshot.proto`) |
|---:|---|---|
| 1 | Meta | `Meta`: source, JVM info, time range, CPU sampling interval, properties |
| 2 | Symbols | `Symbols`: string table plus columnar method table (class, name, descriptor string ids) and frame table (method, line, kind) |
| 3 | Stacks | `Stacks`: prefix-trie nodes. Node `i+1` = (`parent[i]`, `frame[i]`), node 0 is the empty stack |
| 4 | Samples | `Samples`: one section per sample kind (cpu, wall, alloc, monitor, wait, io, exception), columnar, time delta-encoded |
| 5 | Threads | `Threads` |
| 6 | ThreadStates | `ThreadStates`: state intervals, columnar |
| 7 | Telemetry | `Telemetry`: named series (`heap.used`, `cpu.process`, …), time delta-encoded |
| 8 | Gc | `Gc` |
| 9 | Monitors | `Monitors` |
| 10 | Bookmarks | `Bookmarks` |
| 11 | ThreadDumps | `ThreadDumps` |
| 12 | HeapDump | Raw HPROF, codec 2 |
| 13 | Config | Reserved: recording configuration |
| 14 | Extension | Reserved: vendor or plugin data, identified by the index entry `name` |

Unknown section kinds must be ignored.

## Codec 2: independent zstd frames

```
frame_raw_size u32      uncompressed size of every frame except the last (8 MiB)
n              u32      number of frames
lengths        n × u64  compressed length of each frame
frames                  concatenated zstd frames
```

Frame `i` decompresses to raw bytes `[i × frame_raw_size, …)`. This allows parallel extraction
and random access without decompressing the whole dump.

## Derived data

Heap analysis indexes (object index, CSR graphs, dominator tree, retained sizes) are not part of
the file. They are cached next to it in `<file>.openprofiler-idx/`, or under `~/.cache/openprofiler/` when that
directory is not writable. The cache is keyed by file identity and engine version, and can always
be deleted.

## Not yet implemented

Arrow IPC encoding of the event and telemetry sections (plan D.6), which would let
DuckDB/Polars query snapshots directly, is planned as minor version 1.1 under a new codec. Until
then, use `openprofiler export --format csv|json`.
