import { describe, expect, it } from "vitest";
import { fitCurve, formatOffset, growth, niceMax } from "./calls";

describe("call analysis helpers", () => {
  it("evaluates growth models", () => {
    expect(growth("O(1)", 100)).toBe(0);
    expect(growth("O(n)", 100)).toBe(100);
    expect(growth("O(n^2)", 10)).toBe(100);
    expect(growth("O(log n)", 0)).toBe(0);
    expect(growth("O(n log n)", Math.E)).toBeCloseTo(Math.E);
  });

  it("draws a fitted curve", () => {
    const c = fitCurve({ model: "O(n)", a: 5, b: 2, r2: 1 }, 100, 4);
    expect(c).toEqual([
      [0, 5],
      [25, 55],
      [50, 105],
      [75, 155],
      [100, 205],
    ]);
  });

  it("formats offsets and axis bounds", () => {
    expect(formatOffset(1_500, 0)).toBe("+1.5 µs");
    expect(formatOffset(2_500_000, 0)).toBe("+2.500 ms");
    expect(niceMax(730)).toBe(1000);
    expect(niceMax(180)).toBe(200);
    expect(niceMax(0)).toBe(1);
  });
});
