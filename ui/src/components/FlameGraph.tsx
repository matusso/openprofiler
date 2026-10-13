import { useEffect, useMemo, useRef, useState } from "react";
import type { FlameLayout, Unit } from "../api/types";
import { ancestorsOf, diffColor, frameColor, indexFlame } from "../lib/flame";
import { formatPercent, formatValue } from "../lib/format";
import { useApp } from "../state";
import { cssVar } from "./TimeChart";

const ROW = 18;

export interface FlameGraphProps {
  layout: FlameLayout;
  unit: Unit;
  orientation?: "icicle" | "flame";
  search?: string;
  /** Called on single click with the rectangle's label (for the inspector / sandwich). */
  onSelect?: (label: string, index: number) => void;
  onContextMenu?: (label: string, index: number, e: React.MouseEvent) => void;
  label?: string;
}

interface Hover {
  i: number;
  x: number;
  y: number;
}

export function FlameGraph({
  layout,
  unit,
  orientation = "icicle",
  search,
  onSelect,
  onContextMenu,
  label,
}: FlameGraphProps) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(800);
  const [zoom, setZoom] = useState(0);
  const [hover, setHover] = useState<Hover | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const theme = useApp((s) => s.theme);
  const index = useMemo(() => indexFlame(layout), [layout]);

  useEffect(() => {
    setZoom(0);
    setSelected(null);
  }, [layout]);

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    setWidth(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  const n = layout.node.length;
  const z = Math.min(zoom, Math.max(0, n - 1));
  const zx = n ? layout.x[z] : 0;
  const zw = n ? Math.max(1, layout.width[z]) : 1;
  const zd = n ? layout.depth[z] : 0;
  const ancestors = useMemo(() => (n ? ancestorsOf(index, z) : []), [index, z, n]);
  const rows = layout.max_depth + 1;
  const height = Math.max(ROW * (rows + 1), 60);

  const matches = useMemo(() => {
    const q = search?.trim().toLowerCase();
    if (!q) return null;
    const set = new Set<number>();
    layout.names.forEach((nm, i) => {
      if (nm.toLowerCase().includes(q)) set.add(i);
    });
    return set;
  }, [search, layout.names]);

  const matchedWeight = useMemo(() => {
    if (!matches) return 0;
    // Sum the outermost matching rectangles to avoid double counting.
    let w = 0;
    let skipUntil = -1;
    for (let i = 0; i < n; i++) {
      if (i < skipUntil) continue;
      if (matches.has(layout.name[i])) {
        w += layout.width[i];
        skipUntil = index.end[i];
      }
    }
    return w;
  }, [matches, layout, index, n]);

  const yOf = (level: number) => (orientation === "icicle" ? level * ROW : height - (level + 1) * ROW);

  useEffect(() => {
    const c = canvasRef.current;
    if (!c) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = Math.round(width * dpr);
    c.height = Math.round(height * dpr);
    c.style.width = `${width}px`;
    c.style.height = `${height}px`;
    const ctx = c.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const light = theme === "light";
    const fg = light ? "#1a1a1a" : "#121212";
    const dimFg = cssVar("var(--fg-faint)");
    const bgDim = cssVar("var(--bg-hover)");
    const sel = cssVar("var(--focus)");
    ctx.font = "11px Inter, -apple-system, system-ui, sans-serif";
    ctx.textBaseline = "middle";
    const charW = 6.4;

    const drawRect = (i: number, px: number, pw: number, level: number, faded: boolean) => {
      const y = yOf(level);
      const nm = layout.names[layout.name[i]];
      let fill: string;
      if (layout.delta) fill = diffColor(layout.delta[i], layout.width[i], light);
      else fill = frameColor(nm, layout.kind[i], light);
      if (matches && !matches.has(layout.name[i])) fill = bgDim;
      else if (matches) fill = "#ffd400";
      ctx.globalAlpha = faded ? 0.45 : 1;
      ctx.fillStyle = fill;
      ctx.fillRect(px, y + 0.5, Math.max(0.5, pw - 0.5), ROW - 1);
      if (i === selected) {
        ctx.strokeStyle = sel;
        ctx.lineWidth = 2;
        ctx.strokeRect(px + 1, y + 1.5, Math.max(0, pw - 2), ROW - 3);
      }
      if (pw > 28) {
        const maxChars = Math.floor((pw - 8) / charW);
        const text = nm.length > maxChars ? nm.slice(0, Math.max(1, maxChars - 1)) + "…" : nm;
        ctx.fillStyle = matches && !matches.has(layout.name[i]) ? dimFg : fg;
        ctx.fillText(text, px + 4, y + ROW / 2);
      }
      ctx.globalAlpha = 1;
    };

    // Ancestors of the zoomed frame as full-width faded bars.
    ancestors.forEach((a, k) => drawRect(a, 0, width, k, true));
    const base = ancestors.length;
    const scale = width / zw;
    for (let i = z; i < index.end[z];) {
      const pw = layout.width[i] * scale;
      if (pw < 0.4) {
        i = index.end[i];
        continue;
      }
      const px = (layout.x[i] - zx) * scale;
      drawRect(i, px, pw, base + layout.depth[i] - zd, false);
      i++;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [layout, index, width, height, z, zx, zw, zd, ancestors, matches, selected, theme, orientation]);

  const hit = (mx: number, my: number): number | null => {
    const level = orientation === "icicle" ? Math.floor(my / ROW) : Math.floor((height - my) / ROW);
    if (level < 0) return null;
    if (level < ancestors.length) return ancestors[level];
    const depth = level - ancestors.length + zd;
    const v = zx + (mx / width) * zw;
    for (let i = z; i < index.end[z];) {
      if (layout.x[i] > v || layout.x[i] + layout.width[i] < v) {
        i = index.end[i];
        continue;
      }
      if (layout.depth[i] === depth) return i;
      i++;
    }
    return null;
  };

  const onMove = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const i = hit(e.clientX - r.left, e.clientY - r.top);
    setHover(i === null ? null : { i, x: e.clientX, y: e.clientY });
  };

  const onClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const i = hit(e.clientX - r.left, e.clientY - r.top);
    if (i === null) return;
    setSelected(i);
    onSelect?.(layout.names[layout.name[i]], i);
  };

  const onDouble = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const i = hit(e.clientX - r.left, e.clientY - r.top);
    if (i !== null) setZoom(i);
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape" || e.key === "Backspace") {
      setZoom(index.parent[z] >= 0 ? index.parent[z] : 0);
    } else if (e.key === "Enter" && selected !== null) {
      setZoom(selected);
    } else if (e.key === "0") setZoom(0);
  };

  const hv = hover ? hover.i : null;
  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", minHeight: 0 }}>
      <div className="toolbar" style={{ borderBottom: "1px solid var(--border)" }}>
        <nav className="breadcrumb" aria-label="Flame zoom">
          {[...ancestors, z].map((a, k) => (
            <span key={a} style={{ display: "contents" }}>
              {k > 0 && <span className="chev">›</span>}
              <button aria-current={a === z} onClick={() => setZoom(a)} title={layout.names[layout.name[a]]}>
                {k === 0 && a === 0 ? "All" : layout.names[layout.name[a]]}
              </button>
            </span>
          ))}
        </nav>
        <span className="grow" />
        {matches && (
          <span className="badge accent">
            {matches.size ? `match: ${formatPercent(matchedWeight, layout.total)}` : "no match"}
          </span>
        )}
        <span className="faint" style={{ fontSize: 11.5 }}>
          Double-click to zoom · Esc to zoom out
        </span>
      </div>
      <div
        ref={wrapRef}
        className="canvas-wrap"
        style={{ overflowY: "auto", flex: 1 }}
        tabIndex={0}
        onKeyDown={onKey}
        aria-label={label ?? "Flame graph"}
        role="img"
      >
        <canvas
          ref={canvasRef}
          onMouseMove={onMove}
          onMouseLeave={() => setHover(null)}
          onClick={onClick}
          onDoubleClick={onDouble}
          onContextMenu={(e) => {
            if (!onContextMenu) return;
            const r = e.currentTarget.getBoundingClientRect();
            const i = hit(e.clientX - r.left, e.clientY - r.top);
            if (i === null || i === 0) return;
            setSelected(i);
            onContextMenu(layout.names[layout.name[i]], i, e);
          }}
          style={{ cursor: hv !== null ? "pointer" : "default" }}
        />
      </div>
      {hover && hv !== null && (
        <div className="tooltip" style={{ left: hover.x + 14, top: hover.y + 14 }}>
          <div className="t-title mono">{layout.names[layout.name[hv]]}</div>
          <div>
            Total: <b>{formatValue(layout.width[hv], unit)}</b> (
            {formatPercent(layout.width[hv], layout.total)})
          </div>
          <div>
            Self: {formatValue(layout.self_weight[hv], unit)} (
            {formatPercent(layout.self_weight[hv], layout.total)})
          </div>
          {layout.delta && (
            <div>
              Δ:{" "}
              <b style={{ color: layout.delta[hv] > 0 ? "var(--diff-pos)" : "var(--diff-neg)" }}>
                {layout.delta[hv] > 0 ? "+" : ""}
                {formatValue(layout.delta[hv], unit)}
              </b>
            </div>
          )}
          <div className="faint">{layout.kind[hv]}</div>
        </div>
      )}
    </div>
  );
}
