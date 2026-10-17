import type { ComplexityFit, ComplexityModel } from "../api/types";

/** Growth function of a complexity model (as fitted by the daemon: `t = a + b·f(n)`). */
export function growth(model: ComplexityModel, n: number): number {
  const ln = Math.log(Math.max(1, n));
  switch (model) {
    case "O(1)":
      return 0;
    case "O(log n)":
      return ln;
    case "O(n)":
      return n;
    case "O(n log n)":
      return n * ln;
    case "O(n^2)":
      return n * n;
  }
}

/** `steps + 1` points of a fitted curve over `[0, maxSize]`. */
export function fitCurve(fit: ComplexityFit, maxSize: number, steps = 64): [number, number][] {
  const out: [number, number][] = [];
  for (let i = 0; i <= steps; i++) {
    const n = (maxSize * i) / steps;
    out.push([n, fit.a + fit.b * growth(fit.model, n)]);
  }
  return out;
}

/** `1.234 ms` style offset of `t` from `origin`, precise enough for a call trace. */
export function formatOffset(t: number, origin: number): string {
  const us = (t - origin) / 1e3;
  if (us < 1000) return `+${us.toFixed(1)} µs`;
  if (us < 1e6) return `+${(us / 1e3).toFixed(3)} ms`;
  return `+${(us / 1e6).toFixed(4)} s`;
}

/** A "nice" upper bound for an axis (1, 2 or 5 × 10^k at or above `v`). */
export function niceMax(v: number): number {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}
