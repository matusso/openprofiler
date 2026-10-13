import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import type { Summary, TreeKind } from "../api/types";
import { sk } from "../api/hooks";
import { formatNanos, formatRelTime } from "../lib/format";
import { client, store, useApp } from "../state";
import { cssVar } from "./TimeChart";

const BUCKETS = 400;
const KINDS: TreeKind[] = ["cpu", "wall", "alloc", "monitor", "wait", "io"];

/** Global time-range scrubber: an activity histogram that can be brushed to filter every view. */
export function TimeScrubber({ summary }: { summary: Summary }) {
  const id = summary.session.id;
  const range = useApp((s) => s.range);
  const theme = useApp((s) => s.theme);
  const kind = KINDS.find((k) => (summary.sample_counts[k] ?? 0) > 0);
  const domain: [number, number] = [summary.start, Math.max(summary.end, summary.start + 1)];
  const hist = useQuery({
    queryKey: sk(id, "scrub-hist", kind, summary.end),
    queryFn: () =>
      client.histogram(id, { kind: kind!, buckets: BUCKETS, filter: { range: [domain[0], domain[1] + 1] } }),
    enabled: !!kind,
  });
  const heap = useQuery({
    queryKey: sk(id, "scrub-heap", summary.end),
    queryFn: () => client.telemetry(id, { keys: ["heap.used"], points: BUCKETS }),
    enabled: !kind && summary.session.capabilities.includes("telemetry"),
  });
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [drag, setDrag] = useState<{ a: number; b: number } | null>(null);

  const toX = (t: number, w: number) => ((t - domain[0]) / (domain[1] - domain[0])) * w;
  const toT = (x: number, w: number) => domain[0] + (x / w) * (domain[1] - domain[0]);

  useEffect(() => {
    const c = canvasRef.current;
    if (!c) return;
    const w = c.clientWidth;
    const h = c.clientHeight;
    const dpr = window.devicePixelRatio || 1;
    c.width = w * dpr;
    c.height = h * dpr;
    const ctx = c.getContext("2d")!;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    const accent = cssVar("var(--accent)");
    const muted = cssVar("var(--fg-faint)");
    let values: number[] = [];
    if (hist.data) values = hist.data.values;
    else if (heap.data?.series[0]) values = heap.data.series[0].v;
    const max = Math.max(1, ...values);
    const bw = w / Math.max(1, values.length);
    ctx.fillStyle = muted;
    ctx.globalAlpha = 0.55;
    values.forEach((v, i) => {
      const bh = (v / max) * (h - 8);
      ctx.fillRect(i * bw, h - bh, Math.max(1, bw - 0.5), bh);
    });
    ctx.globalAlpha = 1;
    const sel = drag ? [Math.min(drag.a, drag.b), Math.max(drag.a, drag.b)] : range;
    if (sel) {
      const x0 = toX(sel[0], w);
      const x1 = toX(sel[1], w);
      ctx.fillStyle = "rgba(0,0,0,0.35)";
      ctx.fillRect(0, 0, x0, h);
      ctx.fillRect(x1, 0, w - x1, h);
      ctx.strokeStyle = accent;
      ctx.lineWidth = 1.5;
      ctx.strokeRect(x0, 0.75, Math.max(1, x1 - x0), h - 1.5);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hist.data, heap.data, range, drag, theme, summary.start, summary.end]);

  const pos = (e: React.MouseEvent) => {
    const r = canvasRef.current!.getBoundingClientRect();
    return toT(Math.max(0, Math.min(r.width, e.clientX - r.left)), r.width);
  };

  return (
    <div
      className="scrubber"
      onMouseDown={(e) => {
        const t = pos(e);
        setDrag({ a: t, b: t });
      }}
      onMouseMove={(e) => drag && setDrag({ ...drag, b: pos(e) })}
      onMouseUp={() => {
        if (drag) {
          const a = Math.min(drag.a, drag.b);
          const b = Math.max(drag.a, drag.b);
          const w = canvasRef.current?.getBoundingClientRect().width ?? 1;
          const minSpan = ((domain[1] - domain[0]) / w) * 3;
          store.set({ range: b - a > minSpan ? [Math.round(a), Math.round(b)] : null });
          setDrag(null);
        }
      }}
      onMouseLeave={() => setDrag(null)}
      title="Drag to filter all views to a time range"
    >
      <canvas ref={canvasRef} role="img" aria-label="Session activity timeline" />
      <div className="scrubber-info">
        {range ? (
          <>
            <span>
              {formatRelTime(range[0], summary.start)} – {formatRelTime(range[1], summary.start)} (
              {formatNanos(range[1] - range[0])})
            </span>
            <button
              className="btn"
              onMouseDown={(e) => e.stopPropagation()}
              onClick={() => store.set({ range: null })}
            >
              Clear range
            </button>
          </>
        ) : (
          <span>
            {kind ? `${kind} activity` : "heap"} · {formatNanos(summary.end - summary.start)}
          </span>
        )}
      </div>
    </div>
  );
}
