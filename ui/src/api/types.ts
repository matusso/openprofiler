// Types mirroring docs/spec/query-api.md. Keep in sync with the spec.

export type SessionKind = "hprof" | "jfr" | "snapshot" | "live" | "dex";
export type Capability =
  | "cpu"
  | "wall"
  | "alloc"
  | "monitor"
  | "wait"
  | "io"
  | "exception"
  | "telemetry"
  | "threads"
  | "heap"
  | "gc"
  | "thread_dumps"
  | "recording"
  | "decompile"
  | "mbeans";

export type SessionState = "loading" | "ready" | "live" | "detached" | "failed";

export interface Session {
  id: string;
  kind: SessionKind;
  name: string;
  path?: string;
  state: SessionState;
  progress?: number;
  error?: string;
  capabilities: Capability[];
  generation: number;
}

export interface LocalJvm {
  pid: number;
  main_class: string;
  args: string;
  vm_version?: string;
  attachable: boolean;
  profiled: boolean;
  user?: string;
}

export interface JvmInfo {
  name: string;
  vm_name: string;
  vm_version: string;
  vm_vendor: string;
  java_version: string;
  jvm_args: string[];
  main_class: string;
  os: string;
  arch: string;
  cpus?: number;
  max_heap?: number;
  pid?: number;
}

export interface HeapSummary {
  objects: number;
  classes: number;
  total_shallow: number;
  reachable_objects?: number;
  reachable_shallow?: number;
  edges?: number;
  gc_roots: number;
  id_size: number;
  dump_time?: number;
  analysis_state: "indexing" | "dominators" | "ready";
}

export interface Summary {
  session: Session;
  /** Absent for heap-only sessions. */
  jvm?: JvmInfo;
  start: number;
  end: number;
  sample_counts: Record<string, number>;
  threads: number;
  heap?: HeapSummary;
}

export type Unit = "nanos" | "bytes" | "count" | "percent" | "per_second";

export interface Series {
  key: string;
  label: string;
  unit: Unit;
  t: number[];
  v: number[];
}

export interface GcEventDto {
  start: number;
  duration: number;
  name: string;
  cause: string;
  pause?: number;
  heap_before?: number;
  heap_after?: number;
}

export interface BookmarkDto {
  time: number;
  label: string;
  color?: string;
}

export interface TelemetryRequest {
  keys?: string[];
  range?: [number, number];
  points?: number;
}

export interface TelemetryResponse {
  series: Series[];
  gc: GcEventDto[];
  bookmarks: BookmarkDto[];
}

export type TreeKind = "cpu" | "wall" | "alloc" | "monitor" | "wait" | "io" | "exception";
export type ThreadStateName = "runnable" | "waiting" | "blocked" | "net_io" | "sleeping" | "unknown";
export type Aggregation = "method" | "class" | "package";

export interface Filter {
  range?: [number, number];
  threads?: number[];
  states?: ThreadStateName[];
  /** Allocation/exception samples: only these classes (e.g. "byte[]"). */
  classes?: string[];
}

export type Focus = { callees: number } | { callers: number };

export interface TreeRequest {
  kind: TreeKind;
  view: "calltree" | "backtraces";
  filter?: Filter;
  aggregation?: Aggregation;
  collapse_recursion?: boolean;
  compact_prefixes?: string[];
  focus?: Focus;
  node?: number;
  depth?: number;
  min_fraction?: number;
}

export type FrameKindName = "interpreted" | "jit" | "inlined" | "native" | "kernel" | "cpp" | "unknown";

export interface TreeNodeDto {
  id: number;
  parent: number | null;
  key: number;
  depth: number;
  label: string;
  class?: string;
  method?: string;
  signature?: string;
  kind: FrameKindName;
  self: number;
  total: number;
  self_count: number;
  total_count: number;
  child_count: number;
}

export interface TreeResponse {
  total: number;
  total_count: number;
  unit: Unit;
  generation: number;
  nodes: TreeNodeDto[];
}

export interface HotspotsRequest {
  kind: TreeKind;
  filter?: Filter;
  aggregation?: Aggregation;
  compact_prefixes?: string[];
  limit?: number;
}

export interface HotspotDto {
  key: number;
  /** Root id of this hot spot in the backtraces tree with the same parameters. */
  node?: number;
  label: string;
  self: number;
  self_count: number;
  total: number;
  total_count: number;
}

export interface HotspotsResponse {
  total: number;
  unit: Unit;
  hotspots: HotspotDto[];
}

export interface FlameRequest extends Omit<TreeRequest, "view" | "node" | "depth"> {
  view?: TreeRequest["view"];
  inverted?: boolean;
}

export interface FlameLayout {
  total: number;
  max_depth: number;
  names: string[];
  node: number[];
  /** Tree key per rectangle (u32::MAX for the root and in diff layouts). */
  key?: number[];
  x: number[];
  width: number[];
  depth: number[];
  name: number[];
  self_weight: number[];
  kind: string[];
  delta?: number[];
}

export interface CallGraphRequest {
  kind: TreeKind;
  filter?: Filter;
  aggregation?: Aggregation;
  focus_key?: number;
  limit?: number;
}

export interface CallGraphResponse {
  nodes: { key: number; label: string; self: number; total: number }[];
  edges: { from: number; to: number; weight: number }[];
}

export interface HistogramResponse {
  start: number;
  end: number;
  values: number[];
}

export interface ThreadDto {
  index: number;
  java_id: number;
  name: string;
  group?: string;
  is_virtual: boolean;
  is_daemon: boolean;
  start?: number;
  end?: number;
  cpu?: number;
}

export interface ThreadSpan {
  thread: number;
  start: number;
  end: number;
  state: string;
}

export interface DumpedThread {
  name: string;
  java_id: number;
  state: string;
  is_virtual: boolean;
  frames: string[];
  locked: string[];
  waiting_on?: string | null;
}

export interface ThreadDump {
  time: number;
  label: string;
  threads: DumpedThread[];
}

export interface MonitorEventDto {
  start: number;
  duration: number;
  kind: "enter" | "wait" | "park";
  thread: number;
  owner?: number | null;
  monitor_class: string;
  monitor_id?: number | null;
  stack?: number;
}

export interface MonitorStat {
  monitor_class: string;
  count: number;
  total_duration: number;
  max_duration: number;
}

// ---- Heap ----

export interface ObjectSetDto {
  id: number;
  parent?: number;
  label: string;
  objects: number;
  shallow: number;
  retained?: number;
}

export interface ClassRow {
  class_id?: number;
  name: string;
  instances: number;
  shallow: number;
  retained?: number;
}

export interface ObjectRow {
  id: number;
  address: string;
  class_name: string;
  shallow: number;
  retained?: number | null;
  preview?: string | null;
  child_count?: number;
}

export interface RefRow {
  object: ObjectRow;
  field: string;
}

export interface PathStep {
  object: ObjectRow;
  field?: string;
  root_kind?: string;
}

export interface FieldDto {
  name: string;
  type: string;
  value: string;
  ref?: number;
}

export interface ObjectDetail {
  object: ObjectRow;
  fields: FieldDto[];
  array_length?: number;
  elements?: { index: number; value: string; ref?: number }[];
  gc_root_kinds: string[];
  dominator?: number;
}

export type SelectStep =
  | { class_id: number }
  | { class_name: string }
  | { package: string }
  | { retained_by: number }
  | { referenced_by_set: true }
  | { referencing_set: true }
  | { dominated_by_set: true }
  | { objects: number[] };

export type Severity = "info" | "warning" | "critical";

export interface Insight {
  kind: string;
  title: string;
  severity: Severity;
  summary: string;
  wasted_bytes?: number;
  count?: number;
  /** Where to inspect it, e.g. "heap.inspection:duplicate_strings". */
  view?: string;
}

export interface Paged<T> {
  total_rows: number;
  rows: T[];
}

export interface DiffNodeDto {
  id: number;
  parent: number | null;
  label: string;
  a_self: number;
  a_total: number;
  b_self: number;
  b_total: number;
  child_count: number;
}

export interface HistogramDiffRow {
  name: string;
  a_instances: number;
  b_instances: number;
  a_shallow: number;
  b_shallow: number;
}

export type CompareKind = TreeKind | "heap_histogram" | "telemetry";

export interface CompareRequest {
  a: string;
  b: string;
  kind: CompareKind;
  aggregation?: Aggregation;
  filter?: Filter;
  node?: number;
  depth?: number;
}

export type ServerEvent =
  | { type: "sessions" }
  | { type: "session"; id: string; state: SessionState; progress?: number; generation: number }
  | { type: "invalidate"; id: string; kinds: string[]; generation: number };

export interface EventsRequest {
  type?: string;
  range?: [number, number];
  search?: string;
  offset?: number;
  limit?: number;
}

export interface EventRow {
  type: string;
  start: number;
  duration: number;
  thread?: string | null;
  top_frame?: string | null;
  fields: Record<string, unknown>;
}

export interface EventsResponse {
  types: { name: string; count: number }[];
  total_rows: number;
  rows: EventRow[];
}

export interface NavigateRequest {
  class: string;
  method?: string;
  line?: number;
}

// ---------- Decompiler ----------

export interface CodeSummary {
  /** Decompiler name and version (`openprofiler-dex 0.1.0`). */
  engine: string;
  /** `jvm` for class files (the low-level view is JVM bytecode), `dex` for Android code. */
  format?: "dex" | "jvm";
  classes: number;
  packages: number;
  resources: number;
  dex_files: { name: string; version: number; classes: number }[];
  /** Problems found while loading (unreadable dex files, ...). */
  warnings: string[];
}

export interface CodePackageDto {
  /** Display name; single-child package chains are compacted (`com.example.app`). */
  name: string;
  full: string;
  classes: number;
}

export interface CodeClassRef {
  name: string;
  full: string;
  kind: "class" | "interface" | "enum" | "annotation" | "abstract";
}

export interface CodePackagesResponse {
  packages: CodePackageDto[];
  classes: CodeClassRef[];
}

export interface CodeNodeDto {
  id: number;
  kind: "class" | "method" | "field" | "other";
  name: string;
  full: string;
  /** Top-level class whose source holds the node. */
  cls?: string;
}

/** `[start, length, node id, 1 if declaration else 0]`, sorted by start. */
export type CodeLink = [number, number, number, 0 | 1];

export interface CodeClassResponse {
  name: string;
  package: string;
  view: "java" | "smali";
  code: string;
  links: CodeLink[];
  nodes: Record<string, CodeNodeDto>;
  /** Definition offset of the requested node. */
  focus?: number;
}

export interface CodeUsage {
  cls: string;
  pos: number;
  line: number;
  snippet: string;
  in?: string;
}

export interface CodeUsagesResponse {
  target: CodeNodeDto;
  usages: CodeUsage[];
  truncated: boolean;
}

export interface CodeTextMatch {
  kind: "code";
  cls: string;
  pos: number;
  line: number;
  snippet: string;
}

export interface CodeSearchResponse {
  results: (CodeNodeDto | CodeTextMatch)[];
  truncated: boolean;
  scanned: number;
  total: number;
}

export interface CodeResourceRow {
  name: string;
  type: string;
  size?: number;
}

export interface CodeResourceResponse {
  name: string;
  type: string;
  text?: string;
  /** Base64 image bytes. */
  image?: string;
  binary?: boolean;
  /** Files decoded from a container such as resources.arsc. */
  files?: string[];
}

// MBean browser (live sessions with the "mbeans" capability)

export interface MBeanSummary {
  name: string;
  domain: string;
  class_name: string;
  description: string;
}

export interface TrackedAttribute {
  key: string;
  label: string;
  unit: Unit;
  name: string;
  attribute: string;
  item?: string;
  server: string;
}

export interface MBeanServerInfo {
  /** "" for the platform server, the MBeanServerId for other local servers, the URL for remote ones. */
  id: string;
  kind: "platform" | "local" | "remote";
  label: string;
  default_domain: string;
  beans: number;
  error?: string;
}

export interface MBeanSubscription {
  server: string;
  name: string;
}

export interface MBeanNotification {
  seq: number;
  /** Epoch nanoseconds. */
  time: number;
  server: string;
  name: string;
  type: string;
  sequence_number: number;
  message: string;
  data: unknown;
}

export interface MBeanNotificationsResponse {
  notifications: MBeanNotification[];
  subscriptions: MBeanSubscription[];
  last: number;
  missed: number;
}

export interface MBeanListResponse {
  beans: MBeanSummary[];
  tracked: TrackedAttribute[];
}

export interface MBeanAttribute {
  name: string;
  type: string;
  description: string;
  readable: boolean;
  writable: boolean;
  value: unknown;
  error?: string;
}

export interface MBeanOperation {
  name: string;
  return_type: string;
  description: string;
  impact: "info" | "action" | "action_info" | "unknown";
  params: { name: string; type: string; description: string }[];
}

export interface MBeanDetail {
  name: string;
  class_name: string;
  description: string;
  attributes: MBeanAttribute[];
  operations: MBeanOperation[];
  notifications: { name: string; description: string; types: string[] }[];
}

export interface MBeanTrackRequest {
  server: string;
  name: string;
  attribute: string;
  item?: string;
  label?: string;
  unit?: Unit;
  enabled?: boolean;
}

// ---------- File browser ----------

export interface FsEntry {
  name: string;
  path: string;
  dir: boolean;
  size: number;
  /** Seconds since the Unix epoch. */
  modified?: number | null;
  /** What the daemon opens it as (by extension). */
  kind?: "profile" | "heap" | "code" | null;
}

/** A directory on the machine running the daemon. */
export interface FsListing {
  path: string;
  parent?: string | null;
  home?: string | null;
  entries: FsEntry[];
  truncated: boolean;
}
