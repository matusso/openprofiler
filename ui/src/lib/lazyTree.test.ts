import { describe, expect, it } from "vitest";
import { LazyTree } from "./lazyTree";

type N = { id: number; parent: number | null; child_count: number; total: number };

describe("LazyTree", () => {
  it("merges levels and flattens expanded nodes", () => {
    const t = new LazyTree<N>();
    t.merge(
      [
        { id: 0, parent: null, child_count: 2, total: 100 },
        { id: 1, parent: 0, child_count: 1, total: 80 },
        { id: 3, parent: 1, child_count: 1, total: 80 },
        { id: 2, parent: 0, child_count: 0, total: 20 },
      ],
      2,
    );
    expect(t.isLoaded(0)).toBe(true);
    expect(t.isLoaded(1)).toBe(true);
    expect(t.isLoaded(3)).toBe(false);
    expect(t.flatten(new Set()).map((r) => r.node.id)).toEqual([1, 2]);
    expect(t.flatten(new Set([1])).map((r) => [r.node.id, r.level])).toEqual([
      [1, 0],
      [3, 1],
      [2, 0],
    ]);
    // Expanding node 3 loads its children.
    t.merge(
      [
        { id: 3, parent: 1, child_count: 1, total: 80 },
        { id: 4, parent: 3, child_count: 0, total: 70 },
      ],
      1,
    );
    expect(t.isLoaded(3)).toBe(true);
    expect(t.depth.get(4)).toBe(3);
    expect(t.hotPath((n) => n.total)).toEqual([1, 3, 4]);
    expect(t.ancestors(4)).toEqual([3, 1, 0]);
  });
});
