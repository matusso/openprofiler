import type { ElkExtendedEdge, ElkNode } from "elkjs/lib/elk-api";
import { useEffect, useRef, useState } from "react";
import { layoutGraph } from "../lib/elk";

export interface GNode {
  id: string;
  label: string;
  sub?: string;
  focus?: boolean;
  /** 0..1 heat used for the left accent strip. */
  heat?: number;
}

export interface GEdge {
  id: string;
  source: string;
  target: string;
  label?: string;
  /** 0..1 relative weight → stroke width. */
  weight?: number;
}

const CHAR_W = 6.6;

export function GraphView({
  nodes,
  edges,
  onNodeClick,
  direction = "DOWN",
  label,
}: {
  nodes: GNode[];
  edges: GEdge[];
  onNodeClick?: (id: string) => void;
  direction?: "DOWN" | "RIGHT";
  label: string;
}) {
  const [laid, setLaid] = useState<ElkNode | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [view, setView] = useState({ x: 20, y: 20, k: 1 });
  const drag = useRef<{ x: number; y: number; vx: number; vy: number } | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);

  useEffect(() => {
    let cancelled = false;
    const graph: ElkNode = {
      id: "root",
      layoutOptions: {
        "elk.algorithm": "layered",
        "elk.direction": direction,
        "elk.layered.spacing.nodeNodeBetweenLayers": "48",
        "elk.spacing.nodeNode": "24",
        "elk.edgeRouting": "SPLINES",
      },
      children: nodes.map((n) => ({
        id: n.id,
        width: Math.min(360, Math.max(n.label.length, (n.sub ?? "").length) * CHAR_W + 24),
        height: n.sub ? 40 : 26,
      })),
      edges: edges.map((e) => ({ id: e.id, sources: [e.source], targets: [e.target] })),
    };
    layoutGraph(graph)
      .then((g) => {
        if (!cancelled) {
          setLaid(g);
          setErr(null);
          const svg = svgRef.current;
          if (svg && g.width && g.height) {
            const k = Math.min(1, svg.clientWidth / (g.width + 40), svg.clientHeight / (g.height + 40));
            setView({ x: 20, y: 20, k: Math.max(0.2, k) });
          }
        }
      })
      .catch((e: Error) => !cancelled && setErr(e.message));
    return () => {
      cancelled = true;
    };
  }, [nodes, edges, direction]);

  const byId = new Map(nodes.map((n) => [n.id, n]));
  const edgeMeta = new Map(edges.map((e) => [e.id, e]));

  const onWheel = (e: React.WheelEvent) => {
    const r = svgRef.current!.getBoundingClientRect();
    const mx = e.clientX - r.left;
    const my = e.clientY - r.top;
    const k2 = Math.max(0.1, Math.min(3, view.k * (e.deltaY < 0 ? 1.1 : 1 / 1.1)));
    setView({ k: k2, x: mx - ((mx - view.x) * k2) / view.k, y: my - ((my - view.y) * k2) / view.k });
  };

  if (err) return <div className="error">Layout failed: {err}</div>;

  return (
    <svg
      ref={svgRef}
      className="graph-svg"
      role="img"
      aria-label={label}
      onWheel={onWheel}
      onMouseDown={(e) => (drag.current = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y })}
      onMouseMove={(e) => {
        const d = drag.current;
        if (d) setView((v) => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
      }}
      onMouseUp={() => (drag.current = null)}
      onMouseLeave={() => (drag.current = null)}
    >
      <defs>
        <marker
          id="arrow"
          viewBox="0 0 10 10"
          refX="9"
          refY="5"
          markerWidth="7"
          markerHeight="7"
          orient="auto"
        >
          <path d="M0,0 L10,5 L0,10 z" fill="var(--fg-faint)" />
        </marker>
      </defs>
      {!laid && (
        <text x={20} y={30} fill="var(--fg-faint)">
          Laying out…
        </text>
      )}
      <g transform={`translate(${view.x},${view.y}) scale(${view.k})`}>
        {(laid?.edges as ElkExtendedEdge[] | undefined)?.map((e) => {
          const meta = edgeMeta.get(e.id);
          return (e.sections ?? []).map((s, i) => {
            const pts = [s.startPoint, ...(s.bendPoints ?? []), s.endPoint];
            const d = pts.map((p, j) => `${j ? "L" : "M"}${p.x},${p.y}`).join(" ");
            return (
              <g key={`${e.id}-${i}`}>
                <path
                  className="gedge"
                  d={d}
                  strokeWidth={1 + (meta?.weight ?? 0) * 5}
                  markerEnd="url(#arrow)"
                  opacity={0.4 + (meta?.weight ?? 0) * 0.6}
                />
                {meta?.label && (
                  <text
                    x={(s.startPoint.x + s.endPoint.x) / 2 + 4}
                    y={(s.startPoint.y + s.endPoint.y) / 2}
                    fontSize={10}
                    fill="var(--fg-muted)"
                  >
                    {meta.label}
                  </text>
                )}
              </g>
            );
          });
        })}
        {laid?.children?.map((c) => {
          const n = byId.get(c.id);
          if (!n) return null;
          return (
            <g
              key={c.id}
              className={`gnode${n.focus ? " focus" : ""}`}
              transform={`translate(${c.x},${c.y})`}
              onClick={() => onNodeClick?.(c.id)}
              style={{ cursor: onNodeClick ? "pointer" : undefined }}
            >
              <title>{n.label}</title>
              <rect width={c.width} height={c.height} />
              {n.heat !== undefined && (
                <rect
                  width={4}
                  height={c.height}
                  style={{ fill: "var(--accent)", opacity: 0.2 + n.heat * 0.8 }}
                />
              )}
              <text x={10} y={17}>
                {n.label}
              </text>
              {n.sub && (
                <text x={10} y={32} className="sub">
                  {n.sub}
                </text>
              )}
            </g>
          );
        })}
      </g>
    </svg>
  );
}
