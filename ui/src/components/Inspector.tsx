import { useQuery } from "@tanstack/react-query";
import type { Unit } from "../api/types";
import { sk } from "../api/hooks";
import { formatBytes, formatClock, formatCount, formatPercent, formatValue } from "../lib/format";
import { goToSource, sourceTarget } from "../lib/source";
import { client, focusMethod, inspect, store, useApp, type InspectorItem } from "../state";
import { VIEW_OF_KIND } from "../views/profile/shared";
import { IconClose } from "./icons";
import { KIND_COLORS } from "./MethodLabel";

function NodeInspector({ item }: { item: Extract<InspectorItem, { type: "node" }> }) {
  const n = item.node;
  const unit = item.unit as Unit;
  const view = VIEW_OF_KIND[item.kind];
  const isRoot = n.parent === null;
  const aggregation = useApp((s) => s.aggregation);
  const target = sourceTarget(n.label, aggregation, n.class, n.method);
  return (
    <>
      <div className="mono" style={{ wordBreak: "break-all", fontWeight: 600, marginBottom: 10 }}>
        {n.label}
        {n.signature && <span className="faint">{n.signature}</span>}
      </div>
      <dl className="kv">
        {n.class && (
          <>
            <dt>Class</dt>
            <dd className="mono">{n.class}</dd>
          </>
        )}
        <dt>Frame type</dt>
        <dd>
          <span className="kind-chip" style={{ background: KIND_COLORS[n.kind], display: "inline-block" }} />{" "}
          {n.kind}
        </dd>
        <dt>Total</dt>
        <dd>
          <b>{formatValue(n.total, unit)}</b> · {formatPercent(n.total, item.total)}
        </dd>
        <dt>Self</dt>
        <dd>
          {formatValue(n.self, unit)} · {formatPercent(n.self, item.total)}
        </dd>
        <dt>Samples</dt>
        <dd>
          {formatCount(n.total_count)} total · {formatCount(n.self_count)} self
        </dd>
        <dt>Depth</dt>
        <dd>{n.depth}</dd>
      </dl>
      {!isRoot && (
        <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 14 }}>
          <button className="btn primary" disabled={!target} onClick={() => void goToSource(target)}>
            Go to source
          </button>
          <button className="btn" onClick={() => focusMethod(view, item.kind, n.key, n.label, "callees")}>
            Cumulated outgoing calls
          </button>
          <button className="btn" onClick={() => focusMethod(view, item.kind, n.key, n.label, "callers")}>
            Show back traces (callers)
          </button>
        </div>
      )}
    </>
  );
}

function ObjectInspector({ id }: { id: number }) {
  const sid = useApp((s) => s.sessionId);
  const q = useQuery({
    queryKey: sk(sid, "heap-object", id),
    queryFn: () => client.heapObject(sid!, id),
    enabled: !!sid,
  });
  if (q.isLoading) return <div className="spinner" />;
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  const d = q.data!;
  const o = d.object;
  return (
    <>
      <div className="mono" style={{ wordBreak: "break-all", fontWeight: 600 }}>
        {o.class_name}
      </div>
      <div className="faint mono" style={{ marginBottom: 10 }}>
        {o.address}
      </div>
      <dl className="kv">
        <dt>Shallow</dt>
        <dd>{formatBytes(o.shallow)}</dd>
        {o.retained != null && (
          <>
            <dt>Retained</dt>
            <dd>
              <b>{formatBytes(o.retained)}</b>
            </dd>
          </>
        )}
        {d.array_length != null && (
          <>
            <dt>Length</dt>
            <dd>{formatCount(d.array_length)}</dd>
          </>
        )}
        {d.gc_root_kinds.length > 0 && (
          <>
            <dt>GC root</dt>
            <dd>{d.gc_root_kinds.join(", ")}</dd>
          </>
        )}
        {d.dominator != null && (
          <>
            <dt>Dominator</dt>
            <dd>
              <button className="link mono" onClick={() => inspect({ type: "object", id: d.dominator! })}>
                #{d.dominator}
              </button>
            </dd>
          </>
        )}
        {o.preview && (
          <>
            <dt>Value</dt>
            <dd className="mono" style={{ whiteSpace: "pre-wrap" }}>
              {o.preview}
            </dd>
          </>
        )}
      </dl>
      <div style={{ display: "flex", gap: 6, marginTop: 12, flexWrap: "wrap" }}>
        <button
          className="btn"
          onClick={() => store.set({ view: "heap", heapObject: id, heapTab: "references" })}
        >
          References & path to roots
        </button>
      </div>
      {d.fields.length > 0 && (
        <>
          <div className="section-title">Fields</div>
          <div className="field-list">
            {d.fields.map((f, i) => (
              <div className="f" key={i}>
                <span className="mono" title={f.type}>
                  {f.name} <span className="faint">{f.type.split(".").pop()}</span>
                </span>
                <span className="mono" style={{ wordBreak: "break-all" }}>
                  {f.ref !== undefined && f.ref !== null ? (
                    <button className="link mono" onClick={() => inspect({ type: "object", id: f.ref! })}>
                      {f.value}
                    </button>
                  ) : (
                    f.value
                  )}
                </span>
              </div>
            ))}
          </div>
        </>
      )}
      {d.elements && d.elements.length > 0 && (
        <>
          <div className="section-title">Elements</div>
          <div className="field-list">
            {d.elements.map((el) => (
              <div className="f" key={el.index}>
                <span className="mono faint">[{el.index}]</span>
                <span className="mono">
                  {el.ref !== undefined && el.ref !== null ? (
                    <button className="link mono" onClick={() => inspect({ type: "object", id: el.ref! })}>
                      {el.value}
                    </button>
                  ) : (
                    el.value
                  )}
                </span>
              </div>
            ))}
          </div>
        </>
      )}
    </>
  );
}

function Body({ item }: { item: InspectorItem }) {
  switch (item.type) {
    case "node":
      return <NodeInspector item={item} />;
    case "object":
      return <ObjectInspector id={item.id} />;
    case "thread": {
      const t = item.thread;
      return (
        <dl className="kv">
          <dt>Name</dt>
          <dd>{t.name}</dd>
          <dt>Java id</dt>
          <dd>{t.java_id}</dd>
          {t.group && (
            <>
              <dt>Group</dt>
              <dd>{t.group}</dd>
            </>
          )}
          <dt>Virtual</dt>
          <dd>{t.is_virtual ? "yes" : "no"}</dd>
          <dt>Daemon</dt>
          <dd>{t.is_daemon ? "yes" : "no"}</dd>
          {t.start !== undefined && t.start !== null && (
            <>
              <dt>Started</dt>
              <dd>{formatClock(t.start)}</dd>
            </>
          )}
          {t.end !== undefined && t.end !== null && (
            <>
              <dt>Ended</dt>
              <dd>{formatClock(t.end)}</dd>
            </>
          )}
          {t.cpu !== undefined && t.cpu !== null && (
            <>
              <dt>CPU</dt>
              <dd>{formatValue(t.cpu, "nanos")}</dd>
            </>
          )}
        </dl>
      );
    }
    case "insight": {
      const i = item.insight;
      return (
        <>
          <span className={`badge ${i.severity}`}>{i.severity}</span>
          <h3 style={{ margin: "8px 0" }}>{i.title}</h3>
          <p className="muted">{i.summary}</p>
          <dl className="kv">
            {i.wasted_bytes != null && (
              <>
                <dt>Wasted</dt>
                <dd>{formatBytes(i.wasted_bytes)}</dd>
              </>
            )}
            {i.count != null && (
              <>
                <dt>Count</dt>
                <dd>{formatCount(i.count)}</dd>
              </>
            )}
            <dt>Kind</dt>
            <dd className="mono">{i.kind}</dd>
          </dl>
        </>
      );
    }
  }
}

const TITLES: Record<InspectorItem["type"], string> = {
  node: "Frame",
  object: "Object",
  thread: "Thread",
  insight: "Insight",
};

export function Inspector() {
  const item = useApp((s) => s.inspector);
  const open = useApp((s) => s.inspectorOpen);
  // The decompiler has nothing to inspect and needs the width for code.
  const code = useApp((s) => s.view === "code");
  if (!open || code) return null;
  return (
    <aside className="inspector" aria-label="Inspector">
      <div className="inspector-head">
        {item ? TITLES[item.type] : "Inspector"}
        <span className="grow" />
        <button
          className="btn ghost icon"
          aria-label="Close inspector"
          onClick={() => store.set({ inspectorOpen: false })}
        >
          <IconClose />
        </button>
      </div>
      <div className="inspector-body">
        {item ? (
          <Body item={item} />
        ) : (
          <p className="faint">Select a frame, object, thread or insight to see its details here.</p>
        )}
      </div>
    </aside>
  );
}
