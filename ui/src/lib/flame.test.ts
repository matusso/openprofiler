import { describe, expect, it } from "vitest";
import type { FlameLayout } from "../api/types";
import { ancestorsOf, diffColor, indexFlame } from "./flame";

const layout: FlameLayout = {
  total: 100,
  max_depth: 2,
  names: ["root", "a", "b", "c"],
  node: [0, 1, 2, 3],
  x: [0, 0, 0, 60],
  width: [100, 60, 40, 40],
  depth: [0, 1, 2, 1],
  name: [0, 1, 2, 3],
  self_weight: [0, 20, 40, 40],
  kind: ["unknown", "jit", "jit", "jit"],
};

describe("flame index", () => {
  it("computes parents and subtree ends", () => {
    const idx = indexFlame(layout);
    expect(Array.from(idx.parent)).toEqual([-1, 0, 1, 0]);
    expect(Array.from(idx.end)).toEqual([4, 3, 3, 4]);
    expect(ancestorsOf(idx, 2)).toEqual([0, 1]);
  });
  it("diff colours are diverging", () => {
    expect(diffColor(10, 10, false)).toContain("355");
    expect(diffColor(-10, 10, false)).toContain("215");
  });
});
