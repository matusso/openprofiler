# Ember Query API v1

The Query API is the single contract between `ember-daemon` and every client: the desktop/web UI,
the `ember` CLI exporter, the MCP server and the IDE bridge. It behaves identically for live
sessions, `.ember` snapshots, HPROF heap dumps and JFR recordings.

Transport: JSON over HTTP (`/api/v1/...`) plus a WebSocket (`/api/v1/events`) that pushes
invalidations. All requests carry `Authorization: Bearer <token>` (or `?token=` for the
WebSocket and for downloads). The token is printed by `ember daemon` and injected into the
Tauri webview.

Conventions:

- Times are epoch **nanoseconds** as JSON numbers (`i64`; safe up to 2^53 in JS for the next
  ~100 years only at microsecond precision — clients treat them as opaque ordinals and divide by
  1e6 for display).
- Object ids in heap sessions are dense `u32` indices assigned by the heap index, not addresses.
  The original address is exposed as a hex string `address`.
- Tree node ids are dense `u32`s; they are stable for identical request parameters on the same
  session generation. `generation` increments when a live session's data changes.
- Errors: HTTP 4xx/5xx with `{"error": "message"}`.

## Sessions

| Method & path | Body | Result |
|---|---|---|
| `GET /api/v1/sessions` | | `Session[]` |
| `POST /api/v1/sessions/open` | `{ "path": string }` | `Session` |
| `POST /api/v1/sessions/attach` | `{ "pid": number, "options"?: AttachOptions }` | `Session` |
| `DELETE /api/v1/sessions/{id}` | | `{}` |
| `GET /api/v1/jvms` | | `LocalJvm[]` |
| `GET /api/v1/sessions/{id}/summary` | | `Summary` |

```ts
type SessionKind = "hprof" | "jfr" | "ember" | "live";
type Capability = "cpu" | "wall" | "alloc" | "monitor" | "wait" | "io" | "exception"
                | "telemetry" | "threads" | "heap" | "gc" | "thread_dumps" | "recording";
interface Session {
  id: string; kind: SessionKind; name: string; path?: string;
  state: "loading" | "ready" | "live" | "detached" | "failed";
  progress?: number;            // 0..1 while loading
  error?: string;
  capabilities: Capability[];
  generation: number;
}
interface LocalJvm {
  pid: number; main_class: string; args: string; vm_version?: string;
  attachable: boolean; profiled: boolean; user?: string;
}
interface Summary {
  session: Session;
  jvm: { name, vm_name, vm_version, vm_vendor, java_version, jvm_args: string[], main_class, os, arch,
         cpus?: number, max_heap?: number, pid?: number };
  start: number; end: number;
  sample_counts: { [kind: string]: number };
  threads: number;
  heap?: HeapSummary;
}
```

## Telemetry

`POST /api/v1/sessions/{id}/telemetry`

```ts
// request
{ keys?: string[], range?: [number, number], points?: number /* default 800, LTTB */ }
// response
{ series: { key: string, label: string, unit: Unit, t: number[], v: number[] }[],
  gc: { start: number, duration: number, name: string, cause: string, pause?: number,
        heap_before?: number, heap_after?: number }[],
  bookmarks: { time: number, label: string, color?: string }[] }
type Unit = "nanos" | "bytes" | "count" | "percent" | "per_second";
```

Well-known keys: `heap.used`, `heap.committed`, `heap.max`, `nonheap.used`, `gc.pause`,
`gc.activity`, `cpu.process`, `cpu.system`, `threads.live`, `threads.daemon`, `classes.loaded`,
`alloc.rate`, `samples.cpu` (derived from samples when no explicit series exists).

## Trees (CPU, allocation, monitor, …)

`POST /api/v1/sessions/{id}/tree`

```ts
interface TreeRequest {
  kind: "cpu" | "wall" | "alloc" | "monitor" | "wait" | "io" | "exception";
  view: "calltree" | "backtraces";       // backtraces = inverted (hot spots are its roots)
  filter?: Filter;
  aggregation?: "method" | "class" | "package";
  collapse_recursion?: boolean;
  compact_prefixes?: string[];           // e.g. ["java.", "jdk.internal."]
  focus?: { callees: number } | { callers: number };   // node key
  node?: number;                         // expand this node (default: root)
  depth?: number;                        // levels to return below `node` (default 1)
  min_fraction?: number;                 // auto-expand children above this share (0..1)
}
interface Filter {
  range?: [number, number];
  threads?: number[];                    // thread indices from /threads
  states?: ("runnable" | "waiting" | "blocked" | "net_io" | "sleeping" | "unknown")[];
  classes?: string[];                    // allocation/exception samples: only these classes ("byte[]")
}
interface TreeResponse {
  total: number; total_count: number; unit: Unit; generation: number;
  nodes: TreeNodeDto[];                  // `node` itself first, then descendants (pre-order)
}
interface TreeNodeDto {
  id: number; parent: number | null; key: number; depth: number;
  label: string;                         // "java.util.HashMap.get"
  class?: string; method?: string; signature?: string;
  kind: "interpreted" | "jit" | "inlined" | "native" | "kernel" | "cpp" | "unknown";
  self: number; total: number; self_count: number; total_count: number;
  child_count: number;
}
```

`POST /api/v1/sessions/{id}/hotspots` — `{ kind, filter?, aggregation?, compact_prefixes?, limit? }`
→ `{ total, unit, hotspots: { key, label, self, self_count, total, total_count }[] }`.
Expanding a hot spot's back traces is a `tree` request with `view: "backtraces"` and
`node` = the hot spot's root node id (or `focus: { callers: key }`).

`POST /api/v1/sessions/{id}/flame` — `TreeRequest` fields (`node`/`depth` ignored) plus
`inverted?: boolean` → `FlameLayout`:

```ts
interface FlameLayout {
  total: number; max_depth: number; names: string[];
  node: number[]; key: number[]; x: number[]; width: number[]; depth: number[]; name: number[];
  self_weight: number[]; kind: string[]; delta?: number[];
}
```

`POST /api/v1/sessions/{id}/callgraph` — `{ kind, filter?, aggregation?, focus_key?, limit? }` →
`{ nodes: { key, label, self, total }[], edges: { from: number, to: number, weight: number }[] }`
(indices into `nodes`). With `focus_key`, only the neighbourhood (callers and callees, 2 levels) is
returned.

`POST /api/v1/sessions/{id}/histogram` — `{ kind, filter?, buckets? }` → `{ start, end, values: number[] }`.

## Threads & monitors

- `GET /api/v1/sessions/{id}/threads` → `{ threads: { index, java_id, name, group?, is_virtual,
  is_daemon, start?, end?, cpu?: number }[] }`
- `POST /api/v1/sessions/{id}/thread-states` `{ range?, threads?, max_spans? }` →
  `{ spans: { thread: number, start: number, end: number, state: string }[] }`
- `GET /api/v1/sessions/{id}/thread-dumps` → `ThreadDump[]`; `POST .../thread-dumps` captures
  one (live only).
- `POST /api/v1/sessions/{id}/monitors` `{ range?, view: "events" | "stats" }` → events or
  per-monitor-class stats `{ monitor_class, count, total_duration, max_duration }[]`.

## Heap

Available when the session has the `heap` capability. Object sets are referenced by id; set `0`
is "all objects". New sets are created by selection steps and returned as `ObjectSetDto`.

```ts
interface HeapSummary {
  objects: number; classes: number; total_shallow: number; gc_roots: number;
  id_size: number; dump_time?: number; analysis_state: "indexing" | "dominators" | "ready";
}
interface ObjectSetDto { id: number; parent?: number; label: string; objects: number; shallow: number; retained?: number }
```

| Method & path | Body | Result |
|---|---|---|
| `GET .../heap/summary` | | `HeapSummary` |
| `POST .../heap/classes` | `{ set?, group_by?: "class"\|"package"\|"classloader", sort?: "instances"\|"shallow"\|"retained"\|"name", filter?: string, offset?, limit? }` | `{ total_rows, rows: { class_id?, name, instances, shallow, retained? }[] }` |
| `POST .../heap/objects` | `{ set?, class_id?, sort?: "shallow"\|"retained", offset?, limit? }` | `{ total_rows, rows: ObjectRow[] }` |
| `POST .../heap/biggest` | `{ object?: number, limit? }` (no object = dominator-tree roots) | `{ rows: ObjectRow[] }` (children in dominator tree, by retained) |
| `POST .../heap/dominator-classes` | `{ object?: number }` | `{ rows: { name, objects, retained }[] }` (dominator tree grouped by class, for sunburst) |
| `POST .../heap/refs` | `{ object: number, direction: "incoming"\|"outgoing", offset?, limit? }` | `{ total_rows, rows: RefRow[] }` |
| `POST .../heap/merged-refs` | `{ set: number, direction: "incoming"\|"outgoing" }` | `{ rows: { class_name, field, objects, refs }[] }` |
| `POST .../heap/path-to-roots` | `{ object: number, max_paths?: number, exclude_weak?: boolean }` | `{ paths: PathStep[][] }` (each path from object to root) |
| `GET .../heap/object/{obj}` | | `ObjectDetail` |
| `POST .../heap/select` | `{ from?: number, step: SelectStep }` | `ObjectSetDto` |
| `GET .../heap/sets` | | `ObjectSetDto[]` |
| `GET .../heap/inspections` | | `{ inspections: Insight[] }` |
| `POST .../heap/inspection` | `{ kind: string, limit? }` | `{ insight: Insight, rows: any[] }` |
| `POST .../heap/histogram-diff` | `{ other_session: string }` | `{ rows: { name, a_instances, b_instances, a_shallow, b_shallow }[] }` |

```ts
interface ObjectRow { id: number; address: string; class_name: string; shallow: number; retained?: number;
                      preview?: string; /* string value, array length, … */ child_count?: number }
interface RefRow { object: ObjectRow; field: string; /* "value", "[12]", "<root: JNI global>" */ }
interface PathStep { object: ObjectRow; field?: string; root_kind?: string }
interface ObjectDetail { object: ObjectRow; fields: { name: string, type: string, value: string,
                         ref?: number }[]; array_length?: number; elements?: { index: number, value: string, ref?: number }[];
                         gc_root_kinds: string[]; dominator?: number }
type SelectStep =
  | { class_id: number } | { class_name: string }          // instances of a class
  | { package: string }
  | { retained_by: number }                                 // objects retained by an object
  | { referenced_by_set: true } | { referencing_set: true } // outgoing/incoming of `from`
  | { dominated_by_set: true }
  | { objects: number[] };
interface Insight { kind: string; title: string; severity: "info" | "warning" | "critical";
                    summary: string; wasted_bytes?: number; count?: number }
```

Inspection kinds: `duplicate_strings`, `duplicate_arrays`, `empty_collections`,
`sparse_arrays`, `zero_arrays`, `bad_hash_distribution`, `thread_locals`,
`classloader_leaks`, `big_collections`, `inner_class_retention`.

## Compare

`POST /api/v1/compare` `{ a: sessionId, b: sessionId, kind: "cpu"|"alloc"|...|"heap_histogram"|"telemetry",
aggregation?, filter? }` → `{ nodes: DiffNode[] }` (`{ id, parent, label, a_self, a_total, b_self, b_total, child_count }`, pre-order, depth-limited like trees, `node`/`depth` supported) or histogram rows.

`POST /api/v1/compare/flame` → `FlameLayout` with `delta` (laid out by `b`, coloured by `b - a`).

## Insights

`GET /api/v1/sessions/{id}/insights` → `{ insights: Insight[] }`, rule-based findings across CPU,
memory, threads and heap (e.g. "Duplicate strings waste 120 MB", "GC overhead 23%",
"Lock contention on `app.Cache`").

## Live control (live sessions)

- `POST .../recording` `{ kind: "cpu"|"alloc"|"monitor"|"telemetry", enabled: boolean, interval_ms? }`
- `POST .../heap-dump` → creates a new `hprof` session from a dump streamed back by the agent.
- `POST .../snapshot` `{ path }` → writes a `.ember` file.
- `POST .../bookmark` `{ label }`
- `POST .../gc` → runs a full GC.

## Export

`POST /api/v1/sessions/{id}/export` `{ view: "calltree"|"hotspots"|"flame"|"classes"|"biggest"|"telemetry"|"threads",
format: "csv"|"json"|"html"|"svg"|"collapsed", request: <view request> }` → file body with the
matching `Content-Type`.

## Events (WebSocket `/api/v1/events?token=…`)

Server → client messages, JSON:

```ts
{ type: "sessions" }                                       // session list changed
{ type: "session", id: string, state, progress?, generation }
{ type: "invalidate", id: string, kinds: string[], generation: number }  // refetch visible views
```

## Event browser

`POST /api/v1/sessions/{id}/events` (JFR sessions)

```ts
// request
{ type?: string /* e.g. "jdk.GarbageCollection"; omit to list types only */,
  range?: [number, number], search?: string, offset?: number, limit?: number /* default 200, max 5000 */ }
// response
{ types: { name: string, count: number }[], total_rows: number,
  rows: { type: string, start: number, duration: number, thread?: string, top_frame?: string,
          fields: { [name: string]: any } }[] }
// Field values: numbers, strings, booleans, nested objects, arrays,
// { timestamp: ns } and { duration: ns }.
```

## IDE bridge

WebSocket `/api/v1/ide?token=…`, JSON-RPC 2.0.

- Requests: `hello`, `sessions`, `openFile {path}`, `gutterData {session?, classes[], kind?}`
  (returns `{lines: {class, method, line, self_percent, total_percent}[]}`) and
  `profiledPackages {session?}`.
- The daemon sends a `navigate {class, method?, line?}` notification when a client calls
  `POST /api/v1/ide/navigate` with the same body. The response is `{delivered: number}`.
- The daemon publishes its URL and token in `~/.ember/daemon.json` (mode 0600) for IDE plugins.
