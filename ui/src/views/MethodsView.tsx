import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import type {
  CallsRequest,
  ComplexityFit,
  ComplexityMethod,
  ComplexityResponse,
  OutlierSite,
  OutliersResponse,
  TraceResponse,
  TracedCall,
  TracedThread,
} from "../api/types";
import { sk, useCurrentSession } from "../api/hooks";
import { DataTable } from "../components/DataTable";
import { MethodLabel } from "../components/MethodLabel";
import { fitCurve, formatOffset, niceMax } from "../lib/calls";
import { formatCount, formatNanos } from "../lib/format";
import { client, readonly, useApp } from "../state";
import { SearchBox } from "./profile/shared";

type Tab = "outliers" | "trace" | "complexity";

const TRACE_PAGE = 2000;

/** Runs one call-analysis query for the current session and time range. */
function useCalls<T>(req: Omit<CallsRequest, "range">) {
  const sid = useApp((s) => s.sessionId)!;
  const range = useApp((s) => s.range);
  const full: CallsRequest = { ...req, range: range ?? undefined };
  return useQuery({ queryKey: sk(sid, "calls", full), queryFn: () => client.calls<T>(sid, full) });
}

function errorText(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

function Empty({ loading, error, text }: { loading: boolean; error: unknown; text: React.ReactNode }) {
  return (
    <div className="empty">{loading ? <div className="spinner" /> : error ? errorText(error) : text}</div>
  );
}

// ---------- outliers ----------

function ratioBadge(r: number) {
  const text = `${r >= 100 ? r.toFixed(0) : r.toFixed(1)}×`;
  if (r >= 100) return <span className="badge critical">{text}</span>;
  if (r >= 10) return <span className="badge warning">{text}</span>;
  return <span className="num">{text}</span>;
}

function Outliers() {
  const [search, setSearch] = useState("");
  const [minCalls, setMinCalls] = useState(10);
  const q = useCalls<OutliersResponse>({
    view: "outliers",
    search: search || undefined,
    min_calls: minCalls,
    limit: 2000,
  });
  const rows = q.data?.sites ?? [];
  return (
    <>
      <div className="toolbar">
        <span className="faint">
          Call sites of instrumented methods whose slowest call is far above their mean.
        </span>
        <span className="grow" />
        <label className="check">
          Min calls
          <input
            className="input"
            type="number"
            min={1}
            style={{ width: 70 }}
            value={minCalls}
            onChange={(e) => setMinCalls(Math.max(1, Number(e.target.value) || 1))}
          />
        </label>
        <SearchBox value={search} onChange={setSearch} />
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <DataTable<OutlierSite>
          label="Call-site outliers"
          rows={rows}
          rowKey={(r) => r.stack}
          empty={
            <Empty
              loading={q.isLoading}
              error={q.error}
              text="No instrumented calls in this range. Count calls in your packages (Probes → Instrumentation, or the agent's instrument= option)."
            />
          }
          columns={[
            {
              id: "method",
              header: "Method",
              flex: true,
              render: (r) => <MethodLabel label={r.method} />,
            },
            {
              id: "caller",
              header: "Called from",
              width: 260,
              render: (r) => (r.caller ? <MethodLabel label={r.caller} /> : ""),
            },
            { id: "calls", header: "Calls", width: 80, align: "right", render: (r) => formatCount(r.calls) },
            { id: "mean", header: "Mean", width: 90, align: "right", render: (r) => formatNanos(r.mean) },
            { id: "max", header: "Max", width: 90, align: "right", render: (r) => formatNanos(r.max) },
            {
              id: "ratio",
              header: "Max / mean",
              width: 90,
              align: "right",
              render: (r) => ratioBadge(r.ratio),
            },
            { id: "threads", header: "Threads", width: 70, align: "right", render: (r) => r.threads },
          ]}
        />
      </div>
    </>
  );
}

// ---------- call tracer ----------

/** The page's calls as bars: time across, nesting depth down. */
function TraceTimeline({
  calls,
  selected,
  onSelect,
}: {
  calls: TracedCall[];
  selected: number | null;
  onSelect: (i: number) => void;
}) {
  const layout = useMemo(() => {
    if (calls.length === 0) return null;
    const t0 = calls[0].start;
    let t1 = t0 + 1;
    let depth = 0;
    for (const c of calls) {
      t1 = Math.max(t1, c.end ?? c.start);
      depth = Math.max(depth, c.depth);
    }
    return { t0, span: t1 - t0, depth: Math.min(depth, 24) + 1 };
  }, [calls]);
  if (!layout) return null;
  const row = 12;
  const w = 1000;
  return (
    <svg
      role="img"
      aria-label="Call trace timeline"
      viewBox={`0 0 ${w} ${layout.depth * row}`}
      preserveAspectRatio="none"
      style={{ width: "100%", height: layout.depth * row, display: "block", flexShrink: 0 }}
    >
      {calls.map((c, i) => {
        if (c.depth >= layout.depth) return null;
        const x = ((c.start - layout.t0) / layout.span) * w;
        const x2 = (((c.end ?? layout.t0 + layout.span) - layout.t0) / layout.span) * w;
        return (
          <rect
            key={i}
            x={x}
            y={c.depth * row + 1}
            width={Math.max(0.5, x2 - x)}
            height={row - 2}
            fill={i === selected ? "var(--accent)" : `var(--c${(c.frame % 8) + 1})`}
            opacity={i === selected ? 1 : 0.75}
            onClick={() => onSelect(i)}
          >
            <title>
              {c.method} {c.end != null ? formatNanos(c.end - c.start) : "(running)"}
            </title>
          </rect>
        );
      })}
    </svg>
  );
}

function Trace() {
  const [thread, setThread] = useState<number | null>(null);
  const [offset, setOffset] = useState(0);
  const [sel, setSel] = useState<number | null>(null);
  const q = useCalls<TraceResponse>({
    view: "trace",
    thread: thread ?? undefined,
    offset,
    limit: TRACE_PAGE,
  });
  const data = q.data;
  const current = thread ?? data?.thread ?? null;
  const calls = data?.calls ?? [];
  const origin = data?.threads.find((t) => t.thread === current)?.start ?? calls[0]?.start ?? 0;
  const total = data?.total ?? 0;
  return (
    <div
      className="code-layout"
      style={{ gridTemplateColumns: "minmax(200px, 260px) minmax(0, 1fr)", flex: 1 }}
    >
      <DataTable<TracedThread>
        label="Traced threads"
        rows={data?.threads ?? []}
        rowKey={(t) => t.thread}
        selected={current}
        onSelect={(t) => {
          setThread(t.thread);
          setOffset(0);
          setSel(null);
        }}
        empty={<Empty loading={q.isLoading} error={q.error} text="No call trace recorded" />}
        columns={[
          { id: "name", header: "Thread", flex: true, render: (t) => t.name },
          { id: "events", header: "Events", width: 70, align: "right", render: (t) => formatCount(t.events) },
        ]}
      />
      <div style={{ display: "flex", flexDirection: "column", minHeight: 0, minWidth: 0 }}>
        <div className="toolbar">
          <span className="faint">
            {total > 0
              ? `Calls ${formatCount(offset + 1)}–${formatCount(offset + calls.length)} of ${formatCount(total)}`
              : "Calls of instrumented methods in the order they ran"}
          </span>
          <span className="grow" />
          <button
            className="btn ghost"
            disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - TRACE_PAGE))}
          >
            Previous
          </button>
          <button
            className="btn ghost"
            disabled={offset + TRACE_PAGE >= total}
            onClick={() => setOffset(offset + TRACE_PAGE)}
          >
            Next
          </button>
        </div>
        <TraceTimeline calls={calls} selected={sel} onSelect={setSel} />
        <div style={{ flex: 1, minHeight: 0 }}>
          <DataTable<TracedCall>
            label="Traced calls"
            rows={calls}
            rowKey={(_c, i) => i}
            selected={sel}
            onSelect={(_c, i) => setSel(i)}
            empty={<Empty loading={q.isLoading} error={q.error} text="Select a thread" />}
            columns={[
              {
                id: "start",
                header: "Start",
                width: 110,
                align: "right",
                render: (c) => <span className="num">{formatOffset(c.start, origin)}</span>,
              },
              {
                id: "dur",
                header: "Duration",
                width: 90,
                align: "right",
                render: (c) =>
                  c.end != null ? formatNanos(c.end - c.start) : <span className="faint">running</span>,
              },
              {
                id: "method",
                header: "Method",
                flex: true,
                render: (c) => (
                  <span style={{ paddingLeft: Math.min(c.depth, 40) * 14 }}>
                    <MethodLabel label={c.method} />
                  </span>
                ),
              },
            ]}
          />
        </div>
      </div>
    </div>
  );
}

// ---------- complexity ----------

function ComplexityChart({
  points,
  fit,
  maxSize,
}: {
  points: [number, number][];
  fit: ComplexityFit | null;
  maxSize: number;
}) {
  const w = 640;
  const h = 300;
  const pad = { l: 64, r: 12, t: 10, b: 30 };
  const xMax = niceMax(maxSize);
  const yMax = niceMax(Math.max(...points.map((p) => p[1]), 1));
  const x = (n: number) => pad.l + (n / xMax) * (w - pad.l - pad.r);
  const y = (t: number) => h - pad.b - (Math.min(t, yMax) / yMax) * (h - pad.t - pad.b);
  const curve = fit ? fitCurve(fit, xMax) : [];
  const ticks = [0, 0.25, 0.5, 0.75, 1];
  return (
    <svg
      role="img"
      aria-label="Time against size"
      viewBox={`0 0 ${w} ${h}`}
      style={{ width: "100%", maxWidth: 900 }}
    >
      {ticks.map((f) => (
        <g key={f}>
          <line x1={pad.l} x2={w - pad.r} y1={y(f * yMax)} y2={y(f * yMax)} stroke="var(--chart-grid)" />
          <text x={pad.l - 6} y={y(f * yMax) + 4} textAnchor="end" fontSize={10} fill="var(--chart-axis)">
            {formatNanos(f * yMax)}
          </text>
          <text x={x(f * xMax)} y={h - pad.b + 16} textAnchor="middle" fontSize={10} fill="var(--chart-axis)">
            {formatCount(f * xMax)}
          </text>
        </g>
      ))}
      <text x={w - pad.r} y={h - 2} textAnchor="end" fontSize={10} fill="var(--fg-muted)">
        size
      </text>
      {points.map((p, i) => (
        <circle key={i} cx={x(p[0])} cy={y(p[1])} r={2} fill="var(--accent)" opacity={0.45} />
      ))}
      {curve.length > 0 && (
        <polyline
          points={curve.map(([n, t]) => `${x(n)},${y(t)}`).join(" ")}
          fill="none"
          stroke="var(--warning)"
          strokeWidth={2}
        />
      )}
    </svg>
  );
}

function ComplexityEditor({ sid }: { sid: string }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: sk(sid, "instrumentation"), queryFn: () => client.instrumentation(sid) });
  const [text, setText] = useState<string | null>(null);
  useEffect(() => {
    if (q.data && text === null) setText(q.data.complexity.join("\n"));
  }, [q.data, text]);
  const save = useMutation({
    mutationFn: () =>
      client.setInstrumentation(sid, {
        complexity: (text ?? "")
          .split(/\n/)
          .map((s) => s.trim())
          .filter(Boolean),
      }),
    onSuccess: (r) => qc.setQueryData(sk(sid, "instrumentation"), r),
  });
  return (
    <div className="card" style={{ margin: "8px 16px", display: "flex", flexDirection: "column", gap: 6 }}>
      <span className="label">
        Methods to analyse, one per line: <span className="mono">com.acme.Sorter.sort</span>, optionally{" "}
        <span className="mono">#1</span> (argument index) or <span className="mono">#this</span>. The size is
        the argument's length, size or value; by default the first argument that has one.
      </span>
      <textarea
        className="input mono"
        rows={3}
        style={{ height: "auto", padding: 6 }}
        value={text ?? ""}
        onChange={(e) => setText(e.target.value)}
      />
      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        <button
          className="btn primary"
          disabled={save.isPending || text === null}
          onClick={() => save.mutate()}
        >
          Apply
        </button>
        {save.isError && <span className="error">{errorText(save.error)}</span>}
      </div>
    </div>
  );
}

function Complexity({ live }: { live: boolean }) {
  const sid = useApp((s) => s.sessionId)!;
  const [frame, setFrame] = useState<number | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const q = useCalls<ComplexityResponse>({ view: "complexity", frame: frame ?? undefined });
  const methods = q.data?.methods ?? [];
  const m = q.data?.method;
  const fit = m?.fits.find((f) => f.model === model) ?? m?.best ?? null;
  return (
    <>
      {live && !readonly && <ComplexityEditor sid={sid} />}
      <div
        className="code-layout"
        style={{ gridTemplateColumns: "minmax(260px, 360px) minmax(0, 1fr)", flex: 1 }}
      >
        <DataTable<ComplexityMethod>
          label="Methods"
          rows={methods}
          rowKey={(r) => r.frame}
          selected={m?.summary.frame ?? null}
          onSelect={(r) => {
            setFrame(r.frame);
            setModel(null);
          }}
          empty={<Empty loading={q.isLoading} error={q.error} text="No complexity data recorded" />}
          columns={[
            { id: "method", header: "Method", flex: true, render: (r) => <MethodLabel label={r.method} /> },
            {
              id: "points",
              header: "Calls",
              width: 64,
              align: "right",
              render: (r) => formatCount(r.points),
            },
            {
              id: "best",
              header: "Fit",
              width: 90,
              render: (r) => (r.best ? <span className="badge info">{r.best}</span> : ""),
            },
          ]}
        />
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            minHeight: 0,
            minWidth: 0,
            overflow: "auto",
            padding: 12,
          }}
        >
          {m ? (
            <>
              <div className="toolbar" style={{ padding: 0 }}>
                <MethodLabel label={m.summary.method} />
                <span className="faint">
                  {formatCount(m.summary.points)} calls, {formatCount(m.summary.sizes)} sizes up to{" "}
                  {formatCount(m.summary.max_size)}
                </span>
              </div>
              <ComplexityChart points={m.points} fit={fit} maxSize={m.summary.max_size} />
              <div className="toolbar" style={{ padding: 0, flexWrap: "wrap" }} aria-label="Fitted models">
                <span className="label">Fit (R²)</span>
                {m.fits.map((f) => (
                  <button
                    key={f.model}
                    className="btn ghost"
                    aria-pressed={fit?.model === f.model}
                    title={m.best?.model === f.model ? "Best fit" : undefined}
                    onClick={() => setModel(f.model)}
                  >
                    <span className="mono">{f.model}</span>&nbsp;
                    <span className="num faint">{f.r2.toFixed(3)}</span>
                    {m.best?.model === f.model && <>&nbsp;★</>}
                  </button>
                ))}
              </div>
            </>
          ) : (
            <div className="empty">Select a method</div>
          )}
        </div>
      </div>
    </>
  );
}

// ---------- live controls ----------

const DURATIONS = [1, 5, 10, 30, 60];

function TraceControls({ sid }: { sid: string }) {
  const qc = useQueryClient();
  const [seconds, setSeconds] = useState(5);
  const info = useQuery({
    queryKey: sk(sid, "instrumentation"),
    queryFn: () => client.instrumentation(sid),
    refetchInterval: (q) => (q.state.data?.trace_active ? 1000 : false),
  });
  const run = useMutation({
    mutationFn: (enabled: boolean) => client.callTrace(sid, enabled, seconds * 1000),
    onSuccess: () => qc.invalidateQueries({ queryKey: sk(sid, "instrumentation") }),
  });
  const active = info.data?.trace_active ?? false;
  return (
    <>
      {info.data && (
        <span className="faint">
          {active ? "Tracing… " : info.data.trace_events ? "Last trace: " : ""}
          {info.data.trace_events ? `${formatCount(info.data.trace_events)} events` : ""}
        </span>
      )}
      <select
        className="select"
        aria-label="Trace duration"
        value={seconds}
        onChange={(e) => setSeconds(Number(e.target.value))}
      >
        {DURATIONS.map((d) => (
          <option key={d} value={d}>
            {d} s
          </option>
        ))}
      </select>
      <button className="btn" disabled={run.isPending} onClick={() => run.mutate(!active)}>
        {active ? "Stop trace" : "Start trace"}
      </button>
      {run.isError && <span className="error">{errorText(run.error)}</span>}
    </>
  );
}

// ---------- view ----------

export function MethodsView() {
  const s = useCurrentSession()!;
  const live = s.capabilities.includes("instrument");
  const [tab, setTab] = useState<Tab>("outliers");
  const tabs: { id: Tab; label: string }[] = [
    { id: "outliers", label: "Outliers" },
    { id: "trace", label: "Call tracer" },
    { id: "complexity", label: "Complexity" },
  ];
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Method analysis</h1>
        <span className="grow" />
        {live && !readonly && <TraceControls sid={s.id} />}
      </div>
      <div className="tabs" role="tablist">
        {tabs.map((t) => (
          <button
            key={t.id}
            role="tab"
            className="tab"
            aria-selected={tab === t.id}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div className="view-body" style={{ display: "flex", flexDirection: "column" }}>
        {tab === "outliers" && <Outliers />}
        {tab === "trace" && <Trace />}
        {tab === "complexity" && <Complexity live={live} />}
      </div>
    </div>
  );
}
