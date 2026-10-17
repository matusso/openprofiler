import { useMemo } from "react";
import { useThreads } from "../api/hooks";
import { DataTable, PctBar } from "../components/DataTable";
import { formatValue } from "../lib/format";
import { inspect, store, useApp } from "../state";
import { ProfileTrees } from "./profile/ProfileTrees";

function ThreadTotals() {
  const sid = useApp((s) => s.sessionId);
  const sel = useApp((s) => s.threads);
  const { data, isLoading } = useThreads(sid);
  const rows = useMemo(() => [...(data?.threads ?? [])].sort((a, b) => (b.cpu ?? 0) - (a.cpu ?? 0)), [data]);
  const total = rows.reduce((s, t) => s + (t.cpu ?? 0), 0);
  return (
    <DataTable
      label="Threads by CPU time"
      rows={rows}
      rowKey={(t) => t.index}
      selected={sel?.length === 1 ? sel[0] : null}
      onSelect={(t) => inspect({ type: "thread", thread: t })}
      onActivate={(t) => store.set({ threads: [t.index] })}
      empty={<div className="empty">{isLoading ? <div className="spinner" /> : "No threads"}</div>}
      columns={[
        {
          id: "name",
          header: "Thread (double-click to filter all views)",
          flex: true,
          render: (t) => t.name,
        },
        {
          id: "group",
          header: "Group",
          width: 140,
          render: (t) => <span className="muted">{t.group ?? ""}</span>,
        },
        {
          id: "flags",
          header: "",
          width: 120,
          render: (t) => (
            <>
              {t.is_virtual && <span className="badge">virtual</span>}{" "}
              {t.is_daemon && <span className="badge">daemon</span>}
            </>
          ),
        },
        {
          id: "cpu",
          header: "CPU",
          width: 100,
          align: "right",
          render: (t) => (t.cpu != null ? formatValue(t.cpu, "nanos") : "–"),
        },
        { id: "pct", header: "", width: 120, render: (t) => <PctBar value={t.cpu ?? 0} total={total} /> },
      ]}
    />
  );
}

export function CpuView() {
  return (
    <ProfileTrees
      view="cpu"
      title="CPU"
      kinds={["cpu", "wall", "io", "exception", "calls"]}
      extraTabs={[{ id: "threads", label: "Threads", render: () => <ThreadTotals /> }]}
    />
  );
}
