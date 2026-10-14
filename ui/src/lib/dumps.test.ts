import { describe, expect, it } from "vitest";
import type { ThreadDump } from "../api/types";
import { diffDumps } from "./dumps";

const t = (name: string, state: string, frames: string[]) => ({
  name,
  java_id: name.length,
  state,
  is_virtual: false,
  frames,
  locked: [],
});

describe("diffDumps", () => {
  it("classifies added, removed, changed and same threads", () => {
    const a: ThreadDump = {
      time: 0,
      label: "a",
      threads: [t("main", "RUNNABLE", ["A.a"]), t("gone", "WAITING", []), t("same", "WAITING", ["x"])],
    };
    const b: ThreadDump = {
      time: 1,
      label: "b",
      threads: [t("main", "BLOCKED", ["A.b"]), t("new1", "RUNNABLE", []), t("same", "WAITING", ["x"])],
    };
    const d = diffDumps(a, b);
    const status = Object.fromEntries(d.map((x) => [x.thread.name, x.status]));
    expect(status).toEqual({ main: "changed", new1: "added", gone: "removed", same: "same" });
  });
});
