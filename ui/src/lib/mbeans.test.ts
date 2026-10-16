import { describe, expect, it } from "vitest";
import { beanLabel, guessUnit } from "./mbeans";

describe("mbeans", () => {
  it("labels beans by their key properties", () => {
    expect(beanLabel("java.lang:type=GarbageCollector,name=G1 Young Generation")).toBe(
      "type=GarbageCollector,name=G1 Young Generation",
    );
    expect(beanLabel("odd")).toBe("odd");
  });

  it("guesses units", () => {
    expect(guessUnit("HeapMemoryUsage", "used")).toBe("bytes");
    expect(guessUnit("ProcessCpuLoad")).toBe("count");
    expect(guessUnit("ThreadCount")).toBe("count");
  });
});
