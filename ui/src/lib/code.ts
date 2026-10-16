// Syntax highlighting and line segmentation for the decompiler's code viewer.
// Tokens never span lines, so each virtualized line can be rendered on its own.

export type TokenKind = "kw" | "type" | "str" | "num" | "com" | "ann" | "dir" | "reg" | "tag" | "attr";

/** `[start, end, kind]` with absolute offsets; sorted, non-overlapping, within one line. */
export type Token = [number, number, TokenKind];

export type Language = "java" | "smali" | "xml" | "text";

const JAVA_KEYWORDS = new Set(
  (
    "abstract assert break case catch class const continue default do else enum extends final finally " +
    "for goto if implements import instanceof interface native new package private protected public " +
    "return static strictfp super switch synchronized this throw throws transient try volatile while " +
    "var record yield sealed permits non-sealed true false null"
  ).split(" "),
);
const JAVA_TYPES = new Set("boolean byte char double float int long short void".split(" "));

/** Line start offsets; `starts[i]` is where line `i` (0-based) begins. */
export function lineStarts(code: string): number[] {
  const starts = [0];
  for (let i = code.indexOf("\n"); i >= 0; i = code.indexOf("\n", i + 1)) starts.push(i + 1);
  return starts;
}

/** 0-based line containing offset `pos`. */
export function lineAt(starts: number[], pos: number): number {
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid] <= pos) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** Pushes `[s, e)` split at newlines. */
function pushSplit(out: Token[], code: string, s: number, e: number, kind: TokenKind) {
  let a = s;
  for (let nl = code.indexOf("\n", a); nl >= 0 && nl < e; nl = code.indexOf("\n", a)) {
    if (nl > a) out.push([a, nl, kind]);
    a = nl + 1;
  }
  if (e > a) out.push([a, e, kind]);
}

const isIdentStart = (c: string) => /[A-Za-z_$]/.test(c);
const isIdent = (c: string) => /[A-Za-z0-9_$]/.test(c);
const isDigit = (c: string) => c >= "0" && c <= "9";

function quoted(code: string, i: number, q: string): number {
  let j = i + 1;
  while (j < code.length && code[j] !== q && code[j] !== "\n") j += code[j] === "\\" ? 2 : 1;
  return Math.min(code.length, j + 1);
}

export function tokenizeJava(code: string): Token[] {
  const out: Token[] = [];
  const n = code.length;
  let i = 0;
  while (i < n) {
    const c = code[i];
    if (c === "/" && code[i + 1] === "/") {
      const e = code.indexOf("\n", i);
      const end = e < 0 ? n : e;
      out.push([i, end, "com"]);
      i = end;
    } else if (c === "/" && code[i + 1] === "*") {
      const e = code.indexOf("*/", i + 2);
      const end = e < 0 ? n : e + 2;
      pushSplit(out, code, i, end, "com");
      i = end;
    } else if (c === '"' && code.startsWith('"""', i)) {
      const e = code.indexOf('"""', i + 3);
      const end = e < 0 ? n : e + 3;
      pushSplit(out, code, i, end, "str");
      i = end;
    } else if (c === '"' || c === "'") {
      const end = quoted(code, i, c);
      out.push([i, end, "str"]);
      i = end;
    } else if (c === "@" && isIdentStart(code[i + 1] ?? "")) {
      let j = i + 1;
      while (j < n && (isIdent(code[j]) || code[j] === ".")) j++;
      out.push([i, j, "ann"]);
      i = j;
    } else if (isDigit(c) || (c === "." && isDigit(code[i + 1] ?? ""))) {
      let j = i + 1;
      while (j < n && /[0-9a-fA-FxXlLdDfF_.]/.test(code[j])) j++;
      out.push([i, j, "num"]);
      i = j;
    } else if (isIdentStart(c)) {
      let j = i + 1;
      while (j < n && isIdent(code[j])) j++;
      const w = code.slice(i, j);
      if (JAVA_KEYWORDS.has(w)) out.push([i, j, "kw"]);
      else if (JAVA_TYPES.has(w)) out.push([i, j, "type"]);
      i = j;
    } else {
      i++;
    }
  }
  return out;
}

export function tokenizeSmali(code: string): Token[] {
  const out: Token[] = [];
  const n = code.length;
  let i = 0;
  while (i < n) {
    const c = code[i];
    const prev = i > 0 ? code[i - 1] : "\n";
    if (c === "#") {
      const e = code.indexOf("\n", i);
      const end = e < 0 ? n : e;
      out.push([i, end, "com"]);
      i = end;
    } else if (c === '"') {
      const end = quoted(code, i, c);
      out.push([i, end, "str"]);
      i = end;
    } else if (c === "." && /[\s]/.test(prev) && isIdentStart(code[i + 1] ?? "")) {
      let j = i + 1;
      while (j < n && (isIdent(code[j]) || code[j] === "-")) j++;
      out.push([i, j, "dir"]);
      i = j;
    } else if ((c === "v" || c === "p") && !isIdent(prev) && isDigit(code[i + 1] ?? "")) {
      let j = i + 1;
      while (j < n && isDigit(code[j])) j++;
      if (j < n && isIdent(code[j])) {
        i = j;
        continue;
      }
      out.push([i, j, "reg"]);
      i = j;
    } else if (c === "L" && !isIdent(prev) && /[A-Za-z_$]/.test(code[i + 1] ?? "")) {
      const e = code.indexOf(";", i);
      const nl = code.indexOf("\n", i);
      if (e > 0 && (nl < 0 || e < nl) && /^L[\w$/]+;$/.test(code.slice(i, e + 1))) {
        out.push([i, e + 1, "type"]);
        i = e + 1;
      } else i++;
    } else if ((c === "-" || isDigit(c)) && !isIdent(prev) && isDigit(c === "-" ? (code[i + 1] ?? "") : c)) {
      let j = i + 1;
      while (j < n && /[0-9a-fA-FxXlLtsS]/.test(code[j])) j++;
      out.push([i, j, "num"]);
      i = j;
    } else if (isIdentStart(c)) {
      let j = i + 1;
      while (j < n && (isIdent(code[j]) || code[j] === "-" || code[j] === "/")) j++;
      i = j;
    } else {
      i++;
    }
  }
  return out;
}

export function tokenizeXml(code: string): Token[] {
  const out: Token[] = [];
  const n = code.length;
  let i = 0;
  while (i < n) {
    if (code.startsWith("<!--", i)) {
      const e = code.indexOf("-->", i + 4);
      const end = e < 0 ? n : e + 3;
      pushSplit(out, code, i, end, "com");
      i = end;
    } else if (code[i] === "<") {
      let j = i + 1;
      if (code[j] === "/" || code[j] === "?" || code[j] === "!") j++;
      while (j < n && /[\w:.-]/.test(code[j])) j++;
      out.push([i, j, "tag"]);
      i = j;
      // Attributes until the tag closes.
      while (i < n && code[i] !== ">") {
        if (code[i] === '"' || code[i] === "'") {
          const q = code[i];
          const e = code.indexOf(q, i + 1);
          const end = e < 0 ? n : e + 1;
          pushSplit(out, code, i, end, "str");
          i = end;
        } else if (/[\w:]/.test(code[i])) {
          let k = i;
          while (k < n && /[\w:.-]/.test(code[k])) k++;
          out.push([i, k, "attr"]);
          i = k;
        } else i++;
      }
      if (i < n) {
        const s = code[i - 1] === "/" || code[i - 1] === "?" ? i - 1 : i;
        out.push([s, i + 1, "tag"]);
        i++;
      }
    } else {
      i++;
    }
  }
  // Attribute scanning can emit a closing `/>` that overlaps a preceding token; keep the order strict.
  return out.filter((t, k) => k === 0 || t[0] >= out[k - 1][1]);
}

export function tokenize(code: string, lang: Language): Token[] {
  switch (lang) {
    case "java":
      return tokenizeJava(code);
    case "smali":
      return tokenizeSmali(code);
    case "xml":
      return tokenizeXml(code);
    default:
      return [];
  }
}

export function languageOf(name: string): Language {
  const n = name.toLowerCase();
  if (n.endsWith(".xml") || n.endsWith(".arsc") || n.endsWith(".html") || n.endsWith(".svg")) return "xml";
  if (n.endsWith(".java")) return "java";
  if (n.endsWith(".smali")) return "smali";
  return "text";
}

/** A range on a line that carries extra meaning: a link to a node, or a find-in-file match. */
export interface Mark {
  start: number;
  end: number;
  link?: { node: number; decl: boolean };
  match?: boolean;
  focus?: boolean;
}

export interface Segment {
  start: number;
  end: number;
  kind?: TokenKind;
  mark?: Mark;
  match?: boolean;
  focus?: boolean;
}

/** First index in `arr` (sorted by start) whose end is after `pos`. */
function firstEndingAfter<T>(arr: T[], pos: number, end: (t: T) => number): number {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (end(arr[mid]) <= pos) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Splits line `[ls, le)` into segments where token kind, link and match state are constant.
 * `tokens` and `marks` are sorted by start; marks may overlap tokens and each other.
 */
export function lineSegments(ls: number, le: number, tokens: Token[], marks: Mark[]): Segment[] {
  const toks: Token[] = [];
  for (let k = firstEndingAfter(tokens, ls, (t) => t[1]); k < tokens.length && tokens[k][0] < le; k++)
    toks.push(tokens[k]);
  const ms: Mark[] = [];
  for (let k = firstEndingAfter(marks, ls, (m) => m.end); k < marks.length && marks[k].start < le; k++)
    if (marks[k].end > ls) ms.push(marks[k]);
  const cuts = new Set<number>([ls, le]);
  for (const t of toks) cuts.add(Math.max(ls, t[0])).add(Math.min(le, t[1]));
  for (const m of ms) cuts.add(Math.max(ls, m.start)).add(Math.min(le, m.end));
  const points = [...cuts].sort((a, b) => a - b);
  const out: Segment[] = [];
  for (let k = 0; k + 1 < points.length; k++) {
    const [a, b] = [points[k], points[k + 1]];
    if (a === b) continue;
    const seg: Segment = { start: a, end: b };
    const t = toks.find((t) => t[0] <= a && t[1] >= b);
    if (t) seg.kind = t[2];
    for (const m of ms) {
      if (m.start > a || m.end < b) continue;
      if (m.link) seg.mark = m;
      if (m.match) seg.match = true;
      if (m.focus) seg.focus = true;
    }
    out.push(seg);
  }
  return out;
}

/** Case-insensitive occurrences of `q` in `code` as marks (at most `limit`). */
export function findMatches(code: string, q: string, limit = 5000): Mark[] {
  if (!q) return [];
  const lower = code.toLowerCase();
  const needle = q.toLowerCase();
  const out: Mark[] = [];
  for (
    let i = lower.indexOf(needle);
    i >= 0 && out.length < limit;
    i = lower.indexOf(needle, i + needle.length)
  )
    out.push({ start: i, end: i + needle.length, match: true });
  return out;
}

/** Length of the identifier starting at `pos` (0 when there is none). */
export function identAt(code: string, pos: number): number {
  let i = pos;
  while (i < code.length && isIdent(code[i])) i++;
  return i - pos;
}

/** Splits a fully qualified class name into package and simple name. */
export function splitClassName(full: string): { pkg: string; name: string } {
  const i = full.lastIndexOf(".");
  return i < 0 ? { pkg: "", name: full } : { pkg: full.slice(0, i), name: full.slice(i + 1) };
}
