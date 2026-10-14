import type { Unit } from "../api/types";

const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB"];

export function formatBytes(n: number, digits = 1): string {
  if (!Number.isFinite(n)) return "–";
  const sign = n < 0 ? "-" : "";
  let v = Math.abs(n);
  let i = 0;
  while (v >= 1024 && i < BYTE_UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  return `${sign}${i === 0 ? v.toFixed(0) : v.toFixed(digits)} ${BYTE_UNITS[i]}`;
}

/** Formats a nanosecond duration with an adaptive unit. */
export function formatNanos(ns: number): string {
  if (!Number.isFinite(ns)) return "–";
  const sign = ns < 0 ? "-" : "";
  const a = Math.abs(ns);
  if (a < 1e3) return `${sign}${a.toFixed(0)} ns`;
  if (a < 1e6) return `${sign}${(a / 1e3).toFixed(1)} µs`;
  if (a < 1e9) return `${sign}${(a / 1e6).toFixed(a < 1e7 ? 2 : 0)} ms`;
  if (a < 60e9) return `${sign}${(a / 1e9).toFixed(2)} s`;
  const m = Math.floor(a / 60e9);
  const s = Math.round((a - m * 60e9) / 1e9);
  return `${sign}${m}m ${s}s`;
}

export function formatCount(n: number): string {
  if (!Number.isFinite(n)) return "–";
  const a = Math.abs(n);
  if (a >= 1e9) return `${(n / 1e9).toFixed(1)}G`;
  if (a >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (a >= 1e4) return `${(n / 1e3).toFixed(1)}k`;
  return n.toLocaleString("en-US");
}

export function formatValue(v: number, unit: Unit): string {
  switch (unit) {
    case "bytes":
      return formatBytes(v);
    case "nanos":
      return formatNanos(v);
    case "percent":
      return `${v.toFixed(1)} %`;
    case "per_second":
      return `${formatCount(v)}/s`;
    default:
      return formatCount(v);
  }
}

export function formatPercent(part: number, total: number, digits = 1): string {
  if (!total) return "0 %";
  return `${((part / total) * 100).toFixed(digits)} %`;
}

/** Formats an absolute epoch-nanosecond timestamp relative to `origin` (session start). */
export function formatRelTime(t: number, origin: number): string {
  const s = (t - origin) / 1e9;
  if (Math.abs(s) < 60) return `${s.toFixed(s < 10 ? 2 : 1)} s`;
  const m = Math.floor(s / 60);
  const rest = s - m * 60;
  if (m < 60) return `${m}:${rest.toFixed(0).padStart(2, "0")}`;
  const h = Math.floor(m / 60);
  return `${h}:${String(m % 60).padStart(2, "0")}:${rest.toFixed(0).padStart(2, "0")}`;
}

export function formatClock(t: number): string {
  const d = new Date(t / 1e6);
  return d.toLocaleTimeString("en-GB", { hour12: false });
}

/** Splits `java.util.HashMap.get` into `{ pkg: "java.util", cls: "HashMap", member: "get" }`. */
export function splitLabel(label: string): { pkg: string; cls: string; member: string } {
  const paren = label.indexOf("(");
  const base = paren >= 0 ? label.slice(0, paren) : label;
  const parts = base.split(".");
  if (parts.length < 2) return { pkg: "", cls: "", member: label };
  const member = parts.pop() ?? "";
  const cls = parts.pop() ?? "";
  return { pkg: parts.join("."), cls, member: member + (paren >= 0 ? label.slice(paren) : "") };
}
