import { useEffect, useRef } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import type { BookmarkDto, GcEventDto, Unit } from "../api/types";
import { formatValue } from "../lib/format";
import { useApp } from "../state";

export interface ChartSeries {
  label: string;
  t: number[]; // epoch ns
  v: number[];
  color: string; // CSS var name or colour
  fill?: boolean;
}

export interface TimeChartProps {
  series: ChartSeries[];
  unit: Unit;
  height?: number;
  gc?: GcEventDto[];
  bookmarks?: BookmarkDto[];
  /** Full x domain in epoch ns. */
  domain?: [number, number];
  /** Currently selected global range, drawn as a highlight. */
  range?: [number, number] | null;
  onSelectRange?: (r: [number, number]) => void;
  stacked?: boolean;
}

export function cssVar(name: string): string {
  if (!name.startsWith("var(")) return name;
  const v = name.slice(4, -1).trim();
  return getComputedStyle(document.documentElement).getPropertyValue(v).trim() || "#888";
}

function withAlpha(color: string, alpha: number): string {
  const c = color.trim();
  if (c.startsWith("#") && c.length === 7) {
    const a = Math.round(alpha * 255)
      .toString(16)
      .padStart(2, "0");
    return c + a;
  }
  return c;
}

/** Merges series with different timestamps into uPlot's aligned format (x in seconds). */
export function alignSeries(series: ChartSeries[]): uPlot.AlignedData {
  if (series.length === 1) return [series[0].t.map((t) => t / 1e9), series[0].v];
  const tables = series.map((s) => [s.t.map((t) => t / 1e9), s.v] as uPlot.AlignedData);
  return uPlot.join(tables as uPlot.AlignedData[]);
}

export function TimeChart(props: TimeChartProps) {
  const ref = useRef<HTMLDivElement>(null);
  const plot = useRef<uPlot | null>(null);
  const theme = useApp((s) => s.theme);
  const palette = useApp((s) => s.palette);
  const propsRef = useRef(props);
  propsRef.current = props;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { series, unit, height = 150, domain } = props;
    const axis = cssVar("var(--chart-axis)");
    const grid = cssVar("var(--chart-grid)");
    const gcColor = withAlpha(cssVar("var(--danger)"), 0.28);
    const bmColor = cssVar("var(--info)");
    const accent = cssVar("var(--accent)");
    const data = alignSeries(series);
    const opts: uPlot.Options = {
      width: el.clientWidth || 400,
      height,
      padding: [8, 8, 0, 0],
      legend: { show: false },
      cursor: {
        drag: { x: true, y: false, setScale: false },
        points: { size: 6 },
      },
      select: { show: true, left: 0, top: 0, width: 0, height: 0 },
      scales: {
        x: {
          time: true,
          range: domain ? () => [domain[0] / 1e9, domain[1] / 1e9] : undefined,
        },
        y: { range: (_u, _min, max) => [0, max > 0 ? max * 1.08 : 1] },
      },
      axes: [
        { stroke: axis, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid }, size: 28 },
        {
          stroke: axis,
          grid: { stroke: grid, width: 1 },
          ticks: { show: false },
          size: 64,
          values: (_u, vals) => vals.map((v) => formatValue(v, unit)),
        },
      ],
      series: [
        {},
        ...series.map((s) => {
          const c = cssVar(s.color);
          return {
            label: s.label,
            stroke: c,
            width: 1.5,
            fill: s.fill === false ? undefined : withAlpha(c, 0.14),
            points: { show: false },
            spanGaps: true,
            value: (_u: uPlot, v: number) => formatValue(v, unit),
          };
        }),
      ],
      hooks: {
        drawClear: [
          (u) => {
            const p = propsRef.current;
            const ctx = u.ctx;
            ctx.save();
            const top = u.bbox.top;
            const h = u.bbox.height;
            if (p.range) {
              const x0 = u.valToPos(p.range[0] / 1e9, "x", true);
              const x1 = u.valToPos(p.range[1] / 1e9, "x", true);
              ctx.fillStyle = withAlpha(accent, 0.1);
              ctx.fillRect(x0, top, Math.max(1, x1 - x0), h);
            }
            ctx.fillStyle = gcColor;
            for (const g of p.gc ?? []) {
              const x0 = u.valToPos(g.start / 1e9, "x", true);
              const x1 = u.valToPos((g.start + Math.max(g.duration, 0)) / 1e9, "x", true);
              ctx.fillRect(x0, top, Math.max(1.5, x1 - x0), h);
            }
            ctx.restore();
          },
        ],
        draw: [
          (u) => {
            const p = propsRef.current;
            if (!p.bookmarks?.length) return;
            const ctx = u.ctx;
            ctx.save();
            ctx.strokeStyle = bmColor;
            ctx.setLineDash([4, 3]);
            ctx.lineWidth = 1;
            ctx.fillStyle = bmColor;
            ctx.font = `${10 * devicePixelRatio}px sans-serif`;
            for (const b of p.bookmarks) {
              const x = u.valToPos(b.time / 1e9, "x", true);
              ctx.beginPath();
              ctx.moveTo(x, u.bbox.top);
              ctx.lineTo(x, u.bbox.top + u.bbox.height);
              ctx.stroke();
              ctx.fillText(b.label, x + 3, u.bbox.top + 10 * devicePixelRatio);
            }
            ctx.restore();
          },
        ],
        setSelect: [
          (u) => {
            if (u.select.width < 3) return;
            const a = u.posToVal(u.select.left, "x");
            const b = u.posToVal(u.select.left + u.select.width, "x");
            propsRef.current.onSelectRange?.([Math.round(a * 1e9), Math.round(b * 1e9)]);
            u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
          },
        ],
      },
    };
    const u = new uPlot(opts, data, el);
    plot.current = u;
    const ro = new ResizeObserver(() => {
      if (el.clientWidth > 0) u.setSize({ width: el.clientWidth, height });
    });
    ro.observe(el);
    return () => {
      ro.disconnect();
      u.destroy();
      plot.current = null;
    };
    // Re-create on structural changes; data updates go through setData below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [theme, palette, props.series.length, props.unit, props.height, props.domain?.[0], props.domain?.[1]]);

  useEffect(() => {
    plot.current?.setData(alignSeries(props.series));
  }, [props.series]);

  useEffect(() => {
    plot.current?.redraw(false);
  }, [props.range, props.gc, props.bookmarks]);

  return <div ref={ref} role="img" aria-label={`Chart: ${props.series.map((s) => s.label).join(", ")}`} />;
}
