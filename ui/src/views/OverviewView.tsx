import { useQuery } from "@tanstack/react-query";
import type { Insight, JvmInfo, Series } from "../api/types";
import { sk, useCurrentSession, useSummary, useTelemetry } from "../api/hooks";
import { TimeChart, type ChartSeries } from "../components/TimeChart";
import { formatBytes, formatCount, formatNanos, formatValue } from "../lib/format";
import { client, openInsight, store, useApp } from "../state";

const GROUPS: { title: string; keys: string[]; colors: string[] }[] = [
  { title: "Heap", keys: ["heap.used", "heap.committed"], colors: ["var(--c2)", "var(--c1)"] },
  { title: "CPU load", keys: ["cpu.process", "cpu.system"], colors: ["var(--c3)", "var(--c8)"] },
  { title: "GC activity", keys: ["gc.pause", "gc.activity"], colors: ["var(--danger)", "var(--c4)"] },
  { title: "Threads", keys: ["threads.live", "threads.daemon"], colors: ["var(--c1)", "var(--c7)"] },
  { title: "Allocation rate", keys: ["alloc.rate"], colors: ["var(--c5)"] },
  { title: "Loaded classes", keys: ["classes.loaded"], colors: ["var(--c4)"] },
  { title: "Non-heap", keys: ["nonheap.used"], colors: ["var(--c7)"] },
  { title: "CPU samples", keys: ["samples.cpu"], colors: ["var(--c3)"] },
];

export function InsightList({ insights, onPick }: { insights: Insight[]; onPick?: (i: Insight) => void }) {
  if (!insights.length) return <div className="faint">No findings. 🎉</div>;
  return (
    <div className="insights">
      {insights.map((i, k) => (
        <button key={k} className="insight" onClick={() => (onPick ? onPick(i) : openInsight(i))}>
          <span className={`sev ${i.severity}`} />
          <span>
            <div className="title">{i.title}</div>
            <div className="summary">{i.summary}</div>
          </span>
          <span className="waste">
            {i.wasted_bytes != null
              ? formatBytes(i.wasted_bytes)
              : i.count != null
                ? formatCount(i.count)
                : ""}
          </span>
        </button>
      ))}
    </div>
  );
}

function Stat({ title, value, sub }: { title: string; value: React.ReactNode; sub?: React.ReactNode }) {
  return (
    <div className="card">
      <h3>{title}</h3>
      <div className="stat-value">{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

export function OverviewView() {
  const s = useCurrentSession()!;
  const range = useApp((st) => st.range);
  const summary = useSummary(s.id);
  const hasTelemetry = s.capabilities.includes("telemetry") || s.capabilities.includes("gc");
  const tel = useTelemetry(s.id, { points: 600 }, hasTelemetry);
  const insights = useQuery({
    queryKey: sk(s.id, "insights"),
    queryFn: () => client.insights(s.id),
    retry: false,
  });
  const sm = summary.data;
  if (summary.isLoading)
    return (
      <div className="empty">
        <div className="spinner" />
      </div>
    );
  if (summary.isError) return <div className="error">{String(summary.error.message)}</div>;
  if (!sm) return null;

  const jvm: Partial<JvmInfo> = sm.jvm ?? {};
  const series = tel.data?.series ?? [];
  const byKey = new Map<string, Series>(series.map((x) => [x.key, x]));
  const domain: [number, number] = [sm.start, Math.max(sm.end, sm.start + 1)];
  const gc = tel.data?.gc ?? [];
  const gcPause = gc.reduce((a, g) => a + (g.pause ?? g.duration), 0);
  const samples = Object.entries(sm.sample_counts).filter(([, v]) => v > 0);
  const used = byKey.get("heap.used");
  const latest = (x?: Series) => (x && x.v.length ? x.v[x.v.length - 1] : undefined);
  const known = new Set(GROUPS.flatMap((g) => g.keys));
  const extra = series.filter((x) => !known.has(x.key));

  return (
    <div className="view">
      <div className="view-body pad">
        <div className="cards">
          <Stat
            title="JVM"
            value={<span style={{ fontSize: 16 }}>{jvm.main_class || jvm.name || s.name}</span>}
            sub={[jvm.vm_name, jvm.java_version || jvm.vm_version, jvm.vm_vendor].filter(Boolean).join(" · ")}
          />
          {sm.end > sm.start && (
            <Stat
              title="Duration"
              value={formatNanos(sm.end - sm.start)}
              sub={new Date(sm.start / 1e6).toLocaleString()}
            />
          )}
          {samples.length > 0 && (
            <Stat
              title="Samples"
              value={formatCount(samples.reduce((a, [, v]) => a + v, 0))}
              sub={samples.map(([k, v]) => `${k} ${formatCount(v)}`).join(" · ")}
            />
          )}
          {sm.threads > 0 && <Stat title="Threads" value={sm.threads} />}
          {(!!jvm.max_heap || !!used) && (
            <Stat
              title="Heap"
              value={used ? formatBytes(latest(used)!) : formatBytes(jvm.max_heap!)}
              sub={jvm.max_heap ? `max ${formatBytes(jvm.max_heap)}` : undefined}
            />
          )}
          {gc.length > 0 && (
            <Stat
              title="Garbage collections"
              value={formatCount(gc.length)}
              sub={`${formatNanos(gcPause)} total pause`}
            />
          )}
          {sm.heap && (
            <>
              <Stat
                title="Objects"
                value={formatCount(sm.heap.objects)}
                sub={`${formatCount(sm.heap.classes)} classes`}
              />
              <Stat
                title="Shallow heap"
                value={formatBytes(sm.heap.total_shallow)}
                sub={`${formatCount(sm.heap.gc_roots)} GC roots`}
              />
            </>
          )}
          {!!jvm.cpus && (
            <Stat
              title="Machine"
              value={`${jvm.cpus} CPUs`}
              sub={[jvm.os, jvm.arch].filter(Boolean).join(" · ")}
            />
          )}
        </div>

        {(insights.data?.insights.length ?? 0) > 0 && (
          <>
            <div className="section-title">Insights</div>
            <InsightList insights={insights.data!.insights} />
          </>
        )}

        {hasTelemetry && (
          <>
            <div className="section-title">
              Telemetry{" "}
              <span className="faint" style={{ textTransform: "none", letterSpacing: 0, fontWeight: 400 }}>
                — drag across a chart to filter every view to that time range
              </span>
            </div>
            {tel.isLoading && <div className="spinner" />}
            <div className="chart-grid">
              {[
                ...GROUPS.map((g) => ({ ...g, list: g.keys.map((k) => byKey.get(k)) })),
                ...extra.map((x, i) => ({
                  title: x.label,
                  keys: [x.key],
                  colors: [`var(--c${(i % 8) + 1})`],
                  list: [x],
                })),
              ]
                .filter((g) => g.list.some((x) => x && x.t.length > 0))
                .map((g) => {
                  const cs: ChartSeries[] = [];
                  g.list.forEach((x, i) => {
                    if (x && x.t.length)
                      cs.push({ label: x.label, t: x.t, v: x.v, color: g.colors[i] ?? "var(--c8)" });
                  });
                  const unit = g.list.find(Boolean)!.unit;
                  const first = g.list.find(Boolean)!;
                  return (
                    <div className="card chart-card" key={g.title}>
                      <h3>
                        {g.title}
                        <span className="legend">
                          {cs.map((c) => (
                            <span key={c.label}>
                              <i style={{ background: c.color }} />
                              {c.label}
                            </span>
                          ))}
                        </span>
                        <span className="latest">
                          {latest(first) !== undefined ? formatValue(latest(first)!, unit) : ""}
                        </span>
                      </h3>
                      <TimeChart
                        series={cs}
                        unit={unit}
                        domain={domain}
                        gc={g.title === "Heap" || g.title === "GC activity" ? gc : undefined}
                        bookmarks={tel.data?.bookmarks}
                        range={range}
                        onSelectRange={(r) => store.set({ range: r })}
                      />
                    </div>
                  );
                })}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
