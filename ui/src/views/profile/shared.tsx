import { useEffect, useRef, useState } from "react";
import type { Aggregation, Filter, ThreadStateName, TreeKind, Unit } from "../../api/types";
import { useThreads } from "../../api/hooks";
import { IconDownload, IconSearch } from "../../components/icons";
import { client, store, useApp, type ViewId } from "../../state";

export const KIND_LABELS: Record<TreeKind, string> = {
  cpu: "CPU",
  wall: "Wall clock",
  alloc: "Allocations",
  monitor: "Monitor blocking",
  wait: "Waiting / parking",
  io: "I/O",
  exception: "Exceptions",
};

export const VIEW_OF_KIND: Record<TreeKind, ViewId> = {
  cpu: "cpu",
  wall: "cpu",
  alloc: "memory",
  monitor: "locks",
  wait: "locks",
  io: "cpu",
  exception: "cpu",
};

export const KIND_UNIT: Record<TreeKind, Unit> = {
  cpu: "nanos",
  wall: "nanos",
  alloc: "bytes",
  monitor: "nanos",
  wait: "nanos",
  io: "nanos",
  exception: "count",
};

export const STATES: ThreadStateName[] = ["runnable", "waiting", "blocked", "net_io", "sleeping", "unknown"];

/** Request fragment shared by tree/hotspot/flame/callgraph requests. */
export interface TreeParams {
  kind: TreeKind;
  filter: Filter;
  aggregation: Aggregation;
  collapse_recursion: boolean;
  compact_prefixes?: string[];
}

export function useTreeParams(kind: TreeKind, withStates: boolean, classes?: string[]): TreeParams {
  const range = useApp((s) => s.range);
  const threads = useApp((s) => s.threads);
  const states = useApp((s) => s.states);
  const aggregation = useApp((s) => s.aggregation);
  const collapse = useApp((s) => s.collapseRecursion);
  const compact = useApp((s) => s.compactJava);
  const filter: Filter = {};
  if (range) filter.range = range;
  if (threads?.length) filter.threads = threads;
  if (withStates && states?.length) filter.states = states;
  if (classes?.length) filter.classes = classes;
  return {
    kind,
    filter,
    aggregation,
    collapse_recursion: collapse,
    compact_prefixes: compact ? ["java.", "javax.", "jdk.", "sun.", "com.sun."] : undefined,
  };
}

function Dropdown({
  label,
  children,
  active,
}: {
  label: string;
  children: React.ReactNode;
  active?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    window.addEventListener("mousedown", h);
    return () => window.removeEventListener("mousedown", h);
  }, [open]);
  return (
    <div ref={ref} style={{ position: "relative" }}>
      <button className={`btn${active ? " on" : ""}`} onClick={() => setOpen(!open)} aria-expanded={open}>
        {label} ▾
      </button>
      {open && <div className="menu">{children}</div>}
    </div>
  );
}

export function ThreadSelector() {
  const sid = useApp((s) => s.sessionId);
  const sel = useApp((s) => s.threads);
  const { data } = useThreads(sid);
  const [q, setQ] = useState("");
  const threads = (data?.threads ?? []).filter((t) => t.name.toLowerCase().includes(q.toLowerCase()));
  const toggle = (i: number) => {
    const cur = new Set(sel ?? []);
    if (cur.has(i)) cur.delete(i);
    else cur.add(i);
    store.set({ threads: cur.size ? [...cur] : null });
  };
  return (
    <Dropdown
      label={sel?.length ? `${sel.length} thread${sel.length > 1 ? "s" : ""}` : "All threads"}
      active={!!sel?.length}
    >
      <input
        className="input"
        style={{ width: "100%", marginBottom: 4 }}
        placeholder="Filter threads"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        aria-label="Filter threads"
      />
      <button className="menu-item" onClick={() => store.set({ threads: null })}>
        All threads
      </button>
      {threads.map((t) => (
        <label key={t.index} className="menu-item check">
          <input type="checkbox" checked={!!sel?.includes(t.index)} onChange={() => toggle(t.index)} />
          <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>{t.name}</span>
          {t.is_virtual && <span className="badge">virtual</span>}
        </label>
      ))}
    </Dropdown>
  );
}

export function StateSelector() {
  const sel = useApp((s) => s.states);
  const toggle = (st: ThreadStateName) => {
    const cur = new Set(sel ?? []);
    if (cur.has(st)) cur.delete(st);
    else cur.add(st);
    store.set({ states: cur.size ? [...cur] : null });
  };
  return (
    <Dropdown label={sel?.length ? sel.join(", ") : "All states"} active={!!sel?.length}>
      <button className="menu-item" onClick={() => store.set({ states: null })}>
        All states
      </button>
      {STATES.map((st) => (
        <label key={st} className="menu-item check">
          <input type="checkbox" checked={!!sel?.includes(st)} onChange={() => toggle(st)} />
          <i className="kind-chip" style={{ background: `var(--st-${st})` }} />
          {st.replace("_", " ")}
        </label>
      ))}
    </Dropdown>
  );
}

export function AggregationToggle() {
  const agg = useApp((s) => s.aggregation);
  return (
    <div className="seg" role="group" aria-label="Aggregation level">
      {(["method", "class", "package"] as Aggregation[]).map((a) => (
        <button key={a} aria-pressed={agg === a} onClick={() => store.set({ aggregation: a })}>
          {a[0].toUpperCase() + a.slice(1)}
        </button>
      ))}
    </div>
  );
}

export function TreeOptionsToggles() {
  const collapse = useApp((s) => s.collapseRecursion);
  const compact = useApp((s) => s.compactJava);
  return (
    <>
      <label className="check" title="Fold recursive calls into their outermost occurrence">
        <input
          type="checkbox"
          checked={collapse}
          onChange={(e) => store.set({ collapseRecursion: e.target.checked })}
        />
        Collapse recursion
      </label>
      <label className="check" title="Reduce runs of JDK frames to their entry point">
        <input
          type="checkbox"
          checked={compact}
          onChange={(e) => store.set({ compactJava: e.target.checked })}
        />
        Compact JDK frames
      </label>
    </>
  );
}

export function SearchBox({
  value,
  onChange,
  onEnter,
  placeholder = "Find (/)",
}: {
  value: string;
  onChange: (v: string) => void;
  onEnter?: () => void;
  placeholder?: string;
}) {
  return (
    <span style={{ position: "relative", display: "inline-flex", alignItems: "center" }}>
      <IconSearch style={{ position: "absolute", left: 8, width: 13, color: "var(--fg-faint)" }} />
      <input
        data-view-search
        className="input"
        style={{ paddingLeft: 26, width: 220 }}
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") onEnter?.();
          if (e.key === "Escape") {
            onChange("");
            (e.target as HTMLInputElement).blur();
          }
        }}
        aria-label="Search"
      />
    </span>
  );
}

export async function downloadExport(sessionId: string, view: string, format: string, request: unknown) {
  const blob = await client.export(sessionId, { view, format, request });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `ember-${view}.${format === "collapsed" ? "txt" : format}`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function ExportMenu({
  view,
  request,
  formats,
}: {
  view: string;
  request: unknown;
  formats: string[];
}) {
  const sid = useApp((s) => s.sessionId);
  const [err, setErr] = useState<string | null>(null);
  return (
    <Dropdown label="Export">
      {formats.map((f) => (
        <button
          key={f}
          className="menu-item"
          onClick={() => sid && downloadExport(sid, view, f, request).catch((e: Error) => setErr(e.message))}
        >
          <IconDownload width={14} /> {f.toUpperCase()}
        </button>
      ))}
      {err && <div className="error">{err}</div>}
    </Dropdown>
  );
}
