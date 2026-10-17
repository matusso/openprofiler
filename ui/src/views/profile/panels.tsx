import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import type {
  Aggregation,
  FlameLayout,
  Focus,
  HotspotDto,
  TreeKind,
  TreeNodeDto,
  TreeRequest,
  Unit,
} from "../../api/types";
import { sk } from "../../api/hooks";
import { DataTable, PctBar } from "../../components/DataTable";
import { FlameGraph } from "../../components/FlameGraph";
import { GraphView, type GEdge, type GNode } from "../../components/GraphView";
import { LazyTreeTable } from "../../components/LazyTreeTable";
import { MethodLabel } from "../../components/MethodLabel";
import { formatCount, formatValue } from "../../lib/format";
import { goToSource, sourceTarget } from "../../lib/source";
import { client, focusMethod, inspect, openContextMenu, useApp, type MenuItem } from "../../state";
import { KIND_COUNT_LABEL, KIND_UNIT, VIEW_OF_KIND, type TreeParams } from "./shared";

const ROOT_KEY = 4294967295;

/** Context-menu entries shared by every frame (tree row, hot spot, flame rectangle). */
export function frameMenu(
  kind: TreeKind,
  aggregation: Aggregation,
  label: string,
  key: number | undefined,
  cls?: string,
  method?: string,
): MenuItem[] {
  const target = sourceTarget(label, aggregation, cls, method);
  const view = VIEW_OF_KIND[kind];
  const hasKey = key !== undefined && key !== ROOT_KEY;
  return [
    { label: "Go to source", run: () => void goToSource(target), disabled: !target },
    {
      label: "Cumulated outgoing calls",
      run: () => hasKey && focusMethod(view, kind, key, label, "callees"),
      disabled: !hasKey,
    },
    {
      label: "Show back traces (callers)",
      run: () => hasKey && focusMethod(view, kind, key, label, "callers"),
      disabled: !hasKey,
    },
    { label: "Copy name", run: () => void navigator.clipboard?.writeText(label) },
  ];
}

function treeColumns(kind: TreeKind, unit: Unit, total: () => number) {
  // A calls node's own invocations are its self count; its total count adds its callees'.
  const own = kind === "calls";
  return [
    {
      id: "total",
      header: "Total",
      width: 210,
      render: (n: TreeNodeDto) => (
        <span className="bar-cell" style={{ justifyContent: "flex-end" }}>
          <span className="num" style={{ width: 80 }}>
            {formatValue(n.total, unit)}
          </span>
          <PctBar value={n.total} total={total()} />
        </span>
      ),
    },
    { id: "self", header: "Self", width: 90, render: (n: TreeNodeDto) => formatValue(n.self, unit) },
    {
      id: "count",
      header: KIND_COUNT_LABEL[kind] ?? "Samples",
      width: 80,
      render: (n: TreeNodeDto) => formatCount(own ? n.self_count : n.total_count),
    },
  ];
}

/** Lazy call tree (or back-trace tree) for any sample kind. */
export function CallTreePanel({
  sessionId,
  params,
  view = "calltree",
  focus,
  rootNode,
  search,
  findToken,
}: {
  sessionId: string;
  params: TreeParams;
  view?: TreeRequest["view"];
  focus?: Focus;
  rootNode?: number;
  search?: string;
  findToken?: string;
}) {
  const unit = KIND_UNIT[params.kind];
  const [total, setTotal] = useState(0);
  const gen = useApp((s) => s.sessionId) === sessionId;
  const req: TreeRequest = { ...params, view, focus };
  const fetch = async (node: number | undefined, levels: number) => {
    const r = await client.tree(sessionId, { ...req, node: node ?? rootNode, depth: levels });
    if (node === undefined) setTotal(r.total);
    return r.nodes;
  };
  return (
    <LazyTreeTable<TreeNodeDto>
      label={view === "backtraces" ? "Back traces" : "Call tree"}
      queryKey={sk(sessionId, "tree", req, rootNode ?? null, gen)}
      fetch={fetch}
      weight={(n) => n.total}
      search={search}
      findToken={findToken}
      text={(n) => n.label}
      renderName={(n) =>
        n.parent === null ? (
          <span className="muted">{n.label}</span>
        ) : (
          <MethodLabel label={n.label} kind={n.kind} q={search} />
        )
      }
      columns={treeColumns(params.kind, unit, () => total)}
      onSelect={(n) => inspect({ type: "node", kind: params.kind, node: n, total, unit })}
      onContextMenu={(n, e) =>
        n.parent !== null &&
        openContextMenu(e, frameMenu(params.kind, params.aggregation, n.label, n.key, n.class, n.method))
      }
    />
  );
}

export function HotspotsPanel({
  sessionId,
  params,
  search,
}: {
  sessionId: string;
  params: TreeParams;
  search?: string;
}) {
  const unit = KIND_UNIT[params.kind];
  const req = {
    kind: params.kind,
    filter: params.filter,
    aggregation: params.aggregation,
    compact_prefixes: params.compact_prefixes,
    limit: 2000,
  };
  const q = useQuery({
    queryKey: sk(sessionId, "hotspots", req),
    queryFn: () => client.hotspots(sessionId, req),
  });
  const [picked, setSel] = useState<HotspotDto | null>(null);
  const rows = useMemo(() => {
    const all = q.data?.hotspots ?? [];
    const s = search?.trim().toLowerCase();
    return s ? all.filter((h) => h.label.toLowerCase().includes(s)) : all;
  }, [q.data, search]);
  // Default to the top hot spot; a stale pick (from another filter) falls back too.
  const sel = (picked && rows.find((h) => h.key === picked.key)) ?? rows[0] ?? null;
  const total = q.data?.total ?? 0;
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <div className="split-v">
      <DataTable<HotspotDto>
        label="Hot spots"
        rows={rows}
        rowKey={(h) => h.key}
        selected={sel?.key ?? null}
        onSelect={(h) => setSel(h)}
        onContextMenu={(h, _i, e) =>
          openContextMenu(e, frameMenu(params.kind, params.aggregation, h.label, h.key))
        }
        empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No samples"}</div>}
        columns={[
          {
            id: "name",
            header: "Hot spot",
            flex: true,
            render: (h) => <MethodLabel label={h.label} q={search} />,
          },
          {
            id: "self",
            header: "Self",
            width: 210,
            align: "right",
            render: (h) => (
              <span className="bar-cell" style={{ justifyContent: "flex-end" }}>
                <span className="num" style={{ width: 80 }}>
                  {formatValue(h.self, unit)}
                </span>
                <PctBar value={h.self} total={total} self />
              </span>
            ),
          },
          {
            id: "total",
            header: "Inherent total",
            width: 110,
            align: "right",
            render: (h) => formatValue(h.total, unit),
          },
          {
            id: "count",
            header: KIND_COUNT_LABEL[params.kind] ?? "Samples",
            width: 80,
            align: "right",
            render: (h) => formatCount(h.self_count),
          },
        ]}
      />
      <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
        <div className="toolbar" style={{ padding: "6px 16px" }}>
          <span className="label">Back traces</span>
          {sel && <MethodLabel label={sel.label} />}
        </div>
        <div style={{ flex: 1, minHeight: 0 }}>
          {sel ? (
            <CallTreePanel
              sessionId={sessionId}
              params={params}
              view="backtraces"
              rootNode={sel.node}
              focus={sel.node === undefined ? { callers: sel.key } : undefined}
            />
          ) : (
            <div className="empty">Select a hot spot to see where it is called from.</div>
          )}
        </div>
      </div>
    </div>
  );
}

export function FlamePanel({
  sessionId,
  params,
  search,
  focus,
}: {
  sessionId: string;
  params: TreeParams;
  search?: string;
  focus?: Focus;
}) {
  const unit = KIND_UNIT[params.kind];
  const [orientation, setOrientation] = useState<"icicle" | "flame">("icicle");
  const [inverted, setInverted] = useState(false);
  const [sandwich, setSandwich] = useState<{ key: number; label: string } | null>(null);
  const req = { ...params, inverted, focus };
  const q = useQuery({ queryKey: sk(sessionId, "flame", req), queryFn: () => client.flame(sessionId, req) });
  const qc = useQueryClient();
  const [sandwichErr, setSandwichErr] = useState<string | null>(null);
  const openSandwich = async ({ label, key: known }: { label: string; key?: number }) => {
    if (known !== undefined && known !== ROOT_KEY) {
      setSandwichErr(null);
      setSandwich({ key: known, label });
      return;
    }
    // Older daemons without a `key` column: resolve by label via hot spots.
    const r = await qc.fetchQuery({
      queryKey: sk(sessionId, "hotspots-keys", params.kind, params.aggregation),
      queryFn: () =>
        client.hotspots(sessionId, { kind: params.kind, aggregation: params.aggregation, limit: 100000 }),
    });
    const key = r.hotspots.find((h) => h.label === label)?.key;
    if (key === undefined) setSandwichErr(`Could not resolve “${label}” to a method key.`);
    else {
      setSandwichErr(null);
      setSandwich({ key, label });
    }
  };
  const callers = useQuery({
    queryKey: sk(sessionId, "flame-callers", params, sandwich?.key),
    queryFn: () => client.flame(sessionId, { ...params, focus: { callers: sandwich!.key } }),
    enabled: !!sandwich,
  });
  const callees = useQuery({
    queryKey: sk(sessionId, "flame-callees", params, sandwich?.key),
    queryFn: () => client.flame(sessionId, { ...params, focus: { callees: sandwich!.key } }),
    enabled: !!sandwich,
  });
  const [picked, setPicked] = useState<{ label: string; key?: number } | null>(null);

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <div className="seg" role="group" aria-label="Orientation">
          <button aria-pressed={orientation === "icicle"} onClick={() => setOrientation("icicle")}>
            Icicle
          </button>
          <button aria-pressed={orientation === "flame"} onClick={() => setOrientation("flame")}>
            Flame
          </button>
        </div>
        <label className="check">
          <input type="checkbox" checked={inverted} onChange={(e) => setInverted(e.target.checked)} />
          Inverted (hot spots at the root)
        </label>
        <span className="grow" />
        {picked && (
          <button
            className="btn"
            onClick={() => void openSandwich(picked)}
            title="Callers and callees of the selected frame"
          >
            Sandwich: {picked.label.split("(")[0].split(".").slice(-2).join(".")}
          </button>
        )}
        {sandwichErr && <span className="error">{sandwichErr}</span>}
        {sandwich && (
          <button className="btn on" onClick={() => setSandwich(null)}>
            Close sandwich
          </button>
        )}
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        {sandwich ? (
          <SandwichView
            label={sandwich.label}
            callers={callers.data}
            callees={callees.data}
            unit={unit}
            search={search}
          />
        ) : q.isLoading ? (
          <div className="empty">
            <div className="spinner" />
          </div>
        ) : q.isError ? (
          <div className="error">{String(q.error.message)}</div>
        ) : q.data && q.data.total > 0 ? (
          <FlameGraph
            layout={q.data}
            unit={unit}
            orientation={orientation}
            search={search}
            onSelect={(label, i) => {
              setPicked(i === 0 ? null : { label, key: q.data!.key?.[i] });
            }}
            onContextMenu={(label, i, e) => {
              const key = q.data!.key?.[i];
              openContextMenu(e, [
                ...frameMenu(params.kind, params.aggregation, label, key),
                { label: "Sandwich view", run: () => void openSandwich({ label, key }) },
              ]);
            }}
          />
        ) : (
          <div className="empty">No samples for the current selection.</div>
        )}
      </div>
    </div>
  );
}

function SandwichView({
  label,
  callers,
  callees,
  unit,
  search,
}: {
  label: string;
  callers?: FlameLayout;
  callees?: FlameLayout;
  unit: Unit;
  search?: string;
}) {
  return (
    <div className="split-v">
      <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
        <div className="toolbar" style={{ padding: "4px 16px" }}>
          <span className="label">Callers of</span> <MethodLabel label={label} />
        </div>
        <div style={{ flex: 1, minHeight: 0 }}>
          {callers ? (
            <FlameGraph layout={callers} unit={unit} orientation="flame" search={search} />
          ) : (
            <div className="spinner" />
          )}
        </div>
      </div>
      <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
        <div className="toolbar" style={{ padding: "4px 16px" }}>
          <span className="label">Callees of</span> <MethodLabel label={label} />
        </div>
        <div style={{ flex: 1, minHeight: 0 }}>
          {callees ? (
            <FlameGraph layout={callees} unit={unit} search={search} />
          ) : (
            <div className="spinner" />
          )}
        </div>
      </div>
    </div>
  );
}

export function CallGraphPanel({ sessionId, params }: { sessionId: string; params: TreeParams }) {
  const unit = KIND_UNIT[params.kind];
  const hs = useQuery({
    queryKey: sk(sessionId, "hotspots", {
      kind: params.kind,
      filter: params.filter,
      aggregation: params.aggregation,
      limit: 200,
    }),
    queryFn: () =>
      client.hotspots(sessionId, {
        kind: params.kind,
        filter: params.filter,
        aggregation: params.aggregation,
        limit: 200,
      }),
  });
  const [focusKey, setFocusKey] = useState<number | null>(null);
  const fk = focusKey ?? hs.data?.hotspots[0]?.key;
  const req = {
    kind: params.kind,
    filter: params.filter,
    aggregation: params.aggregation,
    focus_key: fk,
    limit: 60,
  };
  const g = useQuery({
    queryKey: sk(sessionId, "callgraph", req),
    queryFn: () => client.callgraph(sessionId, req),
    enabled: fk !== undefined,
  });
  const { nodes, edges } = useMemo(() => {
    const d = g.data;
    if (!d) return { nodes: [] as GNode[], edges: [] as GEdge[] };
    const maxT = Math.max(1, ...d.nodes.map((n) => n.total));
    const maxW = Math.max(1, ...d.edges.map((e) => e.weight));
    const short = (l: string) => l.split("(")[0].split(".").slice(-2).join(".");
    return {
      nodes: d.nodes.map((n, i) => ({
        id: String(i),
        label: short(n.label),
        sub: `${formatValue(n.total, unit)} total · ${formatValue(n.self, unit)} self`,
        focus: n.key === fk,
        heat: n.total / maxT,
      })),
      edges: d.edges.map((e, i) => ({
        id: `e${i}`,
        source: String(e.from),
        target: String(e.to),
        weight: e.weight / maxW,
        label: formatValue(e.weight, unit),
      })),
    };
  }, [g.data, fk, unit]);
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <span className="label">Focus</span>
        <select
          className="select"
          style={{ maxWidth: 520 }}
          value={fk ?? ""}
          onChange={(e) => setFocusKey(Number(e.target.value))}
          aria-label="Call graph focus method"
        >
          {hs.data?.hotspots.map((h) => (
            <option key={h.key} value={h.key}>
              {h.label}
            </option>
          ))}
        </select>
        <span className="faint" style={{ fontSize: 11.5 }}>
          Click a node to refocus · scroll to zoom · drag to pan
        </span>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        {g.isError ? (
          <div className="error">{String(g.error.message)}</div>
        ) : (
          <GraphView
            label="Call graph"
            nodes={nodes}
            edges={edges}
            onNodeClick={(id) => {
              const n = g.data?.nodes[Number(id)];
              if (n) setFocusKey(n.key);
            }}
          />
        )}
      </div>
    </div>
  );
}
