import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import type { CompareKind, DiffNodeDto, HistogramDiffRow, TreeKind } from "../api/types";
import { useSessions } from "../api/hooks";
import { DataTable } from "../components/DataTable";
import { FlameGraph } from "../components/FlameGraph";
import { LazyTreeTable } from "../components/LazyTreeTable";
import { MethodLabel } from "../components/MethodLabel";
import { formatBytes, formatCount, formatValue } from "../lib/format";
import { client, store, useApp } from "../state";
import { KIND_LABELS, KIND_UNIT } from "./profile/shared";

function Delta({ a, b, fmt }: { a: number; b: number; fmt: (v: number) => string }) {
  const d = b - a;
  const pct = a ? (d / a) * 100 : b ? Infinity : 0;
  const color = d > 0 ? "var(--diff-pos)" : d < 0 ? "var(--diff-neg)" : "var(--fg-faint)";
  return (
    <span style={{ color }}>
      {d > 0 ? "+" : ""}
      {fmt(d)} {Number.isFinite(pct) ? `(${pct > 0 ? "+" : ""}${pct.toFixed(0)}%)` : "(new)"}
    </span>
  );
}

const TREE_KINDS: TreeKind[] = ["cpu", "wall", "alloc", "monitor", "wait", "io"];

export function CompareView() {
  const { data: sessions } = useSessions();
  const a = useApp((s) => s.compareA);
  const b = useApp((s) => s.compareB);
  const cur = useApp((s) => s.sessionId);
  const ready = (sessions ?? []).filter((s) => s.state !== "loading" && s.state !== "failed");
  const sa = ready.find((s) => s.id === (a ?? cur));
  const sb = ready.find((s) => s.id === b);
  const both = sa && sb;
  const common = both
    ? ([
        ...TREE_KINDS.filter((k) => sa.capabilities.includes(k) && sb.capabilities.includes(k)),
      ] as CompareKind[])
    : [];
  if (both && sa.capabilities.includes("heap") && sb.capabilities.includes("heap"))
    common.push("heap_histogram");
  const [kindSel, setKind] = useState<CompareKind>("cpu");
  const kind = common.includes(kindSel) ? kindSel : common[0];
  const [mode, setMode] = useState<"tree" | "flame">("tree");

  const pick = (label: string, value: string | undefined, set: (v: string) => void) => (
    <label className="check">
      <span className="label">{label}</span>
      <select
        className="select"
        value={value ?? ""}
        onChange={(e) => set(e.target.value)}
        aria-label={`Session ${label}`}
      >
        <option value="">Choose session…</option>
        {ready.map((s) => (
          <option key={s.id} value={s.id}>
            {s.name} ({s.kind})
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Compare</h1>
        {pick("Baseline A", sa?.id, (v) => store.set({ compareA: v || null }))}
        <span className="faint">→</span>
        {pick("B", sb?.id, (v) => store.set({ compareB: v || null }))}
        {common.length > 0 && (
          <select
            className="select"
            value={kind}
            onChange={(e) => setKind(e.target.value as CompareKind)}
            aria-label="Comparison kind"
          >
            {common.map((k) => (
              <option key={k} value={k}>
                {k === "heap_histogram"
                  ? "Heap histogram"
                  : k === "telemetry"
                    ? "Telemetry"
                    : KIND_LABELS[k as TreeKind]}
              </option>
            ))}
          </select>
        )}
        {kind && kind !== "heap_histogram" && (
          <div className="seg" role="group" aria-label="Diff display">
            <button aria-pressed={mode === "tree"} onClick={() => setMode("tree")}>
              Tree
            </button>
            <button aria-pressed={mode === "flame"} onClick={() => setMode("flame")}>
              Differential flame
            </button>
          </div>
        )}
      </div>
      <div className="view-body" style={{ marginTop: 10, borderTop: "1px solid var(--border)" }}>
        {!both ? (
          <div className="empty">
            <h2>Pick two sessions</h2>
            <p>
              Open two snapshots, recordings or heap dumps, then choose a baseline (A) and a comparison (B).
            </p>
          </div>
        ) : !kind ? (
          <div className="empty">These sessions have no comparable data.</div>
        ) : kind === "heap_histogram" ? (
          <HistogramDiff a={sa.id} b={sb.id} />
        ) : mode === "tree" ? (
          <DiffTree a={sa.id} b={sb.id} kind={kind as TreeKind} />
        ) : (
          <DiffFlame a={sa.id} b={sb.id} kind={kind as TreeKind} />
        )}
      </div>
    </div>
  );
}

function DiffTree({ a, b, kind }: { a: string; b: string; kind: TreeKind }) {
  const unit = KIND_UNIT[kind];
  const agg = useApp((s) => s.aggregation);
  const req = { a, b, kind, aggregation: agg };
  const fmt = (v: number) => formatValue(v, unit);
  return (
    <LazyTreeTable<DiffNodeDto>
      label="Comparison tree"
      queryKey={["compare", req]}
      fetch={async (node, depth) => (await client.compare({ ...req, node, depth })).nodes}
      weight={(n) => Math.max(n.a_total, n.b_total)}
      renderName={(n) =>
        n.parent === null ? <span className="muted">{n.label}</span> : <MethodLabel label={n.label} />
      }
      columns={[
        { id: "a", header: "A total", width: 100, render: (n) => fmt(n.a_total) },
        { id: "b", header: "B total", width: 100, render: (n) => fmt(n.b_total) },
        {
          id: "d",
          header: "Δ total",
          width: 170,
          render: (n) => <Delta a={n.a_total} b={n.b_total} fmt={fmt} />,
        },
        {
          id: "ds",
          header: "Δ self",
          width: 150,
          render: (n) => <Delta a={n.a_self} b={n.b_self} fmt={fmt} />,
        },
      ]}
    />
  );
}

function DiffFlame({ a, b, kind }: { a: string; b: string; kind: TreeKind }) {
  const agg = useApp((s) => s.aggregation);
  const q = useQuery({
    queryKey: ["compare-flame", a, b, kind, agg],
    queryFn: () => client.compareFlame({ a, b, kind, aggregation: agg }),
  });
  if (q.isLoading)
    return (
      <div className="empty">
        <div className="spinner" />
      </div>
    );
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <span className="legend">
          <span>
            <i style={{ background: "var(--diff-pos)" }} /> more in B
          </span>
          <span>
            <i style={{ background: "var(--diff-neg)" }} /> less in B
          </span>
        </span>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <FlameGraph layout={q.data!} unit={KIND_UNIT[kind]} label="Differential flame graph" />
      </div>
    </div>
  );
}

function HistogramDiff({ a, b }: { a: string; b: string }) {
  const q = useQuery({ queryKey: ["histogram-diff", a, b], queryFn: () => client.heapHistogramDiff(a, b) });
  const [sort, setSort] = useState("shallow");
  const [filter, setFilter] = useState("");
  const rows = useMemo(() => {
    const r = (q.data?.rows ?? []).filter((x) => x.name.toLowerCase().includes(filter.toLowerCase()));
    const key = (x: HistogramDiffRow) =>
      sort === "instances" ? Math.abs(x.b_instances - x.a_instances) : Math.abs(x.b_shallow - x.a_shallow);
    return [...r].sort((x, y) => key(y) - key(x));
  }, [q.data, sort, filter]);
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <input
          className="input"
          placeholder="Filter classes"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-view-search
          aria-label="Filter classes"
        />
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <DataTable<HistogramDiffRow>
          label="Heap histogram difference"
          rows={rows}
          rowKey={(r) => r.name}
          sort={sort}
          onSort={setSort}
          empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No differences"}</div>}
          columns={[
            { id: "n", header: "Class", flex: true, render: (r) => <span className="mono">{r.name}</span> },
            {
              id: "ai",
              header: "A instances",
              width: 100,
              align: "right",
              render: (r) => formatCount(r.a_instances),
            },
            {
              id: "bi",
              header: "B instances",
              width: 100,
              align: "right",
              render: (r) => formatCount(r.b_instances),
            },
            {
              id: "di",
              header: "Δ instances",
              width: 150,
              align: "right",
              sortKey: "instances",
              render: (r) => <Delta a={r.a_instances} b={r.b_instances} fmt={formatCount} />,
            },
            {
              id: "ds",
              header: "Δ shallow",
              width: 170,
              align: "right",
              sortKey: "shallow",
              render: (r) => <Delta a={r.a_shallow} b={r.b_shallow} fmt={formatBytes} />,
            },
          ]}
        />
      </div>
    </div>
  );
}
