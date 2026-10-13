import type { DumpedThread, ThreadDump } from "../api/types";

export function frameKey(t: DumpedThread) {
  return `${t.java_id}:${t.name}`;
}

/** Line-level diff of two thread dumps keyed by thread. */
export function diffDumps(a: ThreadDump, b: ThreadDump) {
  const am = new Map(a.threads.map((t) => [frameKey(t), t]));
  const bm = new Map(b.threads.map((t) => [frameKey(t), t]));
  const out: {
    thread: DumpedThread;
    status: "added" | "removed" | "changed" | "same";
    prev?: DumpedThread;
  }[] = [];
  for (const t of b.threads) {
    const p = am.get(frameKey(t));
    if (!p) out.push({ thread: t, status: "added" });
    else {
      const same = p.state === t.state && p.frames.join("\n") === t.frames.join("\n");
      out.push({ thread: t, status: same ? "same" : "changed", prev: p });
    }
  }
  for (const t of a.threads) if (!bm.has(frameKey(t))) out.push({ thread: t, status: "removed" });
  return out;
}
