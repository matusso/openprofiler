import { describe, expect, it } from "vitest";
import { looksDistroless, parseSshSpec } from "./remote";

describe("parseSshSpec", () => {
  it("parses user, host and port", () => {
    expect(parseSshSpec("me@box:2222")).toEqual({ type: "ssh", host: "box", user: "me", port: 2222 });
    expect(parseSshSpec(" box ")).toEqual({ type: "ssh", host: "box" });
    expect(parseSshSpec("a@b@box")).toEqual({ type: "ssh", host: "box", user: "a@b" });
    expect(parseSshSpec("fe80::1")).toEqual({ type: "ssh", host: "fe80::1" });
  });
  it("rejects invalid specs", () => {
    expect(parseSshSpec("")).toBeNull();
    expect(parseSshSpec("box:x")).toBeNull();
    expect(parseSshSpec("box:70000")).toBeNull();
    expect(parseSshSpec("@box")).toBeNull();
    expect(parseSshSpec("me@")).toBeNull();
  });
});

describe("looksDistroless", () => {
  it("matches shell-less images", () => {
    expect(looksDistroless("gcr.io/distroless/java21-debian12:nonroot")).toBe(true);
    expect(looksDistroless("cgr.dev/chainguard/jre:latest")).toBe(true);
    expect(looksDistroless("eclipse-temurin:21-jre")).toBe(false);
  });
});
