import { describe, expect, it } from "vitest";
import { fuzzyScore } from "./CommandPalette";

describe("fuzzyScore", () => {
  it("prefers direct substring matches", () => {
    expect(fuzzyScore("java.lang.String.hashCode", "hash")).toBeGreaterThan(
      fuzzyScore("java.util.HashMap.get", "hmg"),
    );
  });
  it("rejects non-matching", () => {
    expect(fuzzyScore("Go to CPU", "xyz")).toBe(0);
  });
  it("matches in-order characters", () => {
    expect(fuzzyScore("java.util.HashMap.get", "hmg")).toBeGreaterThan(0);
  });
});
