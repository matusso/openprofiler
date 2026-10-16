import { describe, expect, it } from "vitest";
import {
  findMatches,
  identAt,
  languageOf,
  lineAt,
  lineSegments,
  lineStarts,
  tokenizeJava,
  tokenizeSmali,
  tokenizeXml,
  type Token,
} from "./code";

const text = (code: string, toks: Token[]) => toks.map(([s, e, k]) => `${k}:${code.slice(s, e)}`);

describe("lines", () => {
  it("finds line starts and the line of an offset", () => {
    const starts = lineStarts("ab\ncd\n\nx");
    expect(starts).toEqual([0, 3, 6, 7]);
    expect(lineAt(starts, 0)).toBe(0);
    expect(lineAt(starts, 4)).toBe(1);
    expect(lineAt(starts, 6)).toBe(2);
    expect(lineAt(starts, 99)).toBe(3);
  });
});

describe("tokenizeJava", () => {
  it("highlights keywords, types, strings, numbers, annotations and comments", () => {
    const code = '@Override public int f() { return 42 + "a\\"b".length(); } // done';
    expect(text(code, tokenizeJava(code))).toEqual([
      "ann:@Override",
      "kw:public",
      "type:int",
      "kw:return",
      "num:42",
      'str:"a\\"b"',
      "com:// done",
    ]);
  });
  it("splits block comments at line breaks", () => {
    const code = "/* a\n b */ class X {}";
    expect(text(code, tokenizeJava(code))).toEqual(["com:/* a", "com: b */", "kw:class"]);
  });
});

describe("tokenizeSmali", () => {
  it("highlights directives, registers, types and comments", () => {
    const code =
      '.method public run()V\n    const-string v0, "hi" # note\n    invoke-static {p0}, La/B;->c()V';
    const t = text(code, tokenizeSmali(code));
    expect(t).toContain("dir:.method");
    expect(t).toContain("reg:v0");
    expect(t).toContain("reg:p0");
    expect(t).toContain('str:"hi"');
    expect(t).toContain("com:# note");
    expect(t).toContain("type:La/B;");
  });
});

describe("tokenizeXml", () => {
  it("highlights tags, attributes and values", () => {
    const code = '<manifest package="a.b"><!-- c --><app/></manifest>';
    expect(text(code, tokenizeXml(code))).toEqual([
      "tag:<manifest",
      "attr:package",
      'str:"a.b"',
      "tag:>",
      "com:<!-- c -->",
      "tag:<app",
      "tag:/>",
      "tag:</manifest",
      "tag:>",
    ]);
  });
});

describe("lineSegments", () => {
  it("merges tokens, links and matches into constant segments", () => {
    const code = "return foo.bar;";
    const tokens = tokenizeJava(code);
    const link = { start: 11, end: 14, link: { node: 7, decl: false } };
    const match = { start: 8, end: 12, match: true };
    const segs = lineSegments(0, code.length, tokens, [match, link]);
    const view = segs.map((s) => [
      code.slice(s.start, s.end),
      s.kind ?? "",
      s.mark?.link?.node ?? "",
      s.match ? "m" : "",
    ]);
    expect(view).toEqual([
      ["return", "kw", "", ""],
      [" f", "", "", ""],
      ["oo.", "", "", "m"],
      ["b", "", 7, "m"],
      ["ar", "", 7, ""],
      [";", "", "", ""],
    ]);
  });
  it("only uses ranges on the requested line", () => {
    const code = "int a;\nint b;";
    const segs = lineSegments(7, 13, tokenizeJava(code), []);
    expect(segs.map((s) => code.slice(s.start, s.end))).toEqual(["int", " b;"]);
  });
});

describe("helpers", () => {
  it("finds matches case-insensitively", () => {
    expect(findMatches("Key key KEY", "key").map((m) => m.start)).toEqual([0, 4, 8]);
    expect(findMatches("abc", "")).toEqual([]);
  });
  it("measures identifiers and guesses languages", () => {
    expect(identAt("foo(bar)", 0)).toBe(3);
    expect(identAt("foo(bar)", 3)).toBe(0);
    expect(languageOf("AndroidManifest.xml")).toBe("xml");
    expect(languageOf("resources.arsc")).toBe("xml");
    expect(languageOf("assets/config.json")).toBe("text");
  });
});
