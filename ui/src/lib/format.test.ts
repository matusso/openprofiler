import { describe, expect, it } from "vitest";
import { formatBytes, formatNanos, formatPercent, splitLabel, formatRelTime } from "./format";

describe("format", () => {
  it("bytes", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(3 * 1024 ** 3)).toBe("3.0 GB");
  });
  it("nanos", () => {
    expect(formatNanos(500)).toBe("500 ns");
    expect(formatNanos(2_500_000)).toBe("2.50 ms");
    expect(formatNanos(1.5e9)).toBe("1.50 s");
    expect(formatNanos(125e9)).toBe("2m 5s");
  });
  it("percent", () => {
    expect(formatPercent(1, 4)).toBe("25.0 %");
    expect(formatPercent(1, 0)).toBe("0 %");
  });
  it("splitLabel", () => {
    expect(splitLabel("java.util.HashMap.get")).toEqual({ pkg: "java.util", cls: "HashMap", member: "get" });
    expect(splitLabel("main")).toEqual({ pkg: "", cls: "", member: "main" });
  });
  it("relative time", () => {
    expect(formatRelTime(1_500_000_000, 0)).toBe("1.50 s");
    expect(formatRelTime(125e9, 0)).toBe("2:05");
  });
});
