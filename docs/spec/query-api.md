# OpenProfiler Query API v1

The Query API is the single contract between `openprofiler-daemon` and every client: the desktop/web UI,
the `openprofiler` CLI exporter, the MCP server and the IDE bridge. It behaves identically for live
sessions, `.opsnap` snapshots, HPROF heap dumps and JFR recordings. Android apps (`.apk`, `.dex`, …)
open as `dex` sessions served by the [decompiler](#decompiler) endpoints.

Transport: JSON over HTTP (`/api/v1/...`) plus a WebSocket (`/api/v1/events`) that pushes
invalidations. All requests carry `Authorization: Bearer <token>` (or `?token=` for the
WebSocket and for downloads). The token is printed by `openprofiler daemon` and injected into the
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
| `POST /api/v1/sessions/attach` | `{ "pid"?: number, "options"?: AttachOptions, "infofile"?: string }` | `Session` |
| `DELETE /api/v1/sessions/{id}` | | `{}` |
| `GET /api/v1/jvms` | | `LocalJvm[]` |
| `GET /api/v1/sessions/{id}/summary` | | `Summary` |
| `POST /api/v1/fs/list` | `{ "path"?: string, "hidden"?: boolean }` | `FsListing` |

`fs/list` lists a directory on the daemon's machine for the Open dialog (the home directory when
`path` is omitted; a file path lists its directory). Entries are directories first, then files;
`kind` is what `sessions/open` would open the file as, judged by its extension.

```ts
type FsEntry = { name: string; path: string; dir: boolean; size: number; modified?: number;
                 kind?: "profile" | "heap" | "code" };
type FsListing = { path: string; parent?: string; home?: string; entries: FsEntry[];
                   truncated: boolean };
```

```ts
type SessionKind = "hprof" | "jfr" | "snapshot" | "live" | "dex";
type Capability = "cpu" | "wall" | "alloc" | "monitor" | "wait" | "io" | "exception"
                | "calls" | "probe"
                | "telemetry" | "threads" | "heap" | "gc" | "thread_dumps" | "recording"
                | "decompile" | "mbeans" | "probes" | "instrument";
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
  // Absent for heap-only and dex sessions.
  jvm?: { name, vm_name, vm_version, vm_vendor, java_version, jvm_args: string[], main_class, os, arch,
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
  kind: "cpu" | "wall" | "alloc" | "monitor" | "wait" | "io" | "exception" | "calls" | "probe";
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

## Probes

Kind `calls` trees are exact instrumented call counts (weight = self nanoseconds, `count` =
invocations). Kind `probe` trees are probe calls (weight = nanoseconds, `count` = calls); filter
them with `filter.classes = [payload]` for the call tree or back traces of one SQL statement or URL.

`POST /api/v1/sessions/{id}/probes` (live sessions and snapshots):

```ts
interface ProbesRequest {
  view: "list" | "hotspots" | "events" | "timeline" | "telemetry" | "controls";
  probe?: string;              // probe id; required except for list and timeline
  range?: [number, number];
  payload?: string;            // events: only this payload
  search?: string;             // events, hotspots: payloads containing this (case-insensitive)
  errors_only?: boolean;       // events
  limit?: number;              // default 1000
  open_threshold_ms?: number;  // controls: flag objects open longer than this (default 60000)
}
```

| view | response |
|---|---|
| `list` | `{ probes: { id, name, payload_label, control_label?, calls, total, errors, payloads, events, open_controls, leaked_controls }[] }` |
| `hotspots` | `{ total, hotspots: { payload, calls, total, mean, max, errors, threads }[] }` by total time |
| `events` | `{ events: { start, duration, payload, thread, error?, detail?, context?, control?, top_frame }[] }` |
| `timeline` | `{ lanes: { probe, name, time[], calls[], duration[], errors[] }[] }` per second |
| `telemetry` | `{ series: TelemetrySeries[] }`: `calls` (per second) and `mean` (nanoseconds) |
| `controls` | `{ controls: { id, description, opened, closed?, open_for, thread, top_frame, state: "open" \| "long_open" \| "closed" \| "leaked" }[] }` |

`context` is the request the call was made for (`GET /owners/{id}`), also across threads.
`leaked` control objects were garbage collected without being closed.

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

## Decompiler

Available when the session has the `decompile` capability: `POST /sessions/open` with an `.apk`
(also `.xapk`, `.apks`, `.apkm`, `.aab`) or `.dex` file, JVM bytecode (`.jar`, `.war`, `.ear`,
`.jmod`, `.aar`, `.class`), or a directory of `.class` files. The daemon decompiles in process
with `openprofiler-dex`; opening indexes the classes, and each class is decompiled on first
request and cached. The session stays `loading` while the input is indexed and goes `failed`
with an `error` for unreadable input. `code/summary` has `format`: `jvm` for class files (the
`smali` view is then JVM bytecode) or `dex`.

Paths are under `/api/v1/sessions/{id}/code/`. Node ids are small integers assigned by the
daemon when a class, method or field first appears in a response; they are stable for the life of
the session.

| Method & path | Body | Result |
|---|---|---|
| `GET .../code/summary` | | `{ engine, classes, packages, resources, dex_files: { name, version, classes }[], warnings: string[] }` |
| `POST .../code/packages` | `{ parent?: string }` (`""` = top level) | `{ packages: { name, full, classes }[], classes: ClassRef[] }` |
| `POST .../code/class` | `{ name: string, view?: "java"\|"smali" }` or `{ node: number }` | `ClassCode` |
| `POST .../code/usages` | `{ node: number, limit?: number }` | `{ target: CodeNode, usages: Usage[], truncated }` |
| `POST .../code/search` | `{ query: string, scope?: "names"\|"code", limit?, budget_ms? }` | `{ results: (CodeNode \| TextMatch)[], truncated, scanned, total }` |
| `GET .../code/resources` | | `{ resources: { name, type, size }[] }` |
| `POST .../code/resource` | `{ name: string }` (`"resources.arsc!res/values/strings.xml"` for decoded sub-files) | `Resource` |

```ts
interface ClassRef { name: string; full: string; kind: "class" | "interface" | "enum" | "annotation" | "abstract" }
interface CodeNode { id: number; kind: "class" | "method" | "field"; name: string; full: string;
                     cls?: string /* top-level class whose source declares it */ }
interface ClassCode {
  name: string; package: string; view: "java" | "smali"; code: string;
  links: [start: number, length: number, node: number, decl: 0 | 1][];  // offsets into code, sorted
  nodes: { [id: string]: CodeNode };
  focus?: number;  // with { node }: offset of that node's declaration
}
interface Usage { cls: string; pos: number; line: number; snippet: string; in?: string /* enclosing member */ }
interface TextMatch { kind: "code"; cls: string; pos: number; line: number; snippet: string }
interface Resource { name: string; type: string; text?: string; image?: string /* base64 */;
                     binary?: boolean; files?: string[] /* decoded sub-files */ }
```

- Package chains with a single child and no classes are compacted (`com.example.app`); `full` is
  the package to pass as `parent` next.
- `links` mark references (`decl: 0`, click → declaration) and declarations (`decl: 1`, click →
  usages). Smali has no links.
- `search` with `scope: "names"` matches class, method and field names without decompiling. With
  `scope: "code"` it decompiles classes in parallel and searches their text until `limit` results
  or `budget_ms` (default 20 000) are reached; `truncated` then reports an incomplete scan.
- `AndroidManifest.xml` and binary XML are decoded to text; `resources.arsc` returns its public
  ids plus the list of decoded `res/values*/...` files; images up to 4 MB come back as base64.

## Compare

`POST /api/v1/compare` `{ a: sessionId, b: sessionId, kind: "cpu"|"alloc"|...|"heap_histogram"|"telemetry",
aggregation?, filter? }` → `{ nodes: DiffNode[] }` (`{ id, parent, label, a_self, a_total, b_self, b_total, child_count }`, pre-order, depth-limited like trees, `node`/`depth` supported) or histogram rows.

`POST /api/v1/compare/flame` → `FlameLayout` with `delta` (laid out by `b`, coloured by `b - a`).

## Insights

`GET /api/v1/sessions/{id}/insights` → `{ insights: Insight[] }`, rule-based findings across CPU,
memory, threads and heap (e.g. "Duplicate strings waste 120 MB", "GC overhead 23%",
"Lock contention on `app.Cache`").

## Live control (live sessions)

- `POST .../recording` `{ kind: "cpu"|"alloc"|"monitor"|"telemetry"|"calls"|"probes", enabled: boolean, interval_ms? }`
- `POST .../heap-dump` → creates a new `hprof` session from a dump streamed back by the agent.
- `POST .../snapshot` `{ path }` → writes a `.opsnap` file.
- `POST .../bookmark` `{ label }`
- `POST .../gc` → runs a full GC.
- `POST .../triggers` `{ triggers: Trigger[] }` → replaces the agent's triggers (format in
  [agent-link.md](agent-link.md#triggers)); returns `{ installed }`.
- `GET .../instrumentation` → the agent's instrumentation settings; `POST` with
  `{ include?: string[], exclude?: string[], probes?: string[] }` replaces them (class patterns
  such as `com.acme.*` get exact call counts; `probes` empty = all, `["none"]` = none) and
  retransforms affected classes. Both return `{ include, exclude, available_probes: { id, name,
  payload_label, control_label? }[], enabled_probes, instrumented_classes, instrumented_methods,
  disabled_methods }` (`disabled_methods`: removed by auto-tuning). Needs the `instrument`
  capability.

## MBeans (live sessions with the `mbeans` capability)

The MBean servers of the attached JVM (see [agent-link.md](agent-link.md#mbeans) for servers,
value and argument conversion, and notifications). `server` is an `MBeanServerInfo.id`; omitted or
`""` means the platform MBean server. Errors from the JVM are returned as
`{"error": "agent: <exception>"}`.

| Method & path | Body | Result |
|---|---|---|
| `POST .../mbean-servers` | `{}` | `{ servers: MBeanServerInfo[] }` |
| `POST .../mbean-connect` | `{ url, username?, password?, enabled?: boolean }` (JMX service URL) | `{ servers: MBeanServerInfo[] }` |
| `POST .../mbeans` | `{ server?, pattern?: string }` (ObjectName pattern, e.g. `java.lang:*`) | `{ beans: MBeanSummary[], tracked: TrackedAttribute[] }` |
| `POST .../mbean` | `{ server?, name }` | `MBeanDetail` |
| `POST .../mbean-invoke` | `{ server?, name, operation, signature: string[], args: string[] }` | `{ value: Json }` |
| `POST .../mbean-set` | `{ server?, name, attribute, value: string }` | `{}` |
| `POST .../mbean-track` | `{ server?, name, attribute, item?, label?, unit?: Unit, enabled?: boolean }` | `{ tracked: TrackedAttribute[] }` |
| `POST .../mbean-subscribe` | `{ server?, name, enabled?: boolean }` | `{ subscriptions: { server, name }[] }` |
| `POST .../mbean-notifications` | `{ after?: number }` | `MBeanNotifications` |

```ts
interface MBeanServerInfo { id: string; kind: "platform" | "local" | "remote"; label: string;
                            default_domain: string; beans: number; error?: string }
interface MBeanSummary { name: string; domain: string; class_name: string; description: string }
interface MBeanDetail {
  name: string; class_name: string; description: string;
  attributes: { name: string; type: string; description: string; readable: boolean;
                writable: boolean; value: Json; error?: string }[];
  operations: { name: string; return_type: string; description: string;
                impact: "info" | "action" | "action_info" | "unknown";
                params: { name: string; type: string; description: string }[] }[];
  notifications: { name: string; description: string; types: string[] }[];
}
interface TrackedAttribute { key: string; label: string; unit: Unit; name: string;
                             attribute: string; item?: string; server: string }
interface MBeanNotifications {
  notifications: { seq: number; time: number /* epoch ns */; server: string; name: string;
                   type: string; sequence_number: number; message: string; data: Json }[];
  subscriptions: { server: string; name: string }[];
  last: number;    // pass as the next `after`
  missed: number;  // notifications after `after` dropped from the agent's buffer (last 1000)
}
```

`mbean-track` samples a numeric attribute (or `item` of a composite attribute) once per second as
the telemetry series `key` (`mbean:<name>#<attribute>[.<item>]`), which `POST .../telemetry`
returns like any other series; `enabled: false` stops sampling. `unit` defaults to `count`.

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
- `agent {}` returns `{path, options}`: the native agent the daemon uses and the options to launch a
  JVM with, as `-agentpath:<path>=<options>,infofile=<file>`.
- `attachLaunched {infofile}` waits up to 60 s for the agent of a JVM launched that way to publish
  `infofile`, connects to it and returns the new live `Session`. `POST /api/v1/sessions/attach`
  accepts the same `infofile` instead of a `pid`.
- Requests are handled concurrently, so a pending `attachLaunched` does not delay other requests.
- The daemon sends a `navigate {class, method?, line?}` notification when a client calls
  `POST /api/v1/ide/navigate` with the same body. The response is `{delivered: number}`.
- The daemon publishes its URL and token in `~/.openprofiler/daemon.json` (mode 0600) for IDE plugins.
