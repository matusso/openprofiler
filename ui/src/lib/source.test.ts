import { describe, expect, it } from "vitest";
import { sourceTarget } from "./source";

describe("sourceTarget", () => {
  it("parses method labels", () => {
    expect(sourceTarget("java.util.HashMap.get(Object)")).toEqual({
      class: "java.util.HashMap",
      method: "get",
    });
    expect(sourceTarget("a.B$Inner.run")).toEqual({ class: "a.B$Inner", method: "run" });
  });
  it("handles aggregation levels and explicit fields", () => {
    expect(sourceTarget("java.util.HashMap", "class")).toEqual({ class: "java.util.HashMap" });
    expect(sourceTarget("java.util", "package")).toBeNull();
    expect(sourceTarget("x", "method", "a.B", "c")).toEqual({ class: "a.B", method: "c" });
    expect(sourceTarget("All threads")).toBeNull();
  });
});
