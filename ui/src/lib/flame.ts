import type { FlameLayout } from "../api/types";

/** Derived structure for a pre-order flame layout: parent and subtree end per rectangle. */
export interface FlameIndex {
  parent: Int32Array;
  end: Int32Array;
}

export function indexFlame(l: FlameLayout): FlameIndex {
  const n = l.node.length;
  const parent = new Int32Array(n).fill(-1);
  const end = new Int32Array(n).fill(n);
  const stack: number[] = [];
  for (let i = 0; i < n; i++) {
    const d = l.depth[i];
    while (stack.length && l.depth[stack[stack.length - 1]] >= d) {
      end[stack.pop()!] = i;
    }
    parent[i] = stack.length ? stack[stack.length - 1] : -1;
    stack.push(i);
  }
  return { parent, end };
}

export function ancestorsOf(idx: FlameIndex, i: number): number[] {
  const out: number[] = [];
  let p = idx.parent[i];
  while (p >= 0) {
    out.push(p);
    p = idx.parent[p];
  }
  return out.reverse();
}

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/** Warm palette keyed by class so frames from the same class share a hue. */
export function frameColor(label: string, kind: string, light: boolean): string {
  const paren = label.indexOf("(");
  const base = paren >= 0 ? label.slice(0, paren) : label;
  const cls = base.slice(0, Math.max(0, base.lastIndexOf(".")));
  const h = hash(cls || base);
  if (kind === "native" || kind === "kernel" || kind === "cpp") {
    const hue = 190 + (h % 40);
    return `hsl(${hue} ${light ? 55 : 45}% ${light ? 70 : 48}%)`;
  }
  if (kind === "interpreted") {
    const hue = 45 + (h % 15);
    return `hsl(${hue} 80% ${light ? 66 : 52}%)`;
  }
  const hue = 5 + (h % 35);
  const sat = 70 + (h % 20);
  const lig = light ? 62 + ((h >> 8) % 12) : 48 + ((h >> 8) % 12);
  return `hsl(${hue} ${sat}% ${lig}%)`;
}

/** Diverging colour for differential flames: blue for decrease, red for increase. */
export function diffColor(delta: number, width: number, light: boolean): string {
  const ratio = width > 0 ? Math.max(-1, Math.min(1, delta / width)) : 0;
  const mag = Math.min(1, Math.abs(ratio) * 2);
  const lig = light ? 92 - mag * 32 : 30 + mag * 22;
  if (Math.abs(ratio) < 0.01) return light ? "hsl(220 10% 86%)" : "hsl(220 10% 30%)";
  return ratio > 0 ? `hsl(355 ${40 + mag * 45}% ${lig}%)` : `hsl(215 ${40 + mag * 45}% ${lig}%)`;
}
