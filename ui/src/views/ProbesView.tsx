import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import type {
  ControlObjectDto,
  ControlState,
  InstrumentationInfo,
  ProbeEventDto,
  ProbeHotspot,
  ProbeLane,
  ProbeSeries,
  ProbeSummary,
  ProbesRequest,
} from "../api/types";
import { sk, useCurrentSession, useSummary } from "../api/hooks";
import { DataTable, PctBar } from "../components/DataTable";
import { MethodLabel } from "../components/MethodLabel";
import { TimeChart } from "../components/TimeChart";
import { formatClock, formatCount, formatNanos, formatRelTime } from "../lib/format";
import { client, readonly, store, useApp } from "../state";
import { CallTreePanel } from "./profile/panels";
import { SearchBox, useTreeParams } from "./profile/shared";

type Tab = "hotspots" | "events" | "timeline" | "controls" | "telemetry";

/** Runs one probes query for the current session and time range. */
function useProbes<T>(req: Omit<ProbesRequest, "range">, enabled = true) {
  const sid = useApp((s) => s.sessionId)!;
  const range = useApp((s) => s.range);
  const full: ProbesRequest = { ...req, range: range ?? undefined };
  return useQuery({
    queryKey: sk(sid, "probes", full),
    queryFn: () => client.probes<T>(sid, full),
    enabled,
  });
}

function errorText(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

const STATE_BADGE: Record<ControlState, string> = {
  open: "badge info",
  long_open: "badge warning",
  closed: "badge",
  leaked: "badge critical",
};

const STATE_LABEL: Record<ControlState, string> = {
  open: "open",
  long_open: "open (long)",
  closed: "closed",
  leaked: "leaked",
};

// ---------- probe list ----------

function ProbeList({
  probes,
  selected,
  onSelect,
}: {
  probes: ProbeSummary[];
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  const [all, setAll] = useState(false);
  const rows = all ? probes : probes.filter((p) => p.calls > 0 || p.open_controls > 0);
  return (
    <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
      <div className="toolbar" style={{ padding: "6px 12px" }}>
        <span className="label">Probes</span>
        <span className="grow" />
        <label className="check">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
          Unused
        </label>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <DataTable<ProbeSummary>
          label="Probes"
          rows={rows}
          rowKey={(p) => p.id}
          selected={selected}
          onSelect={(p) => onSelect(p.id)}
          rowClass={(p) => (p.calls === 0 ? "muted" : undefined)}
          empty={<div className="empty">No probe calls recorded yet</div>}
          columns={[
            { id: "name", header: "Probe", flex: true, render: (p) => p.name },
            { id: "calls", header: "Calls", width: 60, align: "right", render: (p) => formatCount(p.calls) },
            {
              id: "total",
              header: "Time",
              width: 80,
              align: "right",
              render: (p) => formatNanos(p.total),
            },
            {
              id: "errors",
              header: "Errors",
              width: 60,
              align: "right",
              render: (p) =>
                p.errors + p.leaked_controls > 0 ? (
                  <span className="badge critical">{formatCount(p.errors + p.leaked_controls)}</span>
                ) : (
                  ""
                ),
            },
          ]}
        />
      </div>
    </div>
  );
}

// ---------- hot spots ----------

function PayloadBacktraces({ payload }: { payload: string }) {
  const sid = useApp((s) => s.sessionId)!;
  const params = useTreeParams("probe", false, [payload]);
  return <CallTreePanel sessionId={sid} params={params} view="backtraces" />;
}

function Hotspots({ probe }: { probe: ProbeSummary }) {
  const [search, setSearch] = useState("");
  const q = useProbes<{ hotspots: ProbeHotspot[]; total: number }>({
    probe: probe.id,
    view: "hotspots",
    search: search || undefined,
    limit: 2000,
  });
  const [sel, setSel] = useState<string | null>(null);
  const rows = q.data?.hotspots ?? [];
  const total = q.data?.total ?? 0;
  const current = sel ?? rows[0]?.payload ?? null;
  return (
    <>
      <div className="toolbar">
        <SearchBox value={search} onChange={setSearch} />
      </div>
      <div className="split-v" style={{ flex: 1, minHeight: 0 }}>
        <DataTable<ProbeHotspot>
          label={`${probe.payload_label} hot spots`}
          rows={rows}
          rowKey={(r) => r.payload}
          selected={current}
          onSelect={(r) => setSel(r.payload)}
          empty={
            <div className="empty">
              {q.isLoading ? (
                <div className="spinner" />
              ) : q.isError ? (
                errorText(q.error)
              ) : (
                "No calls in this range"
              )}
            </div>
          }
          columns={[
            {
              id: "payload",
              header: probe.payload_label,
              flex: true,
              title: (r) => r.payload,
              render: (r) => <span className="mono">{r.payload}</span>,
            },
            {
              id: "total",
              header: "Total time",
              width: 180,
              render: (r) => (
                <span className="bar-cell" style={{ justifyContent: "flex-end" }}>
                  <span className="num" style={{ width: 70 }}>
                    {formatNanos(r.total)}
                  </span>
                  <PctBar value={r.total} total={total} />
                </span>
              ),
            },
            { id: "calls", header: "Calls", width: 80, align: "right", render: (r) => formatCount(r.calls) },
            { id: "mean", header: "Mean", width: 90, align: "right", render: (r) => formatNanos(r.mean) },
            { id: "max", header: "Max", width: 90, align: "right", render: (r) => formatNanos(r.max) },
            {
              id: "errors",
              header: "Errors",
              width: 70,
              align: "right",
              render: (r) =>
                r.errors ? <span className="badge critical">{formatCount(r.errors)}</span> : "",
            },
            { id: "threads", header: "Threads", width: 70, align: "right", render: (r) => r.threads },
          ]}
        />
        <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
          <div className="toolbar" style={{ padding: "6px 16px" }}>
            <span className="label">Back traces</span>
            {current && (
              <span className="mono faint" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                {current}
              </span>
            )}
          </div>
          <div style={{ flex: 1, minHeight: 0 }}>
            {current ? (
              <PayloadBacktraces key={current} payload={current} />
            ) : (
              <div className="empty">Select a payload to see where it is called from.</div>
            )}
          </div>
        </div>
      </div>
    </>
  );
}

// ---------- events ----------

function DurationHistogram({ events }: { events: ProbeEventDto[] }) {
  const buckets = useMemo(() => {
    if (events.length === 0) return [];
    // Log-scale buckets from 1 µs, doubling.
    const n = 24;
    const counts = new Array<number>(n).fill(0);
    for (const e of events) {
      const i = Math.min(n - 1, Math.max(0, Math.floor(Math.log2(Math.max(1, e.duration / 1000)))));
      counts[i]++;
    }
    let last = n - 1;
    while (last > 0 && counts[last] === 0) last--;
    let first = 0;
    while (first < last && counts[first] === 0) first++;
    return counts.slice(first, last + 1).map((c, i) => ({ c, from: 1000 * 2 ** (first + i) }));
  }, [events]);
  if (buckets.length === 0) return null;
  const max = Math.max(...buckets.map((b) => b.c));
  return (
    <div
      aria-label="Duration histogram"
      style={{ display: "flex", alignItems: "flex-end", gap: 2, height: 56, padding: "6px 16px" }}
    >
      {buckets.map((b) => (
        <div
          key={b.from}
          title={`≥ ${formatNanos(b.from)}: ${formatCount(b.c)} events`}
          style={{
            flex: 1,
            minWidth: 4,
            height: `${Math.max(2, (b.c / max) * 100)}%`,
            background: "var(--accent)",
            opacity: b.c ? 0.8 : 0.15,
            borderRadius: 2,
          }}
        />
      ))}
    </div>
  );
}

function Events({ probe }: { probe: ProbeSummary }) {
  const summary = useSummary(useApp((s) => s.sessionId));
  const [search, setSearch] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const q = useProbes<{ events: ProbeEventDto[] }>({
    probe: probe.id,
    view: "events",
    search: search || undefined,
    errors_only: errorsOnly,
    limit: 5000,
  });
  const [sort, setSort] = useState("time");
  const rows = useMemo(() => {
    const r = [...(q.data?.events ?? [])];
    if (sort === "duration") r.sort((a, b) => b.duration - a.duration);
    return r;
  }, [q.data, sort]);
  const origin = summary.data?.start ?? rows[0]?.start ?? 0;
  const focus = (e: ProbeEventDto) => store.set({ range: [e.start - 1e9, e.start + e.duration + 1e9] });
  return (
    <>
      <div className="toolbar">
        <label className="check">
          <input type="checkbox" checked={errorsOnly} onChange={(e) => setErrorsOnly(e.target.checked)} />
          Errors only
        </label>
        <span className="faint">Double-click an event to focus all views on it.</span>
        <span className="grow" />
        <SearchBox value={search} onChange={setSearch} />
      </div>
      <DurationHistogram events={rows} />
      <div style={{ flex: 1, minHeight: 0 }}>
        <DataTable<ProbeEventDto>
          label={`${probe.name} events`}
          rows={rows}
          rowKey={(_e, i) => i}
          sort={sort}
          onSort={setSort}
          onActivate={focus}
          empty={
            <div className="empty">
              {q.isLoading ? (
                <div className="spinner" />
              ) : q.isError ? (
                errorText(q.error)
              ) : (
                "No events in this range"
              )}
            </div>
          }
          columns={[
            {
              id: "time",
              header: "Time",
              width: 90,
              sortKey: "time",
              render: (e) => formatRelTime(e.start, origin),
            },
            {
              id: "dur",
              header: "Duration",
              width: 90,
              align: "right",
              sortKey: "duration",
              render: (e) => formatNanos(e.duration),
            },
            {
              id: "payload",
              header: probe.payload_label,
              flex: true,
              title: (e) => e.payload,
              render: (e) => <span className="mono">{e.payload}</span>,
            },
            { id: "thread", header: "Thread", width: 140, render: (e) => e.thread },
            {
              id: "detail",
              header: "Detail",
              width: 160,
              title: (e) => e.detail ?? "",
              render: (e) => <span className="muted">{e.detail ?? ""}</span>,
            },
            {
              id: "context",
              header: "Request",
              width: 150,
              title: (e) => e.context ?? "",
              render: (e) => (e.context ? <span className="badge accent">{e.context}</span> : ""),
            },
            {
              id: "error",
              header: "Error",
              width: 160,
              title: (e) => e.error ?? "",
              render: (e) => (e.error ? <span className="badge critical">{e.error}</span> : ""),
            },
            {
              id: "frame",
              header: "Called from",
              width: 220,
              render: (e) => (e.top_frame ? <MethodLabel label={e.top_frame} /> : ""),
            },
          ]}
        />
      </div>
    </>
  );
}

// ---------- timeline & telemetry ----------

function useDomain(): [number, number] | undefined {
  const summary = useSummary(useApp((s) => s.sessionId));
  return summary.data ? [summary.data.start, summary.data.end] : undefined;
}

function Timeline({ probe }: { probe: string | null }) {
  const range = useApp((s) => s.range);
  const domain = useDomain();
  const [all, setAll] = useState(true);
  const q = useProbes<{ lanes: ProbeLane[] }>({
    view: "timeline",
    probe: all ? undefined : (probe ?? undefined),
  });
  const lanes = q.data?.lanes ?? [];
  return (
    <>
      <div className="toolbar">
        <label className="check">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
          All probes
        </label>
        <span className="faint">Calls per second; drag to select a time range.</span>
      </div>
      <div className="view-body pad" style={{ overflow: "auto" }}>
        {q.isLoading && <div className="spinner" />}
        {q.isError && <div className="error">{errorText(q.error)}</div>}
        {!q.isLoading && lanes.length === 0 && <div className="empty">No probe calls in this range</div>}
        {lanes.map((l, i) => (
          <div key={l.probe} style={{ marginBottom: 12 }}>
            <div className="label" style={{ marginBottom: 4 }}>
              {l.name}{" "}
              <span className="faint">
                {formatCount(l.calls.reduce((a, b) => a + b, 0))} calls,{" "}
                {formatNanos(l.duration.reduce((a, b) => a + b, 0))}
              </span>
            </div>
            <TimeChart
              series={[
                { label: "Calls", t: l.time, v: l.calls, color: `var(--c${(i % 8) + 1})`, fill: true },
                ...(l.errors.some((e) => e > 0)
                  ? [{ label: "Errors", t: l.time, v: l.errors, color: "var(--danger)" }]
                  : []),
              ]}
              unit="per_second"
              height={80}
              domain={domain}
              range={range}
              onSelectRange={(r) => store.set({ range: r })}
            />
          </div>
        ))}
      </div>
    </>
  );
}

function Telemetry({ probe }: { probe: ProbeSummary }) {
  const range = useApp((s) => s.range);
  const domain = useDomain();
  const q = useProbes<{ series: ProbeSeries[] }>({ probe: probe.id, view: "telemetry" });
  const series = q.data?.series ?? [];
  return (
    <div className="view-body pad" style={{ overflow: "auto" }}>
      {q.isLoading && <div className="spinner" />}
      {q.isError && <div className="error">{errorText(q.error)}</div>}
      <div className="cards" style={{ gridTemplateColumns: "1fr" }}>
        {series.map((s, i) => (
          <div className="card" key={s.key}>
            <h3>{s.label}</h3>
            {s.time.length ? (
              <TimeChart
                series={[{ label: s.label, t: s.time, v: s.value, color: `var(--c${i + 1})`, fill: true }]}
                unit={s.unit}
                height={140}
                domain={domain}
                range={range}
                onSelectRange={(r) => store.set({ range: r })}
              />
            ) : (
              <div className="faint">No calls in this range</div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

// ---------- control objects ----------

function Controls({ probe }: { probe: ProbeSummary }) {
  const [threshold, setThreshold] = useState(60);
  const [openOnly, setOpenOnly] = useState(false);
  const q = useProbes<{ controls: ControlObjectDto[] }>({
    probe: probe.id,
    view: "controls",
    open_threshold_ms: threshold * 1000,
    limit: 5000,
  });
  const rows = (q.data?.controls ?? []).filter((c) => !openOnly || c.state !== "closed");
  const counts = (q.data?.controls ?? []).reduce(
    (m, c) => ({ ...m, [c.state]: (m[c.state] ?? 0) + 1 }),
    {} as Partial<Record<ControlState, number>>,
  );
  return (
    <>
      <div className="toolbar">
        {(["open", "long_open", "leaked", "closed"] as ControlState[]).map((s) => (
          <span key={s} className={STATE_BADGE[s]}>
            {STATE_LABEL[s]}: {formatCount(counts[s] ?? 0)}
          </span>
        ))}
        <span className="grow" />
        <label className="check">
          <input type="checkbox" checked={openOnly} onChange={(e) => setOpenOnly(e.target.checked)} />
          Hide closed
        </label>
        <label className="check">
          Long open after
          <input
            className="input"
            type="number"
            min={1}
            style={{ width: 64 }}
            value={threshold}
            aria-label="Long-open threshold in seconds"
            onChange={(e) => setThreshold(Math.max(1, Number(e.target.value) || 1))}
          />
          s
        </label>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <DataTable<ControlObjectDto>
          label={`${probe.control_label ?? "Control"} objects`}
          rows={rows}
          rowKey={(c) => c.id}
          empty={
            <div className="empty">
              {q.isLoading ? (
                <div className="spinner" />
              ) : q.isError ? (
                errorText(q.error)
              ) : (
                `No ${(probe.control_label ?? "control").toLowerCase()} objects`
              )}
            </div>
          }
          columns={[
            {
              id: "state",
              header: "State",
              width: 100,
              render: (c) => <span className={STATE_BADGE[c.state]}>{STATE_LABEL[c.state]}</span>,
            },
            {
              id: "desc",
              header: probe.control_label ?? "Object",
              flex: true,
              title: (c) => c.description,
              render: (c) => <span className="mono">{c.description}</span>,
            },
            {
              id: "opened",
              header: "Opened",
              width: 100,
              render: (c) => formatClock(c.opened),
            },
            {
              id: "for",
              header: "Open for",
              width: 100,
              align: "right",
              render: (c) => formatNanos(c.open_for),
            },
            { id: "thread", header: "Thread", width: 140, render: (c) => c.thread },
            {
              id: "frame",
              header: "Opened from",
              width: 240,
              render: (c) => (c.top_frame ? <MethodLabel label={c.top_frame} /> : ""),
            },
          ]}
        />
      </div>
    </>
  );
}

// ---------- instrumentation (live) ----------

function InstrumentationPanel({ sid, onClose }: { sid: string; onClose: () => void }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: sk(sid, "instrumentation"), queryFn: () => client.instrumentation(sid) });
  const [include, setInclude] = useState("");
  const [exclude, setExclude] = useState("");
  const [enabled, setEnabled] = useState<string[] | null>(null);
  useEffect(() => {
    if (q.data && enabled === null) {
      setInclude(q.data.include.join("\n"));
      setExclude(q.data.exclude.join("\n"));
      setEnabled(q.data.enabled_probes);
    }
  }, [q.data, enabled]);
  const save = useMutation({
    mutationFn: (info: InstrumentationInfo) => {
      const lines = (s: string) =>
        s
          .split(/[\n,]/)
          .map((x) => x.trim())
          .filter(Boolean);
      const on = enabled ?? [];
      const allOn = info.available_probes.every((p) => on.includes(p.id));
      return client.setInstrumentation(sid, {
        include: lines(include),
        exclude: lines(exclude),
        probes: allOn ? [] : on.length ? on : ["none"],
      });
    },
    onSuccess: (r) => {
      qc.setQueryData(sk(sid, "instrumentation"), r);
      setEnabled(r.enabled_probes);
    },
  });
  const info = q.data;
  const toggle = (id: string) =>
    setEnabled((e) => ((e ?? []).includes(id) ? (e ?? []).filter((x) => x !== id) : [...(e ?? []), id]));
  return (
    <div className="card" style={{ margin: "8px 16px" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 8 }}>
        <h3 style={{ margin: 0 }}>Instrumentation</h3>
        {info && (
          <span className="faint">
            {formatCount(info.instrumented_classes)} classes, {formatCount(info.instrumented_methods)} methods
            instrumented
          </span>
        )}
        <span className="grow" />
        <button className="btn ghost" onClick={onClose}>
          Close
        </button>
      </div>
      {q.isLoading && <div className="spinner" />}
      {q.isError && <div className="error">{errorText(q.error)}</div>}
      {info && (
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 12 }}>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="label">Count calls in (one pattern per line, e.g. com.acme.*)</span>
            <textarea
              className="input mono"
              rows={5}
              style={{ height: "auto", padding: 6 }}
              value={include}
              onChange={(e) => setInclude(e.target.value)}
            />
          </label>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="label">Exclude</span>
            <textarea
              className="input mono"
              rows={5}
              style={{ height: "auto", padding: 6 }}
              value={exclude}
              onChange={(e) => setExclude(e.target.value)}
            />
          </label>
          <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="label">Probes</span>
            {info.available_probes.map((p) => (
              <label key={p.id} className="check">
                <input
                  type="checkbox"
                  checked={(enabled ?? []).includes(p.id)}
                  onChange={() => toggle(p.id)}
                />
                {p.name}
              </label>
            ))}
          </div>
          {info.disabled_methods.length > 0 && (
            <div style={{ gridColumn: "1 / -1" }}>
              <span className="label">Removed by auto-tuning (hot and trivial)</span>
              <div className="mono faint" style={{ fontSize: 11 }}>
                {info.disabled_methods.join(", ")}
              </div>
            </div>
          )}
          <div style={{ gridColumn: "1 / -1", display: "flex", gap: 8, alignItems: "center" }}>
            <button className="btn primary" disabled={save.isPending} onClick={() => save.mutate(info)}>
              Apply
            </button>
            <span className="faint">
              Affected classes are retransformed; call counts appear under CPU → Calls.
            </span>
            {save.isError && <span className="error">{errorText(save.error)}</span>}
          </div>
        </div>
      )}
    </div>
  );
}

// ---------- view ----------

export function ProbesView() {
  const s = useCurrentSession()!;
  const live = s.capabilities.includes("instrument");
  const list = useProbes<{ probes: ProbeSummary[] }>({ view: "list" });
  const [selected, setSelected] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("hotspots");
  const [panel, setPanel] = useState(false);
  const probes = list.data?.probes ?? [];
  const busiest = [...probes].sort((a, b) => b.total - a.total)[0];
  const current = probes.find((p) => p.id === selected) ?? (busiest?.calls ? busiest : undefined);
  const tabs: { id: Tab; label: string }[] = [
    { id: "hotspots", label: "Hot spots" },
    { id: "events", label: `Events${current ? ` (${formatCount(current.events)})` : ""}` },
    { id: "timeline", label: "Timeline" },
    ...(current?.control_label
      ? [{ id: "controls" as Tab, label: `${current.control_label} objects (${current.open_controls} open)` }]
      : []),
    { id: "telemetry", label: "Telemetry" },
  ];
  const active = tabs.some((t) => t.id === tab) ? tab : "hotspots";
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Probes</h1>
        {current && (
          <span className="faint">
            {current.name}: {formatCount(current.calls)} calls, {formatNanos(current.total)}
            {current.errors > 0 && `, ${formatCount(current.errors)} errors`}
          </span>
        )}
        <span className="grow" />
        {live && !readonly && (
          <button className="btn ghost" aria-pressed={panel} onClick={() => setPanel((p) => !p)}>
            Instrumentation…
          </button>
        )}
      </div>
      {live && panel && <InstrumentationPanel sid={s.id} onClose={() => setPanel(false)} />}
      {list.isError ? (
        <div className="error">{errorText(list.error)}</div>
      ) : (
        <div className="code-layout" style={{ gridTemplateColumns: "minmax(280px, 360px) minmax(0, 1fr)" }}>
          <ProbeList probes={probes} selected={current?.id ?? null} onSelect={setSelected} />
          {current ? (
            <div style={{ display: "flex", flexDirection: "column", minHeight: 0, minWidth: 0 }}>
              <div className="tabs" role="tablist">
                {tabs.map((t) => (
                  <button
                    key={t.id}
                    role="tab"
                    className="tab"
                    aria-selected={active === t.id}
                    onClick={() => setTab(t.id)}
                  >
                    {t.label}
                  </button>
                ))}
              </div>
              <div className="view-body" style={{ display: "flex", flexDirection: "column" }}>
                {active === "hotspots" && <Hotspots key={current.id} probe={current} />}
                {active === "events" && <Events key={current.id} probe={current} />}
                {active === "timeline" && <Timeline probe={current.id} />}
                {active === "controls" && <Controls key={current.id} probe={current} />}
                {active === "telemetry" && <Telemetry key={current.id} probe={current} />}
              </div>
            </div>
          ) : (
            <div className="empty">
              {list.isLoading ? (
                <div className="spinner" />
              ) : (
                <p style={{ maxWidth: 480 }}>
                  No probe calls yet. Probes record JDBC, JPA, HTTP, Kafka, gRPC, exceptions, files, sockets
                  and custom <span className="mono">@Probe</span> methods while the application runs.
                </p>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
