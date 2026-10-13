import { useEffect, useState, type ReactNode } from "react";
import type { Focus, TreeKind } from "../../api/types";
import { useCurrentSession } from "../../api/hooks";
import { MethodLabel } from "../../components/MethodLabel";
import { store, useApp, type ViewId } from "../../state";
import { CallGraphPanel, CallTreePanel, FlamePanel, HotspotsPanel } from "./panels";
import {
  AggregationToggle,
  ExportMenu,
  KIND_LABELS,
  SearchBox,
  StateSelector,
  ThreadSelector,
  TreeOptionsToggles,
  useTreeParams,
} from "./shared";

export type TreeTab = "calltree" | "hotspots" | "flame" | "callgraph";

const TABS: { id: TreeTab; label: string }[] = [
  { id: "calltree", label: "Call tree" },
  { id: "hotspots", label: "Hot spots" },
  { id: "flame", label: "Flame graph" },
  { id: "callgraph", label: "Call graph" },
];

/**
 * The shared CPU/allocation/lock analysis surface: tabs for call tree, hot spots, flame and call
 * graph over one sample kind, plus any extra tabs the caller provides.
 */
export function ProfileTrees({
  view,
  title,
  kinds,
  withStates = true,
  extraTabs = [],
  classes,
  toolbarExtra,
}: {
  view: ViewId;
  title: string;
  kinds: TreeKind[];
  withStates?: boolean;
  extraTabs?: { id: string; label: string; render: () => ReactNode }[];
  /** Allocated/thrown class filter applied to every tree request. */
  classes?: string[];
  toolbarExtra?: ReactNode;
}) {
  const session = useCurrentSession();
  const caps = session?.capabilities ?? [];
  const available = kinds.filter((k) => caps.includes(k));
  const [kind, setKind] = useState<TreeKind>(available[0] ?? kinds[0]);
  const [tab, setTab] = useState<string>("calltree");
  const [search, setSearch] = useState("");
  const [findNonce, setFindNonce] = useState("");
  const [focus, setFocus] = useState<{ focus: Focus; label: string } | null>(null);
  const focusReq = useApp((s) => s.focus);
  const params = useTreeParams(kind, withStates, classes);

  useEffect(() => {
    if (!available.includes(kind) && available[0]) setKind(available[0]);
  }, [available, kind]);

  useEffect(() => {
    if (focusReq && focusReq.view === view) {
      if (kinds.includes(focusReq.kind) && caps.includes(focusReq.kind)) setKind(focusReq.kind);
      setFocus({
        focus: focusReq.mode === "callers" ? { callers: focusReq.key } : { callees: focusReq.key },
        label: focusReq.label,
      });
      setTab(focusReq.mode === "callers" ? "calltree" : "calltree");
      store.set({ focus: null });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusReq]);

  if (!session) return null;
  if (available.length === 0)
    return (
      <div className="empty">
        <h2>{title}</h2>
        <p>This session has no {kinds.map((k) => KIND_LABELS[k].toLowerCase()).join(" / ")} data.</p>
      </div>
    );

  const isTree = TABS.some((t) => t.id === tab);
  const extra = extraTabs.find((t) => t.id === tab);
  const treeReq = { ...params, view: "calltree", focus: focus?.focus };

  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">{title}</h1>
        {available.length > 1 && (
          <div className="seg" role="group" aria-label="Sample kind">
            {available.map((k) => (
              <button key={k} aria-pressed={k === kind} onClick={() => setKind(k)}>
                {KIND_LABELS[k]}
              </button>
            ))}
          </div>
        )}
        {focus && (
          <span className="badge accent" style={{ height: 24, gap: 6 }}>
            {"callers" in focus.focus ? "Callers of" : "Calls from"} <MethodLabel label={focus.label} />
            <button
              className="link"
              aria-label="Clear focus"
              onClick={() => setFocus(null)}
              style={{ color: "inherit", marginLeft: 4 }}
            >
              ✕
            </button>
          </span>
        )}
      </div>
      <div className="tabs" role="tablist">
        {[...TABS, ...extraTabs].map((t) => (
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
      {isTree && (
        <div className="toolbar">
          <ThreadSelector />
          {toolbarExtra}
          {withStates && <StateSelector />}
          <AggregationToggle />
          <TreeOptionsToggles />
          <span className="grow" />
          <SearchBox value={search} onChange={setSearch} onEnter={() => setFindNonce(String(Date.now()))} />
          <ExportMenu
            view={tab === "hotspots" ? "hotspots" : tab === "flame" ? "flame" : "calltree"}
            request={treeReq}
            formats={tab === "flame" ? ["svg", "collapsed", "json"] : ["csv", "json", "html"]}
          />
        </div>
      )}
      <div className="view-body">
        {tab === "calltree" && (
          <CallTreePanel
            sessionId={session.id}
            params={params}
            view={focus && "callers" in focus.focus ? "backtraces" : "calltree"}
            focus={focus?.focus}
            search={search}
            findToken={findNonce}
          />
        )}
        {tab === "hotspots" && <HotspotsPanel sessionId={session.id} params={params} search={search} />}
        {tab === "flame" && (
          <FlamePanel sessionId={session.id} params={params} search={search} focus={focus?.focus} />
        )}
        {tab === "callgraph" && <CallGraphPanel sessionId={session.id} params={params} />}
        {extra?.render()}
      </div>
    </div>
  );
}
