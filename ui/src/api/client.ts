import type {
  CallGraphRequest,
  CallGraphResponse,
  ClassRow,
  CompareRequest,
  DiffNodeDto,
  FlameLayout,
  FlameRequest,
  EventsRequest,
  EventsResponse,
  Filter,
  NavigateRequest,
  HeapSummary,
  HistogramDiffRow,
  HistogramResponse,
  HotspotsRequest,
  HotspotsResponse,
  Insight,
  LocalJvm,
  MonitorEventDto,
  MonitorStat,
  ObjectDetail,
  ObjectRow,
  ObjectSetDto,
  Paged,
  PathStep,
  RefRow,
  SelectStep,
  ServerEvent,
  Session,
  Summary,
  TelemetryRequest,
  TelemetryResponse,
  ThreadDto,
  ThreadDump,
  ThreadSpan,
  TreeKind,
  TreeRequest,
  TreeResponse,
} from "./types";

export interface OpenProfilerConfig {
  url: string;
  token: string;
}

declare global {
  interface Window {
    __OPENPROFILER__?: Partial<OpenProfilerConfig>;
  }
}

/** Resolves the daemon URL + token: injected config, then `?api=&token=`, then same origin. */
export function resolveConfig(): OpenProfilerConfig {
  const injected = typeof window !== "undefined" ? window.__OPENPROFILER__ : undefined;
  const params = typeof window !== "undefined" ? new URLSearchParams(window.location.search) : null;
  const url = injected?.url ?? params?.get("api") ?? "";
  const token = injected?.token ?? params?.get("token") ?? sessionStorage.getItem("openprofiler.token") ?? "";
  if (token) sessionStorage.setItem("openprofiler.token", token);
  return { url: url.replace(/\/$/, ""), token };
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

export class OpenProfilerClient {
  constructor(public config: OpenProfilerConfig) {}

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.config.url}/api/v1${path}`, {
      method,
      headers: {
        ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
        ...(this.config.token ? { Authorization: `Bearer ${this.config.token}` } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    if (!res.ok) {
      let msg = res.statusText;
      try {
        const j = (await res.json()) as { error?: string };
        if (j.error) msg = j.error;
      } catch {
        // non-JSON error body
      }
      throw new ApiError(res.status, msg);
    }
    const text = await res.text();
    return (text ? JSON.parse(text) : {}) as T;
  }

  get = <T>(path: string) => this.request<T>("GET", path);
  post = <T>(path: string, body: unknown = {}) => this.request<T>("POST", path, body);

  // Sessions
  sessions = () => this.get<Session[]>("/sessions");
  open = (path: string) => this.post<Session>("/sessions/open", { path });
  attach = (pid: number, options?: Record<string, unknown>) =>
    this.post<Session>("/sessions/attach", { pid, options });
  close = (id: string) => this.request<unknown>("DELETE", `/sessions/${id}`);
  jvms = () => this.get<LocalJvm[]>("/jvms");
  summary = (id: string) => this.get<Summary>(`/sessions/${id}/summary`);

  // Telemetry & trees
  telemetry = (id: string, req: TelemetryRequest) =>
    this.post<TelemetryResponse>(`/sessions/${id}/telemetry`, req);
  tree = (id: string, req: TreeRequest) => this.post<TreeResponse>(`/sessions/${id}/tree`, req);
  hotspots = (id: string, req: HotspotsRequest) =>
    this.post<HotspotsResponse>(`/sessions/${id}/hotspots`, req);
  flame = (id: string, req: FlameRequest) => this.post<FlameLayout>(`/sessions/${id}/flame`, req);
  callgraph = (id: string, req: CallGraphRequest) =>
    this.post<CallGraphResponse>(`/sessions/${id}/callgraph`, req);
  histogram = (id: string, req: { kind: TreeKind; filter?: Filter; buckets?: number }) =>
    this.post<HistogramResponse>(`/sessions/${id}/histogram`, req);

  // Threads & monitors
  threads = (id: string) => this.get<{ threads: ThreadDto[] }>(`/sessions/${id}/threads`);
  threadStates = (id: string, req: { range?: [number, number]; threads?: number[]; max_spans?: number }) =>
    this.post<{ spans: ThreadSpan[] }>(`/sessions/${id}/thread-states`, req);
  threadDumps = (id: string) => this.get<ThreadDump[]>(`/sessions/${id}/thread-dumps`);
  captureThreadDump = (id: string) => this.post<ThreadDump>(`/sessions/${id}/thread-dumps`);
  monitorEvents = (id: string, range?: [number, number]) =>
    this.post<{ events: MonitorEventDto[] }>(`/sessions/${id}/monitors`, { range, view: "events" });
  monitorStats = (id: string, range?: [number, number]) =>
    this.post<{ stats: MonitorStat[] }>(`/sessions/${id}/monitors`, { range, view: "stats" });

  // Heap
  heapSummary = (id: string) => this.get<HeapSummary>(`/sessions/${id}/heap/summary`);
  heapClasses = (
    id: string,
    req: {
      set?: number;
      group_by?: "class" | "package" | "classloader";
      sort?: "instances" | "shallow" | "retained" | "name";
      filter?: string;
      offset?: number;
      limit?: number;
    },
  ) => this.post<Paged<ClassRow>>(`/sessions/${id}/heap/classes`, req);
  heapObjects = (
    id: string,
    req: { set?: number; class_id?: number; sort?: "shallow" | "retained"; offset?: number; limit?: number },
  ) => this.post<Paged<ObjectRow>>(`/sessions/${id}/heap/objects`, req);
  heapBiggest = (id: string, req: { object?: number; limit?: number }) =>
    this.post<{ rows: ObjectRow[] }>(`/sessions/${id}/heap/biggest`, req);
  heapDominatorClasses = (id: string, object?: number) =>
    this.post<{ rows: { name: string; objects: number; retained: number }[] }>(
      `/sessions/${id}/heap/dominator-classes`,
      { object },
    );
  heapRefs = (
    id: string,
    req: { object: number; direction: "incoming" | "outgoing"; offset?: number; limit?: number },
  ) => this.post<Paged<RefRow>>(`/sessions/${id}/heap/refs`, req);
  heapMergedRefs = (id: string, req: { set: number; direction: "incoming" | "outgoing" }) =>
    this.post<{ rows: { class_name: string; field: string; objects: number; refs: number }[] }>(
      `/sessions/${id}/heap/merged-refs`,
      req,
    );
  heapPathToRoots = (id: string, req: { object: number; max_paths?: number; exclude_weak?: boolean }) =>
    this.post<{ paths: PathStep[][] }>(`/sessions/${id}/heap/path-to-roots`, req);
  heapObject = (id: string, obj: number) => this.get<ObjectDetail>(`/sessions/${id}/heap/object/${obj}`);
  heapSelect = (id: string, req: { from?: number; step: SelectStep }) =>
    this.post<ObjectSetDto>(`/sessions/${id}/heap/select`, req);
  heapSets = (id: string) => this.get<ObjectSetDto[]>(`/sessions/${id}/heap/sets`);
  heapSetRetained = (id: string, set: number) =>
    this.post<{ retained: number }>(`/sessions/${id}/heap/set-retained`, { set });
  heapInspections = (id: string) => this.get<{ inspections: Insight[] }>(`/sessions/${id}/heap/inspections`);
  heapInspection = (id: string, kind: string, limit?: number) =>
    this.post<{ insight: Insight; rows: unknown[] }>(`/sessions/${id}/heap/inspection`, { kind, limit });
  heapHistogramDiff = (id: string, other: string) =>
    this.post<{ rows: HistogramDiffRow[] }>(`/sessions/${id}/heap/histogram-diff`, { other_session: other });

  // Event browser & IDE bridge
  events = (id: string, req: EventsRequest) => this.post<EventsResponse>(`/sessions/${id}/events`, req);
  ideNavigate = (req: NavigateRequest) => this.post<{ delivered: number }>("/ide/navigate", req);

  // Compare & insights
  compare = (req: CompareRequest) => this.post<{ nodes: DiffNodeDto[] }>("/compare", req);
  compareFlame = (req: CompareRequest) => this.post<FlameLayout>("/compare/flame", req);
  insights = (id: string) => this.get<{ insights: Insight[] }>(`/sessions/${id}/insights`);

  // Live control
  setRecording = (id: string, kind: string, enabled: boolean, interval_ms?: number) =>
    this.post(`/sessions/${id}/recording`, { kind, enabled, interval_ms });
  heapDump = (id: string) => this.post<Session>(`/sessions/${id}/heap-dump`);
  saveSnapshot = (id: string, path: string) => this.post(`/sessions/${id}/snapshot`, { path });
  bookmark = (id: string, label: string) => this.post(`/sessions/${id}/bookmark`, { label });
  gc = (id: string) => this.post(`/sessions/${id}/gc`);

  /** URL for a file export (GET-able via a form POST is not possible, so we fetch + blob). */
  async export(id: string, body: { view: string; format: string; request: unknown }): Promise<Blob> {
    const res = await fetch(`${this.config.url}/api/v1/sessions/${id}/export`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...(this.config.token ? { Authorization: `Bearer ${this.config.token}` } : {}),
      },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new ApiError(res.status, res.statusText);
    return res.blob();
  }

  /** Subscribes to server events; reconnects with backoff. Returns an unsubscribe function. */
  subscribe(onEvent: (e: ServerEvent) => void, onStatus?: (connected: boolean) => void): () => void {
    let ws: WebSocket | null = null;
    let closed = false;
    let delay = 500;
    const connect = () => {
      if (closed) return;
      const base = this.config.url || window.location.origin;
      const wsUrl =
        base.replace(/^http/, "ws") + `/api/v1/events?token=${encodeURIComponent(this.config.token)}`;
      try {
        ws = new WebSocket(wsUrl);
      } catch {
        setTimeout(connect, delay);
        return;
      }
      ws.onopen = () => {
        delay = 500;
        onStatus?.(true);
      };
      ws.onmessage = (m) => {
        try {
          onEvent(JSON.parse(m.data as string) as ServerEvent);
        } catch {
          // ignore malformed
        }
      };
      ws.onclose = () => {
        onStatus?.(false);
        if (!closed) {
          setTimeout(connect, delay);
          delay = Math.min(delay * 2, 10_000);
        }
      };
    };
    connect();
    return () => {
      closed = true;
      ws?.close();
    };
  }
}
