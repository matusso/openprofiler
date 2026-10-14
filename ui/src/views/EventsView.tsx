import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import type { BookmarkDto, EventRow, GcEventDto } from "../api/types";
import { sk, useCurrentSession, useSummary, useTelemetry } from "../api/hooks";
import { DataTable } from "../components/DataTable";
import { formatBytes, formatCount, formatNanos, formatRelTime } from "../lib/format";
import { client, store, useApp } from "../state";
import { SearchBox } from "./profile/shared";

export function EventsView() {
  const s = useCurrentSession()!;
  const range = useApp((st) => st.range);
  const summary = useSummary(s.id);
  const tel = useTelemetry(s.id, { keys: [], range: range ?? undefined, points: 10 });
  const types = useQuery({
    queryKey: sk(s.id, "event-types", range),
    queryFn: () => client.events(s.id, { range: range ?? undefined, limit: 0 }),
    retry: false,
  });
  const hasBrowser = !types.isError;
  const [tabSel, setTab] = useState<"browser" | "gc" | "bookmarks">("browser");
  const tab = tabSel === "browser" && !hasBrowser ? "gc" : tabSel;
  const [sort, setSort] = useState("time");
  const origin = summary.data?.start ?? 0;
  const gc = useMemo(() => {
    const r = [...(tel.data?.gc ?? [])];
    if (sort === "duration") r.sort((a, b) => b.duration - a.duration);
    else if (sort === "freed")
      r.sort(
        (a, b) => (b.heap_before ?? 0) - (b.heap_after ?? 0) - ((a.heap_before ?? 0) - (a.heap_after ?? 0)),
      );
    return r;
  }, [tel.data, sort]);
  const focus = (t: number, d: number) => store.set({ range: [t - 1e9, t + Math.max(d, 0) + 1e9] });
  return (
    <div className="view">
      <div className="view-header">
        <h1 className="view-title">Events</h1>
        <span className="faint">Double-click an event to focus all views on its time window.</span>
      </div>
      <div className="tabs" role="tablist">
        {hasBrowser && (
          <button
            role="tab"
            className="tab"
            aria-selected={tab === "browser"}
            onClick={() => setTab("browser")}
          >
            Event browser ({formatCount(types.data?.types.reduce((a, t) => a + t.count, 0) ?? 0)})
          </button>
        )}
        <button role="tab" className="tab" aria-selected={tab === "gc"} onClick={() => setTab("gc")}>
          Garbage collections ({tel.data?.gc.length ?? 0})
        </button>
        <button
          role="tab"
          className="tab"
          aria-selected={tab === "bookmarks"}
          onClick={() => setTab("bookmarks")}
        >
          Bookmarks ({tel.data?.bookmarks.length ?? 0})
        </button>
      </div>
      <div className="view-body">
        {tab === "browser" ? (
          <EventBrowser
            sessionId={s.id}
            origin={origin}
            types={types.data?.types ?? []}
            loading={types.isLoading}
          />
        ) : tab === "gc" ? (
          <DataTable<GcEventDto>
            label="Garbage collection events"
            rows={gc}
            rowKey={(_g, i) => i}
            sort={sort}
            onSort={setSort}
            onActivate={(g) => focus(g.start, g.duration)}
            empty={<div className="empty">No GC events</div>}
            columns={[
              {
                id: "t",
                header: "Time",
                width: 90,
                sortKey: "time",
                render: (g) => formatRelTime(g.start, origin),
              },
              { id: "name", header: "Collector", width: 200, render: (g) => g.name },
              {
                id: "cause",
                header: "Cause",
                flex: true,
                render: (g) => <span className="muted">{g.cause}</span>,
              },
              {
                id: "d",
                header: "Duration",
                width: 100,
                align: "right",
                sortKey: "duration",
                render: (g) => formatNanos(g.duration),
              },
              {
                id: "p",
                header: "Pause",
                width: 100,
                align: "right",
                render: (g) => (g.pause != null ? formatNanos(g.pause) : ""),
              },
              {
                id: "b",
                header: "Before",
                width: 100,
                align: "right",
                render: (g) => (g.heap_before != null ? formatBytes(g.heap_before) : ""),
              },
              {
                id: "a",
                header: "After",
                width: 100,
                align: "right",
                render: (g) => (g.heap_after != null ? formatBytes(g.heap_after) : ""),
              },
              {
                id: "f",
                header: "Freed",
                width: 100,
                align: "right",
                sortKey: "freed",
                render: (g) =>
                  g.heap_before != null && g.heap_after != null
                    ? formatBytes(g.heap_before - g.heap_after)
                    : "",
              },
            ]}
          />
        ) : (
          <DataTable<BookmarkDto>
            label="Bookmarks"
            rows={tel.data?.bookmarks ?? []}
            rowKey={(_b, i) => i}
            onActivate={(b) => focus(b.time, 0)}
            empty={<div className="empty">No bookmarks</div>}
            columns={[
              { id: "t", header: "Time", width: 100, render: (b) => formatRelTime(b.time, origin) },
              { id: "l", header: "Label", flex: true, render: (b) => b.label },
            ]}
          />
        )}
      </div>
    </div>
  );
}

/** Renders a JFR field value: `{timestamp}`, `{duration}`, nested objects and arrays. */
export function formatField(name: string, v: unknown, origin: number): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "number") {
    if (/size|bytes|weight|allocated|heap|used|committed|reserved/i.test(name) && Math.abs(v) >= 1024)
      return formatBytes(v);
    return Number.isInteger(v) ? v.toLocaleString("en-US") : v.toFixed(3);
  }
  if (typeof v === "string" || typeof v === "boolean") return String(v);
  if (Array.isArray(v)) return `[${v.map((x) => formatField(name, x, origin)).join(", ")}]`;
  const o = v as Record<string, unknown>;
  if (typeof o.duration === "number" && Object.keys(o).length === 1) return formatNanos(o.duration);
  if (typeof o.timestamp === "number" && Object.keys(o).length === 1)
    return formatRelTime(o.timestamp, origin);
  for (const k of ["name", "javaName", "osName", "description", "type"])
    if (typeof o[k] === "string") return o[k] as string;
  return JSON.stringify(o);
}

const PAGE = 500;

function EventBrowser({
  sessionId,
  origin,
  types,
  loading,
}: {
  sessionId: string;
  origin: number;
  types: { name: string; count: number }[];
  loading: boolean;
}) {
  const range = useApp((s) => s.range);
  const [type, setType] = useState<string | null>(null);
  const [typeFilter, setTypeFilter] = useState("");
  const [search, setSearch] = useState("");
  const [sel, setSel] = useState<number | null>(null);
  const current = type ?? types[0]?.name ?? null;
  const req = { type: current ?? undefined, range: range ?? undefined, search: search || undefined };
  const q = useInfiniteQuery({
    queryKey: sk(sessionId, "events", req),
    queryFn: ({ pageParam }) => client.events(sessionId, { ...req, offset: pageParam, limit: PAGE }),
    initialPageParam: 0,
    getNextPageParam: (last, pages) => {
      const n = pages.reduce((a, p) => a + p.rows.length, 0);
      return n < last.total_rows ? n : undefined;
    },
    enabled: !!current,
  });
  const rows = useMemo(() => q.data?.pages.flatMap((p) => p.rows) ?? [], [q.data]);
  const fieldNames = useMemo(() => {
    const seen = new Set<string>();
    for (const r of rows.slice(0, 200)) for (const k of Object.keys(r.fields)) seen.add(k);
    return [...seen];
  }, [rows]);
  const hasThread = rows.some((r) => r.thread);
  const hasFrame = rows.some((r) => r.top_frame);
  const hasDuration = rows.some((r) => r.duration > 0);
  const visibleTypes = types.filter((t) => t.name.toLowerCase().includes(typeFilter.toLowerCase()));
  const selected = sel !== null ? rows[sel] : null;
  return (
    <div style={{ display: "grid", gridTemplateColumns: "280px 1fr", height: "100%" }}>
      <div
        style={{
          borderRight: "1px solid var(--border)",
          display: "flex",
          flexDirection: "column",
          minHeight: 0,
        }}
      >
        <div style={{ padding: 8 }}>
          <input
            className="input"
            style={{ width: "100%" }}
            placeholder="Filter event types"
            value={typeFilter}
            onChange={(e) => setTypeFilter(e.target.value)}
            aria-label="Filter event types"
          />
        </div>
        <div className="list" role="listbox" aria-label="Event types" style={{ overflow: "auto", flex: 1 }}>
          {loading && <div className="spinner" style={{ margin: 12 }} />}
          {visibleTypes.map((t) => (
            <div
              key={t.name}
              role="option"
              aria-selected={t.name === current}
              className="list-item"
              style={{ padding: "5px 12px" }}
              onClick={() => {
                setType(t.name);
                setSel(null);
              }}
            >
              <span className="mono" style={{ overflow: "hidden", textOverflow: "ellipsis" }} title={t.name}>
                {t.name}
              </span>
              <span className="grow" />
              <span className="badge">{formatCount(t.count)}</span>
            </div>
          ))}
        </div>
      </div>
      <div style={{ display: "flex", flexDirection: "column", minWidth: 0, minHeight: 0 }}>
        <div className="toolbar">
          <span className="mono">{current}</span>
          <span className="faint">{formatCount(q.data?.pages[0]?.total_rows ?? 0)} events</span>
          <span className="grow" />
          <SearchBox value={search} onChange={setSearch} placeholder="Search fields (/)" />
        </div>
        <div style={{ flex: 1, minHeight: 0 }}>
          {q.isError ? (
            <div className="error">{String(q.error.message)}</div>
          ) : (
            <DataTable<EventRow>
              label={`${current ?? ""} events`}
              rows={rows}
              rowKey={(_r, i) => i}
              selected={sel}
              onSelect={(_r, i) => setSel(i)}
              onActivate={(r) =>
                store.set({ range: [r.start - 1e9, r.start + Math.max(r.duration, 0) + 1e9] })
              }
              totalRows={q.data?.pages[0]?.total_rows}
              onLoadMore={() => !q.isFetchingNextPage && q.hasNextPage && q.fetchNextPage()}
              empty={<div className="empty">{q.isLoading ? <div className="spinner" /> : "No events"}</div>}
              columns={[
                { id: "t", header: "Time", width: 90, render: (r) => formatRelTime(r.start, origin) },
                ...(hasDuration
                  ? [
                      {
                        id: "d",
                        header: "Duration",
                        width: 90,
                        align: "right" as const,
                        render: (r: EventRow) => (r.duration ? formatNanos(r.duration) : ""),
                      },
                    ]
                  : []),
                ...(hasThread
                  ? [
                      {
                        id: "th",
                        header: "Thread",
                        width: 160,
                        render: (r: EventRow) => r.thread ?? "",
                        title: (r: EventRow) => r.thread ?? "",
                      },
                    ]
                  : []),
                ...fieldNames.map((k) => ({
                  id: `f-${k}`,
                  header: k,
                  width: 150,
                  render: (r: EventRow) => formatField(k, r.fields[k], origin),
                  title: (r: EventRow) => formatField(k, r.fields[k], origin),
                })),
                ...(hasFrame
                  ? [
                      {
                        id: "tf",
                        header: "Top frame",
                        width: 260,
                        flex: true,
                        render: (r: EventRow) => <span className="mono">{r.top_frame ?? ""}</span>,
                      },
                    ]
                  : []),
              ]}
            />
          )}
        </div>
        {selected && (
          <div
            style={{
              borderTop: "1px solid var(--border)",
              maxHeight: 220,
              overflow: "auto",
              padding: "8px 16px",
            }}
          >
            <pre className="mono" style={{ margin: 0, whiteSpace: "pre-wrap" }}>
              {JSON.stringify(selected, null, 2)}
            </pre>
          </div>
        )}
      </div>
    </div>
  );
}
