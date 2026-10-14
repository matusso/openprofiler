import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import type { ThreadDto, ThreadDump, ThreadSpan } from "../api/types";
import { sk, useCurrentSession, useSummary, useThreads } from "../api/hooks";
import { cssVar } from "../components/TimeChart";
import { formatClock, formatNanos, formatRelTime } from "../lib/format";
import { client, inspect, queryClient, store, useApp } from "../state";
import { STATES } from "./profile/shared";
import { diffDumps, frameKey } from "../lib/dumps";

const ROW = 20;
const NAME_W = 220;

function Timeline({ sessionId }: { sessionId: string }) {
  const range = useApp((s) => s.range);
  const theme = useApp((s) => s.theme);
  const palette = useApp((s) => s.palette);
  const summary = useSummary(sessionId);
  const threads = useThreads(sessionId);
  const spans = useQuery({
    queryKey: sk(sessionId, "thread-states", range),
    queryFn: () => client.threadStates(sessionId, { range: range ?? undefined, max_spans: 400_000 }),
  });
  const [filter, setFilter] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [size, setSize] = useState({ w: 800, h: 400 });
  const [hover, setHover] = useState<{ x: number; y: number; text: string } | null>(null);

  const list: ThreadDto[] = useMemo(
    () => (threads.data?.threads ?? []).filter((t) => t.name.toLowerCase().includes(filter.toLowerCase())),
    [threads.data, filter],
  );
  const byThread = useMemo(() => {
    const m = new Map<number, ThreadSpan[]>();
    for (const s of spans.data?.spans ?? []) {
      let a = m.get(s.thread);
      if (!a) m.set(s.thread, (a = []));
      a.push(s);
    }
    return m;
  }, [spans.data]);

  const t0 = range?.[0] ?? summary.data?.start ?? 0;
  const t1 = range?.[1] ?? summary.data?.end ?? 1;

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setSize({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const c = canvasRef.current;
    if (!c) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = size.w * dpr;
    c.height = size.h * dpr;
    c.style.width = `${size.w}px`;
    c.style.height = `${size.h}px`;
    const ctx = c.getContext("2d")!;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, size.w, size.h);
    const colors = Object.fromEntries(STATES.map((s) => [s, cssVar(`var(--st-${s})`)]));
    const fg = cssVar("var(--fg)");
    const faint = cssVar("var(--fg-faint)");
    const border = cssVar("var(--border)");
    const chartW = size.w - NAME_W;
    const first = Math.floor(scrollTop / ROW);
    const last = Math.min(list.length, first + Math.ceil(size.h / ROW) + 1);
    ctx.font = "12px Inter, system-ui, sans-serif";
    ctx.textBaseline = "middle";
    for (let i = first; i < last; i++) {
      const t = list[i];
      const y = i * ROW - scrollTop;
      ctx.fillStyle = t.end ? faint : fg;
      const name = t.name.length > 30 ? t.name.slice(0, 29) + "…" : t.name;
      ctx.fillText(name, 8, y + ROW / 2);
      for (const s of byThread.get(t.index) ?? []) {
        if (s.end < t0 || s.start > t1) continue;
        const x0 = NAME_W + ((Math.max(s.start, t0) - t0) / (t1 - t0)) * chartW;
        const x1 = NAME_W + ((Math.min(s.end, t1) - t0) / (t1 - t0)) * chartW;
        ctx.fillStyle = colors[s.state] ?? colors.unknown;
        ctx.fillRect(x0, y + 3, Math.max(0.75, x1 - x0), ROW - 6);
      }
      ctx.fillStyle = border;
      ctx.fillRect(0, y + ROW - 0.5, size.w, 0.5);
    }
    ctx.fillStyle = border;
    ctx.fillRect(NAME_W - 1, 0, 1, size.h);
  }, [list, byThread, scrollTop, size, t0, t1, theme, palette]);

  const onMove = (e: React.MouseEvent) => {
    const r = scrollRef.current!.getBoundingClientRect();
    const x = e.clientX - r.left;
    const i = Math.floor((e.clientY - r.top + scrollTop) / ROW);
    const t = list[i];
    if (!t || x < NAME_W) return setHover(t ? { x: e.clientX, y: e.clientY, text: t.name } : null);
    const time = t0 + ((x - NAME_W) / (size.w - NAME_W)) * (t1 - t0);
    const s = byThread.get(t.index)?.find((sp) => sp.start <= time && sp.end >= time);
    setHover({
      x: e.clientX,
      y: e.clientY,
      text: `${t.name}\n${s ? `${s.state} · ${formatNanos(s.end - s.start)}` : "no data"} @ ${formatRelTime(time, summary.data?.start ?? 0)}`,
    });
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <input
          className="input"
          placeholder="Filter threads"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-view-search
          aria-label="Filter threads"
        />
        <span className="legend">
          {STATES.map((s) => (
            <span key={s}>
              <i style={{ background: `var(--st-${s})` }} />
              {s.replace("_", " ")}
            </span>
          ))}
        </span>
        <span className="grow" />
        {spans.data && "truncated" in spans.data && (spans.data as { truncated?: boolean }).truncated && (
          <span className="badge warning">truncated — narrow the time range</span>
        )}
        <span className="faint">{list.length} threads</span>
      </div>
      <div
        ref={scrollRef}
        style={{ flex: 1, overflow: "auto", position: "relative", minHeight: 0 }}
        onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
        onMouseMove={onMove}
        onMouseLeave={() => setHover(null)}
        onClick={(e) => {
          const r = scrollRef.current!.getBoundingClientRect();
          const t = list[Math.floor((e.clientY - r.top + scrollTop) / ROW)];
          if (t) inspect({ type: "thread", thread: t });
        }}
        onDoubleClick={(e) => {
          const r = scrollRef.current!.getBoundingClientRect();
          const t = list[Math.floor((e.clientY - r.top + scrollTop) / ROW)];
          if (t) store.set({ threads: [t.index] });
        }}
        role="img"
        aria-label="Thread state timeline"
      >
        <div style={{ height: list.length * ROW }} />
        <canvas
          ref={canvasRef}
          style={{ position: "sticky", top: 0, left: 0, marginTop: -list.length * ROW }}
        />
      </div>
      {spans.isLoading && <div className="spinner" style={{ position: "absolute", top: 120, left: "50%" }} />}
      {hover && (
        <div className="tooltip" style={{ left: hover.x + 12, top: hover.y + 12, whiteSpace: "pre" }}>
          {hover.text}
        </div>
      )}
    </div>
  );
}

function DumpText({ dump, compare }: { dump: ThreadDump; compare?: ThreadDump }) {
  if (compare) {
    const d = diffDumps(compare, dump).filter((x) => x.status !== "same");
    if (!d.length) return <div className="empty">The two dumps are identical.</div>;
    return (
      <div className="dump">
        {d.map(({ thread, status, prev }) => (
          <div key={frameKey(thread) + status}>
            <div
              className={`thread-head ${status === "added" ? "added" : status === "removed" ? "removed" : ""}`}
            >
              "{thread.name}"{" "}
              {prev && prev.state !== thread.state ? `${prev.state} → ${thread.state}` : thread.state} [
              {status}]
            </div>
            {status === "changed" && prev
              ? (() => {
                  const ps = new Set(prev.frames);
                  const ns = new Set(thread.frames);
                  return (
                    <>
                      {prev.frames
                        .filter((f) => !ns.has(f))
                        .map((f, i) => (
                          <div key={"r" + i} className="removed">
                            {" "}
                            at {f}
                          </div>
                        ))}
                      {thread.frames.map((f, i) => (
                        <div key={i} className={ps.has(f) ? undefined : "added"}>
                          {" "}
                          at {f}
                        </div>
                      ))}
                    </>
                  );
                })()
              : thread.frames.map((f, i) => <div key={i}> at {f}</div>)}
          </div>
        ))}
      </div>
    );
  }
  return (
    <div className="dump">
      {dump.threads.map((t) => (
        <div key={frameKey(t)}>
          <div className="thread-head">
            "{t.name}" #{t.java_id} {t.is_virtual ? "virtual " : ""}
            {t.state}
          </div>
          {t.frames.map((f, i) => (
            <div key={i}> at {f}</div>
          ))}
          {t.waiting_on && <div className="faint"> - waiting on {t.waiting_on}</div>}
          {t.locked.map((l, i) => (
            <div key={"l" + i} className="faint">
              {" "}
              - locked {l}
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

function Dumps({ sessionId, live }: { sessionId: string; live: boolean }) {
  const q = useQuery({
    queryKey: sk(sessionId, "thread-dumps"),
    queryFn: () => client.threadDumps(sessionId),
  });
  const [sel, setSel] = useState(0);
  const [cmp, setCmp] = useState<number | null>(null);
  const capture = useMutation({
    mutationFn: () => client.captureThreadDump(sessionId),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: sk(sessionId, "thread-dumps") }),
  });
  const dumps = q.data ?? [];
  const cur = dumps[sel];
  return (
    <div style={{ display: "grid", gridTemplateColumns: "260px 1fr", height: "100%" }}>
      <div style={{ borderRight: "1px solid var(--border)", overflow: "auto" }}>
        {live && (
          <div style={{ padding: 8 }}>
            <button className="btn primary" onClick={() => capture.mutate()} disabled={capture.isPending}>
              Capture thread dump
            </button>
          </div>
        )}
        {q.isLoading && <div className="spinner" style={{ margin: 12 }} />}
        {!q.isLoading && dumps.length === 0 && (
          <div className="faint" style={{ padding: 12 }}>
            No thread dumps.
          </div>
        )}
        <div className="list" role="listbox" aria-label="Thread dumps">
          {dumps.map((d, i) => (
            <div
              key={i}
              className="list-item"
              role="option"
              aria-selected={i === sel}
              onClick={() => setSel(i)}
            >
              <span>
                <div>{d.label || `Dump ${i + 1}`}</div>
                <div className="faint" style={{ fontSize: 11.5 }}>
                  {formatClock(d.time)} · {d.threads.length} threads
                </div>
              </span>
              <span className="grow" />
              {i !== sel && (
                <label className="check" title="Diff against this dump" onClick={(e) => e.stopPropagation()}>
                  <input
                    type="checkbox"
                    checked={cmp === i}
                    onChange={(e) => setCmp(e.target.checked ? i : null)}
                  />
                  diff
                </label>
              )}
            </div>
          ))}
        </div>
      </div>
      <div style={{ overflow: "auto", minWidth: 0 }}>
        {cur ? <DumpText dump={cur} compare={cmp !== null && cmp !== sel ? dumps[cmp] : undefined} /> : null}
      </div>
    </div>
  );
}

export function ThreadsView() {
  const s = useCurrentSession()!;
  const hasThreads = s.capabilities.includes("threads");
  const [tab, setTab] = useState<"timeline" | "dumps">(hasThreads ? "timeline" : "dumps");
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Threads</h1>
      </div>
      <div className="tabs" role="tablist">
        {hasThreads && (
          <button
            role="tab"
            className="tab"
            aria-selected={tab === "timeline"}
            onClick={() => setTab("timeline")}
          >
            History
          </button>
        )}
        <button role="tab" className="tab" aria-selected={tab === "dumps"} onClick={() => setTab("dumps")}>
          Thread dumps
        </button>
      </div>
      <div className="view-body">
        {tab === "timeline" ? (
          <Timeline sessionId={s.id} />
        ) : (
          <Dumps sessionId={s.id} live={s.state === "live"} />
        )}
      </div>
    </div>
  );
}
