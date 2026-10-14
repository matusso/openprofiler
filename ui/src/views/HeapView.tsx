import { useInfiniteQuery, useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import type { ClassRow, Insight, ObjectRow, ObjectSetDto, RefRow, SelectStep } from "../api/types";
import { sk, useCurrentSession } from "../api/hooks";
import { DataTable, PctBar } from "../components/DataTable";
import { LazyTreeTable } from "../components/LazyTreeTable";
import { Sunburst } from "../components/Sunburst";
import { formatBytes, formatCount } from "../lib/format";
import { client, inspect, store, useApp } from "../state";
import { InsightList } from "./OverviewView";

type Tab = "classes" | "objects" | "biggest" | "references" | "inspections";
const PAGE = 500;

function useSelect(sessionId: string) {
  return useMutation({
    mutationFn: ({ from, step }: { from?: number; step: SelectStep }) =>
      client.heapSelect(sessionId, { from, step }),
    onSuccess: (set) => {
      const trail = store.get().heapTrail;
      store.set({ heapSet: set, heapTrail: [...trail, set] });
    },
  });
}

function SetRetained({ sessionId, set }: { sessionId: string; set: ObjectSetDto }) {
  const m = useMutation({ mutationFn: () => client.heapSetRetained(sessionId, set.id) });
  if (set.retained != null || m.data)
    return <span className="badge accent">retained {formatBytes(m.data?.retained ?? set.retained!)}</span>;
  return (
    <button
      className="btn"
      onClick={() => m.mutate()}
      disabled={m.isPending}
      title="Memory freed if this whole set were collected"
    >
      {m.isPending ? "Calculating…" : "Calculate retained size"}
    </button>
  );
}

function Trail() {
  const trail = useApp((s) => s.heapTrail);
  const set = useApp((s) => s.heapSet);
  return (
    <nav className="breadcrumb" aria-label="Object set selection steps">
      <button aria-current={!set} onClick={() => store.set({ heapSet: null, heapTrail: [] })}>
        All objects
      </button>
      {trail.map((t, i) => (
        <span key={t.id} style={{ display: "contents" }}>
          <span className="chev">›</span>
          <button
            aria-current={set?.id === t.id}
            onClick={() => store.set({ heapSet: t, heapTrail: trail.slice(0, i + 1) })}
            title={`${formatCount(t.objects)} objects · ${formatBytes(t.shallow)} shallow`}
          >
            {t.label} <span className="faint">({formatCount(t.objects)})</span>
          </button>
        </span>
      ))}
    </nav>
  );
}

function ClassesTab({ sessionId, set }: { sessionId: string; set: ObjectSetDto | null }) {
  const [groupBy, setGroupBy] = useState<"class" | "package" | "classloader">("class");
  const [sort, setSort] = useState<"instances" | "shallow" | "retained" | "name">("retained");
  const [filter, setFilter] = useState("");
  const [sel, setSel] = useState<string | null>(null);
  const select = useSelect(sessionId);
  const req = { set: set?.id, group_by: groupBy, sort, filter: filter || undefined };
  const q = useInfiniteQuery({
    queryKey: sk(sessionId, "heap-classes", req),
    queryFn: ({ pageParam }) => client.heapClasses(sessionId, { ...req, offset: pageParam, limit: PAGE }),
    initialPageParam: 0,
    getNextPageParam: (last, pages) => {
      const loaded = pages.reduce((a, p) => a + p.rows.length, 0);
      return loaded < last.total_rows ? loaded : undefined;
    },
  });
  const rows = useMemo(() => q.data?.pages.flatMap((p) => p.rows) ?? [], [q.data]);
  const totalRows = q.data?.pages[0]?.total_rows ?? 0;
  const totalShallow = set?.shallow ?? rows.reduce((a, r) => a + r.shallow, 0);
  const totalRetained = rows.reduce((a, r) => Math.max(a, r.retained ?? 0), 0);
  const drill = (r: ClassRow) => {
    if (groupBy === "class" && r.class_id !== undefined)
      select.mutate({ from: set?.id, step: { class_id: r.class_id } });
    else if (groupBy === "package") select.mutate({ from: set?.id, step: { package: r.name } });
    else select.mutate({ from: set?.id, step: { class_name: r.name } });
  };
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <div className="seg" role="group" aria-label="Group by">
          {(["class", "package", "classloader"] as const).map((g) => (
            <button key={g} aria-pressed={groupBy === g} onClick={() => setGroupBy(g)}>
              {g === "classloader" ? "Class loader" : g[0].toUpperCase() + g.slice(1)}
            </button>
          ))}
        </div>
        <input
          className="input"
          placeholder="Filter (/)"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-view-search
          aria-label="Filter classes"
          style={{ width: 260 }}
        />
        <span className="grow" />
        <span className="faint">{formatCount(totalRows)} rows · double-click to select instances</span>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        {q.isError ? (
          <div className="error">{String(q.error.message)}</div>
        ) : (
          <DataTable<ClassRow>
            label="Classes"
            rows={rows}
            rowKey={(r) => r.name}
            selected={sel}
            onSelect={(r) => setSel(r.name)}
            onActivate={drill}
            sort={sort}
            onSort={(k) => setSort(k as typeof sort)}
            totalRows={totalRows}
            onLoadMore={() => !q.isFetchingNextPage && q.hasNextPage && q.fetchNextPage()}
            empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No classes"}</div>}
            columns={[
              {
                id: "name",
                header: groupBy === "class" ? "Class" : groupBy === "package" ? "Package" : "Class loader",
                flex: true,
                sortKey: "name",
                render: (r) => <span className="mono">{r.name}</span>,
              },
              {
                id: "inst",
                header: "Instances",
                width: 100,
                align: "right",
                sortKey: "instances",
                render: (r) => formatCount(r.instances),
              },
              {
                id: "shallow",
                header: "Shallow",
                width: 100,
                align: "right",
                sortKey: "shallow",
                render: (r) => formatBytes(r.shallow),
              },
              {
                id: "sp",
                header: "",
                width: 124,
                render: (r) => <PctBar value={r.shallow} total={totalShallow} self />,
              },
              {
                id: "ret",
                header: "Retained (≥)",
                width: 110,
                align: "right",
                sortKey: "retained",
                render: (r) => (r.retained != null ? formatBytes(r.retained) : "–"),
              },
              {
                id: "rp",
                header: "",
                width: 124,
                render: (r) =>
                  r.retained != null ? (
                    <PctBar value={r.retained} total={Math.max(totalRetained, totalShallow)} />
                  ) : null,
              },
            ]}
          />
        )}
      </div>
    </div>
  );
}

function objectColumns(withRetained = true) {
  return [
    {
      id: "cls",
      header: "Object",
      flex: true,
      render: (o: ObjectRow) => (
        <span className="mono">
          {o.class_name} <span className="faint">{o.address}</span>
        </span>
      ),
    },
    {
      id: "prev",
      header: "Value",
      width: 220,
      render: (o: ObjectRow) => <span className="mono muted">{o.preview ?? ""}</span>,
      title: (o: ObjectRow) => o.preview ?? "",
    },
    {
      id: "sh",
      header: "Shallow",
      width: 90,
      align: "right" as const,
      render: (o: ObjectRow) => formatBytes(o.shallow),
    },
    ...(withRetained
      ? [
          {
            id: "ret",
            header: "Retained",
            width: 100,
            align: "right" as const,
            render: (o: ObjectRow) => (o.retained != null ? formatBytes(o.retained) : "–"),
          },
        ]
      : []),
  ];
}

function ObjectsTab({ sessionId, set }: { sessionId: string; set: ObjectSetDto | null }) {
  const [sort, setSort] = useState<"shallow" | "retained">("retained");
  const sel = useApp((s) => s.heapObject);
  const req = { set: set?.id, sort };
  const q = useInfiniteQuery({
    queryKey: sk(sessionId, "heap-objects", req),
    queryFn: ({ pageParam }) => client.heapObjects(sessionId, { ...req, offset: pageParam, limit: PAGE }),
    initialPageParam: 0,
    getNextPageParam: (last, pages) => {
      const loaded = pages.reduce((a, p) => a + p.rows.length, 0);
      return loaded < last.total_rows ? loaded : undefined;
    },
  });
  const rows = useMemo(() => q.data?.pages.flatMap((p) => p.rows) ?? [], [q.data]);
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <DataTable<ObjectRow>
      label="Objects"
      rows={rows}
      rowKey={(o) => o.id}
      selected={sel}
      onSelect={(o) => (store.set({ heapObject: o.id }), inspect({ type: "object", id: o.id }))}
      onActivate={(o) => store.set({ heapObject: o.id })}
      sort={sort}
      onSort={(k) => setSort(k as typeof sort)}
      totalRows={q.data?.pages[0]?.total_rows}
      onLoadMore={() => !q.isFetchingNextPage && q.hasNextPage && q.fetchNextPage()}
      empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No objects"}</div>}
      columns={objectColumns().map((c) =>
        c.id === "sh" ? { ...c, sortKey: "shallow" } : c.id === "ret" ? { ...c, sortKey: "retained" } : c,
      )}
    />
  );
}

type DomNode = ObjectRow & { parent: number | null; child_count: number };

function BiggestTab({ sessionId }: { sessionId: string }) {
  const [mode, setMode] = useState<"tree" | "sunburst">("tree");
  const fetch = async (node: number | undefined): Promise<DomNode[]> => {
    const r = await client.heapBiggest(sessionId, { object: node, limit: 200 });
    const self: DomNode =
      node === undefined
        ? {
            id: -1,
            address: "",
            class_name: "Dominator tree roots",
            shallow: 0,
            retained: r.rows.reduce((a, x) => a + (x.retained ?? 0), 0),
            parent: null,
            child_count: r.rows.length,
          }
        : { id: node, address: "", class_name: "", shallow: 0, parent: null, child_count: r.rows.length };
    const parent = node ?? -1;
    return [self, ...r.rows.map((x) => ({ ...x, parent, child_count: x.child_count ?? 1 }))];
  };
  const [total, setTotal] = useState(0);
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <div className="seg" role="group" aria-label="Display">
          <button aria-pressed={mode === "tree"} onClick={() => setMode("tree")}>
            Dominator tree
          </button>
          <button aria-pressed={mode === "sunburst"} onClick={() => setMode("sunburst")}>
            Sunburst
          </button>
        </div>
        <span className="faint">
          Objects that keep the most memory alive. Retained size = memory freed if the object were collected.
        </span>
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        {mode === "tree" ? (
          <LazyTreeTable<DomNode>
            label="Biggest objects (dominator tree)"
            queryKey={sk(sessionId, "heap-dom-tree")}
            initialLevels={1}
            expandLevels={1}
            autoExpand={false}
            fetch={async (node) => {
              const nodes = await fetch(node);
              if (node === undefined) setTotal(nodes[0].retained ?? 0);
              return nodes;
            }}
            weight={(n) => n.retained ?? 0}
            onSelect={(n) =>
              n.id >= 0 && (store.set({ heapObject: n.id }), inspect({ type: "object", id: n.id }))
            }
            renderName={(n) => (
              <span className="mono" title={n.preview ?? undefined}>
                {n.class_name} <span className="faint">{n.address}</span>{" "}
                {n.preview && <span className="muted">{n.preview}</span>}
              </span>
            )}
            columns={[
              {
                id: "ret",
                header: "Retained",
                width: 200,
                render: (n) => (
                  <span className="bar-cell" style={{ justifyContent: "flex-end" }}>
                    <span className="num" style={{ width: 80 }}>
                      {formatBytes(n.retained ?? 0)}
                    </span>
                    <PctBar value={n.retained ?? 0} total={total} />
                  </span>
                ),
              },
              { id: "sh", header: "Shallow", width: 90, render: (n) => formatBytes(n.shallow) },
            ]}
          />
        ) : (
          <Sunburst
            sessionId={sessionId}
            onSelect={(o) => (store.set({ heapObject: o.id }), inspect({ type: "object", id: o.id }))}
          />
        )}
      </div>
    </div>
  );
}

function ReferencesTab({ sessionId, set }: { sessionId: string; set: ObjectSetDto | null }) {
  const obj = useApp((s) => s.heapObject);
  const [dir, setDir] = useState<"incoming" | "outgoing">("incoming");
  const [scope, setScope] = useState<"object" | "set">(obj !== null ? "object" : "set");
  const [excludeWeak, setExcludeWeak] = useState(true);
  const [history, setHistory] = useState<number[]>([]);
  const refs = useQuery({
    queryKey: sk(sessionId, "heap-refs", obj, dir),
    queryFn: () => client.heapRefs(sessionId, { object: obj!, direction: dir, limit: 1000 }),
    enabled: scope === "object" && obj !== null,
  });
  const merged = useQuery({
    queryKey: sk(sessionId, "heap-merged-refs", set?.id ?? 0, dir),
    queryFn: () => client.heapMergedRefs(sessionId, { set: set?.id ?? 0, direction: dir }),
    enabled: scope === "set",
  });
  const paths = useQuery({
    queryKey: sk(sessionId, "heap-paths", obj, excludeWeak),
    queryFn: () =>
      client.heapPathToRoots(sessionId, { object: obj!, max_paths: 5, exclude_weak: excludeWeak }),
    enabled: scope === "object" && obj !== null,
  });
  const go = (id: number) => {
    if (obj !== null) setHistory([...history, obj]);
    store.set({ heapObject: id });
    inspect({ type: "object", id });
  };
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <div className="seg" role="group" aria-label="Scope">
          <button aria-pressed={scope === "object"} onClick={() => setScope("object")}>
            Selected object
          </button>
          <button aria-pressed={scope === "set"} onClick={() => setScope("set")}>
            Current set (merged)
          </button>
        </div>
        <div className="seg" role="group" aria-label="Direction">
          <button aria-pressed={dir === "incoming"} onClick={() => setDir("incoming")}>
            Incoming
          </button>
          <button aria-pressed={dir === "outgoing"} onClick={() => setDir("outgoing")}>
            Outgoing
          </button>
        </div>
        {scope === "object" && history.length > 0 && (
          <button
            className="btn"
            onClick={() => (
              store.set({ heapObject: history[history.length - 1] }),
              setHistory(history.slice(0, -1))
            )}
          >
            ← Back
          </button>
        )}
      </div>
      {scope === "set" ? (
        <div style={{ flex: 1, minHeight: 0 }}>
          {merged.isError ? (
            <div className="error">{String(merged.error.message)}</div>
          ) : (
            <DataTable
              label="Merged references"
              rows={merged.data?.rows ?? []}
              rowKey={(r) => `${r.class_name}.${r.field}`}
              empty={
                <div className="empty">
                  {merged.isLoading ? <div className="spinner" /> : "No references"}
                </div>
              }
              columns={[
                {
                  id: "c",
                  header: dir === "incoming" ? "Referencing class" : "Referenced class",
                  flex: true,
                  render: (r) => <span className="mono">{r.class_name}</span>,
                },
                {
                  id: "f",
                  header: "Field",
                  width: 200,
                  render: (r) => <span className="mono">{r.field}</span>,
                },
                {
                  id: "o",
                  header: "Objects",
                  width: 100,
                  align: "right",
                  render: (r) => formatCount(r.objects),
                },
                {
                  id: "r",
                  header: "References",
                  width: 100,
                  align: "right",
                  render: (r) => formatCount(r.refs),
                },
              ]}
            />
          )}
        </div>
      ) : obj === null ? (
        <div className="empty">
          Select an object (Objects, Biggest objects or the inspector) to see its references.
        </div>
      ) : (
        <div className="split-v">
          <div style={{ minHeight: 0 }}>
            {refs.isError ? (
              <div className="error">{String(refs.error.message)}</div>
            ) : (
              <DataTable<RefRow>
                label={`${dir} references`}
                rows={refs.data?.rows ?? []}
                rowKey={(r, i) => `${r.object.id}-${i}`}
                onSelect={(r) => inspect({ type: "object", id: r.object.id })}
                onActivate={(r) => go(r.object.id)}
                totalRows={refs.data?.total_rows}
                empty={
                  <div className="empty">
                    {refs.isLoading ? <div className="spinner" /> : "No references"}
                  </div>
                }
                columns={[
                  {
                    id: "f",
                    header: dir === "incoming" ? "Via field" : "Field",
                    width: 180,
                    render: (r) => <span className="mono">{r.field}</span>,
                  },
                  ...objectColumns().map((c) => ({
                    ...c,
                    render: (r: RefRow) => c.render(r.object),
                    title: "title" in c && c.title ? (r: RefRow) => c.title!(r.object) : undefined,
                  })),
                ]}
              />
            )}
          </div>
          <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
            <div className="toolbar" style={{ padding: "6px 16px" }}>
              <span className="label">Paths to GC roots</span>
              <label className="check">
                <input
                  type="checkbox"
                  checked={excludeWeak}
                  onChange={(e) => setExcludeWeak(e.target.checked)}
                />
                Exclude weak/soft references
              </label>
            </div>
            <div style={{ flex: 1, overflow: "auto", padding: "8px 16px" }}>
              {paths.isLoading && <div className="spinner" />}
              {paths.isError && <div className="error">{String(paths.error.message)}</div>}
              {paths.data?.paths.length === 0 && (
                <div className="faint">No path to a GC root (object is unreachable).</div>
              )}
              {paths.data?.paths.map((p, i) => (
                <div key={i} style={{ marginBottom: 12 }}>
                  <div className="label">Path {i + 1}</div>
                  {p.map((st, k) => (
                    <div
                      key={k}
                      className="mono"
                      style={{ paddingLeft: k * 14, fontSize: 12, lineHeight: "20px" }}
                    >
                      {k > 0 && <span className="faint">↑ {st.field ?? ""} </span>}
                      <button className="link mono" onClick={() => go(st.object.id)}>
                        {st.object.class_name}
                      </button>{" "}
                      <span className="faint">{st.object.address}</span>
                      {st.root_kind && (
                        <span className="badge info" style={{ marginLeft: 6 }}>
                          GC root: {st.root_kind}
                        </span>
                      )}
                    </div>
                  ))}
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function InspectionsTab({ sessionId }: { sessionId: string }) {
  const q = useQuery({
    queryKey: sk(sessionId, "heap-inspections"),
    queryFn: () => client.heapInspections(sessionId),
  });
  const preselect = useApp((s) => s.heapInspection);
  const [picked, setKind] = useState<Insight | null>(null);
  useEffect(() => setKind(null), [preselect]);
  const kind = picked ?? q.data?.inspections.find((i) => i.kind === preselect) ?? null;
  const detail = useQuery({
    queryKey: sk(sessionId, "heap-inspection", kind?.kind),
    queryFn: () => client.heapInspection(sessionId, kind!.kind, 500),
    enabled: !!kind,
  });
  const rows = (detail.data?.rows ?? []) as Record<string, unknown>[];
  const FIRST = ["label", "name", "class_name", "value", "class"];
  const cols = rows.length
    ? Object.keys(rows[0])
        .filter((k) => typeof rows[0][k] !== "object" || rows[0][k] === null)
        .sort((a, b) => {
          const ia = FIRST.indexOf(a);
          const ib = FIRST.indexOf(b);
          return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
        })
    : [];
  const fmtCell = (k: string, v: unknown) => {
    if (typeof v === "number" && /bytes|size|wasted|shallow|retained/i.test(k)) return formatBytes(v);
    if (typeof v === "number") return formatCount(v);
    return String(v ?? "");
  };
  return (
    <div className="split">
      <div style={{ overflow: "auto", padding: 16 }}>
        {q.isLoading && <div className="spinner" />}
        {q.isError && <div className="error">{String(q.error.message)}</div>}
        {q.data && (
          <InsightList
            insights={q.data.inspections}
            onPick={(i) => (setKind(i), inspect({ type: "insight", insight: i }))}
          />
        )}
      </div>
      <div style={{ minHeight: 0 }}>
        {!kind ? (
          <div className="empty">Pick an inspection to see the affected objects.</div>
        ) : detail.isLoading ? (
          <div className="empty">
            <div className="spinner" />
          </div>
        ) : detail.isError ? (
          <div className="error">{String(detail.error.message)}</div>
        ) : (
          <DataTable<Record<string, unknown>>
            label={kind.title}
            rows={rows}
            rowKey={(_r, i) => i}
            onActivate={(r) => {
              const id =
                (r.object as ObjectRow | undefined)?.id ?? (typeof r.id === "number" ? r.id : undefined);
              if (id !== undefined) {
                store.set({ heapObject: id });
                inspect({ type: "object", id });
              }
            }}
            empty={<div className="empty">No affected objects</div>}
            columns={cols.map((k, i) => ({
              id: k,
              header: k.replace(/_/g, " "),
              flex: i === 0,
              width: i === 0 ? 240 : 120,
              align: typeof rows[0][k] === "number" ? ("right" as const) : undefined,
              render: (r: Record<string, unknown>) => (
                <span className={i === 0 ? "mono" : undefined}>{fmtCell(k, r[k])}</span>
              ),
              title: (r: Record<string, unknown>) => String(r[k] ?? ""),
            }))}
          />
        )}
      </div>
    </div>
  );
}

export function HeapView() {
  const s = useCurrentSession()!;
  const set = useApp((st) => st.heapSet);
  const tab = useApp((st) => st.heapTab);
  const setTab = (t: Tab) => store.set({ heapTab: t });
  const summary = useQuery({
    queryKey: sk(s.id, "heap-summary"),
    queryFn: () => client.heapSummary(s.id),
    enabled: s.capabilities.includes("heap"),
    refetchInterval: (q) => (q.state.data && q.state.data.analysis_state !== "ready" ? 1500 : false),
  });
  if (!s.capabilities.includes("heap"))
    return (
      <div className="empty">
        <h2>No heap data</h2>
        <p>
          Open an HPROF heap dump{s.state === "live" ? " or take a heap dump from the live session" : ""}.
        </p>
      </div>
    );
  const sm = summary.data;
  const tabs: { id: Tab; label: string }[] = [
    { id: "classes", label: "Classes" },
    { id: "objects", label: "Objects" },
    { id: "biggest", label: "Biggest objects" },
    { id: "references", label: "References" },
    { id: "inspections", label: "Inspections" },
  ];
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Heap walker</h1>
        {sm && (
          <span className="muted">
            {formatCount(sm.objects)} objects · {formatCount(sm.classes)} classes ·{" "}
            {formatBytes(sm.total_shallow)} · {formatCount(sm.gc_roots)} GC roots
          </span>
        )}
        {sm && sm.analysis_state !== "ready" && (
          <span className="badge accent">computing {sm.analysis_state}…</span>
        )}
        {summary.isError && <span className="error">{String(summary.error.message)}</span>}
      </div>
      <div className="toolbar" style={{ borderBottom: 0, paddingBottom: 0 }}>
        <Trail />
        <span className="grow" />
        {set && <SetRetained key={set.id} sessionId={s.id} set={set} />}
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
      <div className="view-body">
        {tab === "classes" && <ClassesTab sessionId={s.id} set={set} />}
        {tab === "objects" && <ObjectsTab sessionId={s.id} set={set} />}
        {tab === "biggest" && <BiggestTab sessionId={s.id} />}
        {tab === "references" && <ReferencesTab sessionId={s.id} set={set} />}
        {tab === "inspections" && <InspectionsTab sessionId={s.id} />}
      </div>
    </div>
  );
}
