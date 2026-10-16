import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState } from "react";
import type { CodeClassResponse, CodeLink, CodeNodeDto, CodeTextMatch } from "../api/types";
import { sk, useCurrentSession } from "../api/hooks";
import { IconChevron, IconDownload, IconSearch } from "../components/icons";
import {
  findMatches,
  identAt,
  languageOf,
  lineAt,
  lineSegments,
  lineStarts,
  splitClassName,
  tokenize,
  type Language,
  type Mark,
} from "../lib/code";
import { formatBytes, formatCount } from "../lib/format";
import {
  client,
  codeHistory,
  openCode,
  openContextMenu,
  store,
  toast,
  useApp,
  type CodeLocation,
} from "../state";

const LINE_HEIGHT = 18;

// ---------------------------------------------------------------- code lines

interface CodeLinesProps {
  code: string;
  lang: Language;
  links?: CodeLink[];
  nodes?: Record<string, CodeNodeDto>;
  /** Offset to scroll to and highlight (definition or usage). */
  focus?: number;
  find?: string;
  findIndex?: number;
  onFindCount?: (n: number) => void;
  onLink?: (node: CodeNodeDto, decl: boolean) => void;
  onUsages?: (node: CodeNodeDto) => void;
}

function CodeLines({
  code,
  lang,
  links,
  nodes,
  focus,
  find,
  findIndex = 0,
  onFindCount,
  onLink,
  onUsages,
}: CodeLinesProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const starts = useMemo(() => lineStarts(code), [code]);
  const tokens = useMemo(() => tokenize(code, lang), [code, lang]);
  const matches = useMemo(() => findMatches(code, find ?? ""), [code, find]);
  useEffect(() => onFindCount?.(matches.length), [matches.length, onFindCount]);
  const current = matches.length
    ? matches[((findIndex % matches.length) + matches.length) % matches.length]
    : null;
  const marks = useMemo(() => {
    const m: Mark[] = (links ?? []).map(([start, len, node, decl]) => ({
      start,
      end: start + len,
      link: { node, decl: decl === 1 },
    }));
    for (const x of matches) m.push(x === current ? { ...x, focus: true } : x);
    if (focus != null && focus >= 0) {
      const len = Math.max(1, identAt(code, focus));
      m.push({ start: focus, end: focus + len, focus: true });
    }
    return m.sort((a, b) => a.start - b.start || a.end - b.end);
  }, [links, matches, current, focus, code]);

  const virt = useVirtualizer({
    count: starts.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => LINE_HEIGHT,
    overscan: 30,
  });
  const target = current?.start ?? focus;
  useEffect(() => {
    if (target == null || target < 0) return;
    virt.scrollToIndex(lineAt(starts, target), { align: "center" });
  }, [target, starts, virt]);

  const gutter = String(starts.length).length;
  const node = (id: number) => nodes?.[String(id)];
  return (
    <div className="code-scroll mono" ref={scrollRef} tabIndex={0} aria-label="Source code">
      <div style={{ height: virt.getTotalSize(), position: "relative", minWidth: "100%" }}>
        {virt.getVirtualItems().map((v) => {
          const ls = starts[v.index];
          const le = v.index + 1 < starts.length ? starts[v.index + 1] - 1 : code.length;
          const segs = lineSegments(ls, le, tokens, marks);
          return (
            <div
              key={v.index}
              className="code-line"
              style={{ transform: `translateY(${v.start}px)`, height: LINE_HEIGHT }}
            >
              <span className="code-gutter" style={{ width: `${gutter + 2}ch` }}>
                {v.index + 1}
              </span>
              <span className="code-text">
                {segs.map((s) => {
                  const cls = [
                    s.kind ? `tk-${s.kind}` : "",
                    s.mark?.link ? "code-link" : "",
                    s.mark?.link?.decl ? "decl" : "",
                    s.match ? "code-match" : "",
                    s.focus ? "code-focus" : "",
                  ]
                    .filter(Boolean)
                    .join(" ");
                  const text = code.slice(s.start, s.end);
                  const n = s.mark?.link ? node(s.mark.link.node) : undefined;
                  if (!n)
                    return cls ? (
                      <span key={s.start} className={cls}>
                        {text}
                      </span>
                    ) : (
                      text
                    );
                  const decl = s.mark!.link!.decl;
                  return (
                    <span
                      key={s.start}
                      className={cls}
                      title={`${n.kind} ${n.full}${decl ? " · click for usages" : ""}`}
                      onClick={() => onLink?.(n, decl)}
                      onContextMenu={(e) =>
                        openContextMenu(e, [
                          { label: "Go to declaration", run: () => onLink?.(n, false) },
                          { label: "Find usages", run: () => onUsages?.(n) },
                          {
                            label: "Copy qualified name",
                            run: () => void navigator.clipboard?.writeText(n.full),
                          },
                        ])
                      }
                    >
                      {text}
                    </span>
                  );
                })}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- side bar

/** Keep the Java/Smali choice when opening another class from the tree. */
function currentCodeView(): "java" | "smali" {
  const c = store.get().code;
  return c?.type === "class" ? c.view : "java";
}

function PackageNode({
  sid,
  name,
  full,
  count,
  depth,
  selected,
}: {
  sid: string;
  name: string;
  full: string;
  count: number;
  depth: number;
  selected?: string;
}) {
  // Expand packages on the path to the open class.
  const onPath = !!selected && selected.startsWith(full + ".");
  const [open, setOpen] = useState(onPath);
  useEffect(() => {
    if (onPath) setOpen(true);
  }, [onPath]);
  return (
    <>
      <button
        className="code-tree-item"
        style={{ paddingLeft: 8 + depth * 14 }}
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        title={full}
      >
        <span className={`tree-toggle${open ? " open" : ""}`}>
          <IconChevron />
        </span>
        <span className="code-icon pkg">P</span>
        <span className="text">{name}</span>
        <span className="faint code-count">{formatCount(count)}</span>
      </button>
      {open && <PackageChildren sid={sid} parent={full} depth={depth + 1} selected={selected} />}
    </>
  );
}

function PackageChildren({
  sid,
  parent,
  depth,
  selected,
}: {
  sid: string;
  parent: string;
  depth: number;
  selected?: string;
}) {
  const q = useQuery({
    queryKey: sk(sid, "code", "packages", parent),
    queryFn: () => client.codePackages(sid, parent),
    staleTime: Infinity,
  });
  if (q.isLoading)
    return (
      <div style={{ paddingLeft: 8 + depth * 14 + 20 }} className="code-tree-loading">
        <div className="spinner" />
      </div>
    );
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  return (
    <>
      {q.data!.packages.map((p) => (
        <PackageNode
          key={p.full}
          sid={sid}
          name={p.name}
          full={p.full}
          count={p.classes}
          depth={depth}
          selected={selected}
        />
      ))}
      {q.data!.classes.map((c) => (
        <button
          key={c.full}
          className="code-tree-item"
          style={{ paddingLeft: 8 + depth * 14 + 16 }}
          aria-selected={selected === c.full}
          onClick={() => openCode({ type: "class", cls: c.full, view: currentCodeView() })}
          title={c.full}
        >
          <span className={`code-icon ${c.kind}`}>
            {c.kind === "interface" ? "I" : c.kind === "enum" ? "E" : c.kind === "annotation" ? "@" : "C"}
          </span>
          <span className="text">{c.name}</span>
        </button>
      ))}
    </>
  );
}

function SearchResults({ sid, query, inCode }: { sid: string; query: string; inCode: boolean }) {
  const q = useQuery({
    queryKey: sk(sid, "code", "search", query, inCode),
    queryFn: () => client.codeSearch(sid, { query, scope: inCode ? "code" : "names", limit: 300 }),
    staleTime: Infinity,
    enabled: query.length >= (inCode ? 3 : 2),
  });
  if (query.length < (inCode ? 3 : 2))
    return <div className="faint code-hint">Type {inCode ? 3 : 2} or more characters.</div>;
  if (q.isLoading)
    return (
      <div className="empty">
        <div className="spinner" />
        {inCode && <div>Decompiling and searching… large apps can take a while.</div>}
      </div>
    );
  if (q.isError) return <div className="error">{String(q.error.message)}</div>;
  const r = q.data!;
  return (
    <>
      <div className="faint code-hint">
        {r.results.length} result{r.results.length === 1 ? "" : "s"}
        {r.truncated ? ` (stopped after ${formatCount(r.scanned)} of ${formatCount(r.total)} classes)` : ""}
      </div>
      {r.results.map((x, i) =>
        "id" in x ? (
          <button
            key={`n${x.id}`}
            className="code-tree-item"
            onClick={() => x.cls && openCode({ type: "class", cls: x.cls, view: "java", node: x.id })}
            title={x.full}
          >
            <span className={`code-icon ${x.kind}`}>
              {x.kind === "method" ? "m" : x.kind === "field" ? "f" : "C"}
            </span>
            <span className="text">
              {x.kind === "class" ? x.full : `${x.name}`}
              {x.kind !== "class" && <span className="faint"> · {x.cls}</span>}
            </span>
          </button>
        ) : (
          <TextMatchRow key={`t${i}`} m={x} />
        ),
      )}
    </>
  );
}

function TextMatchRow({ m }: { m: CodeTextMatch }) {
  return (
    <button
      className="code-tree-item code-match-row"
      onClick={() => openCode({ type: "class", cls: m.cls, view: "java", pos: m.pos })}
      title={`${m.cls}:${m.line}`}
    >
      <span className="code-match-loc faint">
        {splitClassName(m.cls).name}:{m.line}
      </span>
      <span className="text mono">{m.snippet}</span>
    </button>
  );
}

function ClassesSide({ sid, selected }: { sid: string; selected?: string }) {
  const [query, setQuery] = useState("");
  const [debounced, setDebounced] = useState("");
  const [inCode, setInCode] = useState(false);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(query.trim()), 250);
    return () => clearTimeout(t);
  }, [query]);
  return (
    <>
      <div className="code-side-tools">
        <div className="code-search">
          <IconSearch width={13} />
          <input
            className="input grow"
            data-view-search
            placeholder={inCode ? "Search decompiled code…" : "Search classes, methods, fields…"}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setQuery("")}
            aria-label="Search code"
          />
        </div>
        <label className="check" title="Search the text of decompiled classes (slower)">
          <input type="checkbox" checked={inCode} onChange={(e) => setInCode(e.target.checked)} />
          In code
        </label>
      </div>
      <div className="code-side-list">
        {debounced ? (
          <SearchResults sid={sid} query={debounced} inCode={inCode} />
        ) : (
          <PackageChildren sid={sid} parent="" depth={0} selected={selected} />
        )}
      </div>
    </>
  );
}

function ResourcesSide({ sid, selected }: { sid: string; selected?: string }) {
  const [filter, setFilter] = useState("");
  const q = useQuery({
    queryKey: sk(sid, "code", "resources"),
    queryFn: () => client.codeResources(sid),
    staleTime: Infinity,
  });
  const rows = (q.data?.resources ?? []).filter((r) => r.name.toLowerCase().includes(filter.toLowerCase()));
  return (
    <>
      <div className="code-side-tools">
        <div className="code-search">
          <IconSearch width={13} />
          <input
            className="input grow"
            placeholder="Filter resources…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            aria-label="Filter resources"
          />
        </div>
      </div>
      <div className="code-side-list">
        {q.isLoading && (
          <div className="empty">
            <div className="spinner" />
          </div>
        )}
        {q.isError && <div className="error">{String(q.error.message)}</div>}
        {q.data && rows.length === 0 && <div className="faint code-hint">No resources.</div>}
        {rows.map((r) => (
          <button
            key={r.name}
            className="code-tree-item"
            aria-selected={selected === r.name || selected?.startsWith(r.name + "!")}
            onClick={() => openCode({ type: "resource", name: r.name })}
            title={r.name}
          >
            <span className="badge code-res-type">{r.type}</span>
            <span className="text">{r.name}</span>
            {r.size != null && <span className="faint code-count">{formatBytes(r.size)}</span>}
          </button>
        ))}
      </div>
    </>
  );
}

// ---------------------------------------------------------------- main panes

function download(name: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function HistoryButtons() {
  const back = useApp((s) => s.codeBack.length);
  const fwd = useApp((s) => s.codeForward.length);
  return (
    <span className="seg">
      <button disabled={!back} onClick={() => codeHistory(-1)} title="Back (Alt+←)" aria-label="Back">
        ←
      </button>
      <button disabled={!fwd} onClick={() => codeHistory(1)} title="Forward (Alt+→)" aria-label="Forward">
        →
      </button>
    </span>
  );
}

function FindBox({
  value,
  onChange,
  count,
  onStep,
}: {
  value: string;
  onChange: (v: string) => void;
  count: number;
  onStep: (d: number) => void;
}) {
  return (
    <span className="code-find">
      <input
        className="input"
        placeholder="Find in file"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") onStep(e.shiftKey ? -1 : 1);
          if (e.key === "Escape") onChange("");
        }}
        aria-label="Find in file"
      />
      {value && <span className="faint">{count ? `${count} found` : "no match"}</span>}
    </span>
  );
}

function UsagesPanel({ sid, node, onClose }: { sid: string; node: CodeNodeDto; onClose: () => void }) {
  const q = useQuery({
    queryKey: sk(sid, "code", "usages", node.id),
    queryFn: () => client.codeUsages(sid, node.id),
    staleTime: Infinity,
  });
  return (
    <div className="code-usages">
      <div className="toolbar">
        <strong>Usages of</strong>
        <span className="mono">{node.full}</span>
        {q.data && (
          <span className="faint">
            {q.data.usages.length}
            {q.data.truncated ? "+" : ""} found
          </span>
        )}
        <span className="grow" />
        <button className="btn ghost" onClick={onClose}>
          Close
        </button>
      </div>
      <div className="code-side-list">
        {q.isLoading && (
          <div className="empty">
            <div className="spinner" />
          </div>
        )}
        {q.isError && <div className="error">{String(q.error.message)}</div>}
        {q.data && q.data.usages.length === 0 && <div className="faint code-hint">No usages found.</div>}
        {q.data?.usages.map((u, i) => (
          <button
            key={i}
            className="code-tree-item code-match-row"
            onClick={() => openCode({ type: "class", cls: u.cls, view: "java", pos: u.pos })}
            title={`${u.cls}:${u.line}`}
          >
            <span className="code-match-loc faint">
              {splitClassName(u.cls).name}
              {u.in ? `.${u.in}` : ""}:{u.line}
            </span>
            <span className="text mono">{u.snippet}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

function ClassPane({
  sid,
  loc,
  onUsages,
}: {
  sid: string;
  loc: Extract<CodeLocation, { type: "class" }>;
  onUsages: (n: CodeNodeDto) => void;
}) {
  const [find, setFind] = useState("");
  const [findIndex, setFindIndex] = useState(0);
  const [findCount, setFindCount] = useState(0);
  const summary = useQuery({
    queryKey: sk(sid, "code", "summary"),
    queryFn: () => client.codeSummary(sid),
    staleTime: Infinity,
  });
  // The low-level view is smali for Android code and JVM bytecode for class files.
  const jvm = summary.data?.format === "jvm";
  const q = useQuery<CodeClassResponse>({
    queryKey: sk(sid, "code", "class", loc.cls, loc.view, loc.view === "java" ? (loc.node ?? null) : null),
    queryFn: () =>
      client.codeClass(
        sid,
        loc.node != null && loc.view === "java"
          ? { node: loc.node, view: "java" }
          : { name: loc.cls, view: loc.view },
      ),
    staleTime: Infinity,
    placeholderData: keepPreviousData,
  });
  useEffect(() => setFindIndex(0), [find]);
  const data = q.data;
  const stale = q.isPlaceholderData;
  const { pkg, name } = splitClassName(data && !stale ? data.name : loc.cls);
  const focus = loc.view === "java" ? (loc.pos ?? (data && !stale ? data.focus : undefined)) : undefined;
  const setView = (view: "java" | "smali") => openCode({ type: "class", cls: data?.name ?? loc.cls, view });
  const onLink = (n: CodeNodeDto, decl: boolean) => {
    if (decl) onUsages(n);
    else if (n.cls) openCode({ type: "class", cls: n.cls, view: "java", node: n.id });
  };
  return (
    <div className="code-main">
      <div className="toolbar code-toolbar">
        <HistoryButtons />
        <span className="code-title" title={data?.name ?? loc.cls}>
          {pkg && <span className="faint">{pkg}.</span>}
          <strong>{name}</strong>
        </span>
        {q.isFetching && <div className="spinner" />}
        <span className="grow" />
        <FindBox
          value={find}
          onChange={setFind}
          count={findCount}
          onStep={(d) => setFindIndex((i) => i + d)}
        />
        <span className="seg" role="group" aria-label="Code view">
          <button aria-pressed={loc.view === "java"} onClick={() => setView("java")}>
            Java
          </button>
          <button aria-pressed={loc.view === "smali"} onClick={() => setView("smali")}>
            {jvm ? "Bytecode" : "Smali"}
          </button>
        </span>
        <button
          className="btn"
          disabled={!data}
          title="Copy source"
          onClick={() =>
            data &&
            navigator.clipboard
              ?.writeText(data.code)
              .then(() => toast("Source copied"))
              .catch(() => toast("Clipboard not available", "warning"))
          }
        >
          Copy
        </button>
        <button
          className="btn icon"
          disabled={!data}
          title="Download source"
          aria-label="Download source"
          onClick={() =>
            data && download(`${name}.${loc.view === "smali" && jvm ? "bytecode.txt" : loc.view}`, data.code)
          }
        >
          <IconDownload />
        </button>
      </div>
      {q.isError ? (
        <div className="error">{String(q.error.message)}</div>
      ) : !data ? (
        <div className="empty">
          <div className="spinner" />
          <div>Decompiling {name}…</div>
        </div>
      ) : (
        <CodeLines
          code={data.code}
          lang={data.view}
          links={data.links}
          nodes={data.nodes}
          focus={stale ? undefined : focus}
          find={find}
          findIndex={findIndex}
          onFindCount={setFindCount}
          onLink={onLink}
          onUsages={onUsages}
        />
      )}
    </div>
  );
}

const IMAGE_MIME: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
};

function ResourcePane({ sid, loc }: { sid: string; loc: Extract<CodeLocation, { type: "resource" }> }) {
  const [find, setFind] = useState("");
  const [findIndex, setFindIndex] = useState(0);
  const [findCount, setFindCount] = useState(0);
  const q = useQuery({
    queryKey: sk(sid, "code", "resource", loc.name),
    queryFn: () => client.codeResource(sid, loc.name),
    staleTime: Infinity,
  });
  const [base, sub] = loc.name.split("!");
  const display = sub ?? base;
  const ext = display.split(".").pop()?.toLowerCase() ?? "";
  const r = q.data;
  return (
    <div className="code-main">
      <div className="toolbar code-toolbar">
        <HistoryButtons />
        <span className="code-title" title={loc.name}>
          {sub && <span className="faint">{base} › </span>}
          <strong>{display}</strong>
        </span>
        {q.isFetching && <div className="spinner" />}
        <span className="grow" />
        {r?.files && r.files.length > 0 && (
          <select
            className="select"
            value=""
            onChange={(e) =>
              e.target.value && openCode({ type: "resource", name: `${base}!${e.target.value}` })
            }
            aria-label="Decoded files"
          >
            <option value="">Decoded files ({r.files.length})…</option>
            {r.files.map((f) => (
              <option key={f} value={f}>
                {f}
              </option>
            ))}
          </select>
        )}
        {r?.text != null && (
          <>
            <FindBox
              value={find}
              onChange={setFind}
              count={findCount}
              onStep={(d) => setFindIndex((i) => i + d)}
            />
            <button
              className="btn icon"
              title="Download"
              aria-label="Download"
              onClick={() => download(display.split("/").pop() ?? "resource.txt", r.text!)}
            >
              <IconDownload />
            </button>
          </>
        )}
      </div>
      {q.isError ? (
        <div className="error">{String(q.error.message)}</div>
      ) : !r ? (
        <div className="empty">
          <div className="spinner" />
        </div>
      ) : r.image ? (
        <div className="code-image">
          <img src={`data:${IMAGE_MIME[ext] ?? "image/png"};base64,${r.image}`} alt={display} />
        </div>
      ) : r.text != null ? (
        <CodeLines
          code={r.text}
          lang={languageOf(display)}
          find={find}
          findIndex={findIndex}
          onFindCount={setFindCount}
        />
      ) : (
        <div className="empty">
          <h2>{r.binary ? "Binary file" : "No preview"}</h2>
          <p>{r.type} resource</p>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- view

export function CodeView() {
  const s = useCurrentSession()!;
  const loc = useApp((st) => st.code);
  const side = useApp((st) => st.codeSide);
  const [usagesOf, setUsagesOf] = useState<CodeNodeDto | null>(null);
  const enabled = s.capabilities.includes("decompile");
  const summary = useQuery({
    queryKey: sk(s.id, "code", "summary"),
    queryFn: () => client.codeSummary(s.id),
    enabled,
  });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.altKey || e.metaKey || e.ctrlKey) return;
      if (e.key === "ArrowLeft") codeHistory(-1);
      else if (e.key === "ArrowRight") codeHistory(1);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  if (!enabled)
    return (
      <div className="empty">
        <h2>No code to decompile</h2>
        <p>
          Open an Android app (<code>.apk</code>, <code>.xapk</code>, <code>.aab</code>), a <code>.dex</code>{" "}
          file or Java bytecode (<code>.jar</code>, <code>.war</code>, <code>.class</code>) to browse it as
          Java source.
        </p>
      </div>
    );
  const sm = summary.data;
  const selectedClass = loc?.type === "class" ? loc.cls : undefined;
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Code</h1>
        {sm && (
          <span className="muted">
            {formatCount(sm.classes)} classes · {formatCount(sm.packages)} packages ·{" "}
            {formatCount(sm.resources)} resources
            {sm.format !== "jvm" && (
              <>
                {" "}
                · {sm.dex_files.length} dex {sm.dex_files.length === 1 ? "file" : "files"}
              </>
            )}
          </span>
        )}
        {sm && sm.warnings.length > 0 && (
          <span className="badge warning" title={sm.warnings.join("\n")}>
            {sm.warnings.length} load {sm.warnings.length === 1 ? "warning" : "warnings"}
          </span>
        )}
        {summary.isError && <span className="error">{String(summary.error.message)}</span>}
      </div>
      <div className="code-layout">
        <aside className="code-side" aria-label="Classes and resources">
          <div className="tabs" role="tablist" style={{ marginTop: 0 }}>
            <button
              role="tab"
              className="tab"
              aria-selected={side === "classes"}
              onClick={() => store.set({ codeSide: "classes" })}
            >
              Classes
            </button>
            <button
              role="tab"
              className="tab"
              aria-selected={side === "resources"}
              onClick={() => store.set({ codeSide: "resources" })}
            >
              Resources
            </button>
          </div>
          {side === "classes" ? (
            <ClassesSide sid={s.id} selected={selectedClass} />
          ) : (
            <ResourcesSide sid={s.id} selected={loc?.type === "resource" ? loc.name : undefined} />
          )}
        </aside>
        <div className={`code-center${usagesOf ? " with-usages" : ""}`}>
          {loc?.type === "class" ? (
            <ClassPane key={loc.cls} sid={s.id} loc={loc} onUsages={setUsagesOf} />
          ) : loc?.type === "resource" ? (
            <ResourcePane key={loc.name} sid={s.id} loc={loc} />
          ) : (
            <div className="empty">
              <h2>Pick a class</h2>
              <p>
                Browse packages on the left, or press <kbd>/</kbd> to search classes, methods and fields.
                <br />
                Click a reference to jump to its declaration; click a declaration to list its usages.
              </p>
              {sm && <p className="faint">{sm.engine}</p>}
            </div>
          )}
          {usagesOf && <UsagesPanel sid={s.id} node={usagesOf} onClose={() => setUsagesOf(null)} />}
        </div>
      </div>
    </div>
  );
}
