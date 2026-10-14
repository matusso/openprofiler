import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import type { ObjectRow } from "../api/types";
import { sk } from "../api/hooks";
import { frameColor } from "../lib/flame";
import { formatBytes, formatPercent } from "../lib/format";
import { client, useApp } from "../state";
import { cssVar } from "./TimeChart";

const MAX_LEVELS = 5;
const MIN_ANGLE = 0.012;

interface Seg {
  row: ObjectRow;
  a0: number;
  a1: number;
  level: number;
}

/** Dominator-tree sunburst. Children are fetched lazily via `/heap/biggest`. */
export function Sunburst({
  sessionId,
  onSelect,
}: {
  sessionId: string;
  onSelect?: (row: ObjectRow) => void;
}) {
  const qc = useQueryClient();
  const theme = useApp((s) => s.theme);
  const [center, setCenter] = useState<ObjectRow | null>(null);
  const [trail, setTrail] = useState<ObjectRow[]>([]);
  const [version, setVersion] = useState(0);
  const [hover, setHover] = useState<{ seg: Seg; x: number; y: number } | null>(null);
  const cache = useRef(new Map<string, ObjectRow[]>());
  const loading = useRef(new Set<string>());
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState(480);

  useEffect(() => {
    cache.current.clear();
    setCenter(null);
    setTrail([]);
  }, [sessionId]);

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() =>
      setSize(Math.max(200, Math.min(el.clientWidth, el.clientHeight) - 16)),
    );
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const keyOf = (obj: number | undefined) => (obj === undefined ? "root" : String(obj));

  const load = (obj: number | undefined) => {
    const k = keyOf(obj);
    if (cache.current.has(k) || loading.current.has(k)) return;
    loading.current.add(k);
    qc.fetchQuery({
      queryKey: sk(sessionId, "heap-biggest", obj ?? null, 60),
      queryFn: () => client.heapBiggest(sessionId, { object: obj, limit: 60 }),
    })
      .then((r) => {
        cache.current.set(k, r.rows);
        setVersion((v) => v + 1);
      })
      .catch(() => cache.current.set(k, []))
      .finally(() => loading.current.delete(k));
  };

  const segs = useMemo(() => {
    const out: Seg[] = [];
    const rootRows = cache.current.get(keyOf(center?.id));
    if (!rootRows) {
      load(center?.id);
      return out;
    }
    const total = center?.retained ?? rootRows.reduce((s, r) => s + (r.retained ?? r.shallow), 0);
    const place = (rows: ObjectRow[], parentTotal: number, a0: number, a1: number, level: number) => {
      let a = a0;
      for (const r of rows) {
        const w = ((r.retained ?? r.shallow) / Math.max(1, parentTotal)) * (a1 - a0);
        if (w < MIN_ANGLE) break;
        const seg = { row: r, a0: a, a1: a + w, level };
        out.push(seg);
        if (level < MAX_LEVELS && (r.child_count ?? 1) > 0) {
          const kids = cache.current.get(String(r.id));
          if (kids) place(kids, r.retained ?? r.shallow, a, a + w, level + 1);
          else if (w > MIN_ANGLE * 3) load(r.id);
        }
        a += w;
      }
    };
    place(rootRows, total, 0, Math.PI * 2, 1);
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [center, version, sessionId]);

  const ringW = size / 2 / (MAX_LEVELS + 1);

  useEffect(() => {
    const c = canvasRef.current;
    if (!c) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = size * dpr;
    c.height = size * dpr;
    c.style.width = `${size}px`;
    c.style.height = `${size}px`;
    const ctx = c.getContext("2d")!;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, size, size);
    const cx = size / 2;
    const light = theme === "light";
    const bg = cssVar("var(--bg)");
    ctx.fillStyle = cssVar("var(--bg-elev-2)");
    ctx.beginPath();
    ctx.arc(cx, cx, ringW, 0, Math.PI * 2);
    ctx.fill();
    for (const s of segs) {
      const r0 = s.level * ringW;
      const r1 = r0 + ringW - 1;
      ctx.beginPath();
      ctx.arc(cx, cx, r1, s.a0 - Math.PI / 2, s.a1 - Math.PI / 2);
      ctx.arc(cx, cx, r0, s.a1 - Math.PI / 2, s.a0 - Math.PI / 2, true);
      ctx.closePath();
      ctx.fillStyle = frameColor(s.row.class_name + ".x", "jit", light);
      if (hover && hover.seg !== s) ctx.globalAlpha = 0.75;
      ctx.fill();
      ctx.globalAlpha = 1;
      ctx.strokeStyle = bg;
      ctx.lineWidth = 1;
      ctx.stroke();
    }
    ctx.fillStyle = cssVar("var(--fg)");
    ctx.font = "600 12px Inter, system-ui, sans-serif";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    const label = center ? center.class_name.split(".").pop()! : "Heap";
    ctx.fillText(label.length > 14 ? label.slice(0, 13) + "…" : label, cx, cx - 7);
    ctx.font = "11px Inter, system-ui, sans-serif";
    ctx.fillStyle = cssVar("var(--fg-muted)");
    const tot =
      center?.retained ?? segs.filter((s) => s.level === 1).reduce((a, s) => a + (s.row.retained ?? 0), 0);
    ctx.fillText(formatBytes(tot), cx, cx + 8);
  }, [segs, size, theme, hover, center, ringW]);

  const hit = (e: React.MouseEvent): Seg | null => {
    const r = canvasRef.current!.getBoundingClientRect();
    const x = e.clientX - r.left - size / 2;
    const y = e.clientY - r.top - size / 2;
    const rad = Math.sqrt(x * x + y * y);
    const level = Math.floor(rad / ringW);
    let ang = Math.atan2(y, x) + Math.PI / 2;
    if (ang < 0) ang += Math.PI * 2;
    return segs.find((s) => s.level === level && ang >= s.a0 && ang < s.a1) ?? null;
  };

  const total =
    center?.retained ?? segs.filter((s) => s.level === 1).reduce((a, s) => a + (s.row.retained ?? 0), 0);

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="toolbar">
        <nav className="breadcrumb" aria-label="Sunburst path">
          <button aria-current={!center} onClick={() => (setCenter(null), setTrail([]))}>
            Heap
          </button>
          {trail.map((t, i) => (
            <span key={t.id} style={{ display: "contents" }}>
              <span className="chev">›</span>
              <button
                aria-current={center?.id === t.id}
                onClick={() => {
                  setCenter(t);
                  setTrail(trail.slice(0, i + 1));
                }}
              >
                {t.class_name.split(".").pop()}
              </button>
            </span>
          ))}
        </nav>
        <span className="grow" />
        <span className="faint" style={{ fontSize: 11.5 }}>
          Click to select · double-click to drill in · click centre to go up
        </span>
      </div>
      <div
        ref={wrapRef}
        style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", minHeight: 0 }}
      >
        <canvas
          ref={canvasRef}
          role="img"
          aria-label="Dominator tree sunburst"
          onMouseMove={(e) => {
            const s = hit(e);
            setHover(s ? { seg: s, x: e.clientX, y: e.clientY } : null);
          }}
          onMouseLeave={() => setHover(null)}
          onClick={(e) => {
            const s = hit(e);
            if (s) onSelect?.(s.row);
            else if (center) {
              const t = trail.slice(0, -1);
              setTrail(t);
              setCenter(t[t.length - 1] ?? null);
            }
          }}
          onDoubleClick={(e) => {
            const s = hit(e);
            if (s && (s.row.child_count ?? 1) > 0) {
              setCenter(s.row);
              setTrail([...trail, s.row]);
            }
          }}
        />
      </div>
      {hover && (
        <div className="tooltip" style={{ left: hover.x + 14, top: hover.y + 14 }}>
          <div className="t-title mono">{hover.seg.row.class_name}</div>
          <div className="faint mono">{hover.seg.row.address}</div>
          <div>
            Retained: <b>{formatBytes(hover.seg.row.retained ?? 0)}</b> (
            {formatPercent(hover.seg.row.retained ?? 0, total)})
          </div>
          <div>Shallow: {formatBytes(hover.seg.row.shallow)}</div>
          {hover.seg.row.preview && <div className="mono">{hover.seg.row.preview}</div>}
        </div>
      )}
    </div>
  );
}
