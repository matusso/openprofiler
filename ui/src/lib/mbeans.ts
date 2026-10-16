import type { Unit } from "../api/types";

/** Key properties of an ObjectName, i.e. everything after the domain. */
export function beanLabel(name: string): string {
  const i = name.indexOf(":");
  return i < 0 ? name : name.slice(i + 1);
}

/** Bytes for memory-looking attributes, counts otherwise (loads are 0–1 fractions, not percent). */
export function guessUnit(attribute: string, item?: string): Unit {
  const s = `${attribute} ${item ?? ""}`;
  if (/bytes|memory|size|used|committed|max|init|capacity/i.test(s)) return "bytes";
  return "count";
}
