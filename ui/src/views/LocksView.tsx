import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import type { MonitorEventDto, MonitorStat } from "../api/types";
import { sk, useThreads } from "../api/hooks";
import { DataTable, PctBar } from "../components/DataTable";
import { formatCount, formatNanos, formatRelTime } from "../lib/format";
import { client, useApp } from "../state";
import { ProfileTrees } from "./profile/ProfileTrees";

function MonitorEvents() {
  const sid = useApp((s) => s.sessionId)!;
  const range = useApp((s) => s.range);
  const q = useQuery({
    queryKey: sk(sid, "monitors", "events", range),
    queryFn: () => client.monitorEvents(sid, range ?? undefined),
  });
  const threads = useThreads(sid);
  const summaryStart = q.data?.events.reduce((m, e) => Math.min(m, e.start), Infinity) ?? 0;
  const [sort, setSort] = useState("duration");
  const name = (i?: number | null) =>
    i == null ? "" : (threads.data?.threads.find((t) => t.index === i)?.name ?? `#${i}`);
  const rows = useMemo(() => {
    const r = [...(q.data?.events ?? [])];
    if (sort === "duration") r.sort((a, b) => b.duration - a.duration);
    else r.sort((a, b) => a.start - b.start);
    return r;
  }, [q.data, sort]);
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <DataTable<MonitorEventDto>
      label="Monitor events"
      rows={rows}
      rowKey={(_e, i) => i}
      sort={sort}
      onSort={setSort}
      empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No monitor events"}</div>}
      columns={[
        {
          id: "time",
          header: "Time",
          width: 90,
          sortKey: "time",
          render: (e) => formatRelTime(e.start, summaryStart),
        },
        {
          id: "dur",
          header: "Duration",
          width: 100,
          align: "right",
          sortKey: "duration",
          render: (e) => formatNanos(e.duration),
        },
        { id: "kind", header: "Type", width: 80, render: (e) => <span className="badge">{e.kind}</span> },
        {
          id: "mon",
          header: "Monitor class",
          flex: true,
          render: (e) => <span className="mono">{e.monitor_class}</span>,
        },
        { id: "thread", header: "Waiting thread", width: 200, render: (e) => name(e.thread) },
        { id: "owner", header: "Owner", width: 200, render: (e) => name(e.owner) },
      ]}
    />
  );
}

function MonitorStats() {
  const sid = useApp((s) => s.sessionId)!;
  const range = useApp((s) => s.range);
  const q = useQuery({
    queryKey: sk(sid, "monitors", "stats", range),
    queryFn: () => client.monitorStats(sid, range ?? undefined),
  });
  const rows = [...(q.data?.stats ?? [])].sort((a, b) => b.total_duration - a.total_duration);
  const total = rows.reduce((s, r) => s + r.total_duration, 0);
  return (
    <DataTable<MonitorStat>
      label="Monitor statistics"
      rows={rows}
      rowKey={(r) => r.monitor_class}
      empty={
        <div className="empty">{q.isLoading ? <div className="spinner" /> : "No monitor statistics"}</div>
      }
      columns={[
        {
          id: "mon",
          header: "Monitor class",
          flex: true,
          render: (r) => <span className="mono">{r.monitor_class}</span>,
        },
        { id: "count", header: "Events", width: 90, align: "right", render: (r) => formatCount(r.count) },
        {
          id: "total",
          header: "Total blocked",
          width: 110,
          align: "right",
          render: (r) => formatNanos(r.total_duration),
        },
        {
          id: "pct",
          header: "",
          width: 120,
          render: (r) => <PctBar value={r.total_duration} total={total} />,
        },
        { id: "max", header: "Max", width: 100, align: "right", render: (r) => formatNanos(r.max_duration) },
        {
          id: "avg",
          header: "Average",
          width: 100,
          align: "right",
          render: (r) => formatNanos(r.total_duration / Math.max(1, r.count)),
        },
      ]}
    />
  );
}

export function LocksView() {
  return (
    <ProfileTrees
      view="locks"
      title="Monitors & locks"
      kinds={["monitor", "wait"]}
      withStates={false}
      extraTabs={[
        { id: "events", label: "Monitor events", render: () => <MonitorEvents /> },
        { id: "stats", label: "Statistics", render: () => <MonitorStats /> },
      ]}
    />
  );
}
