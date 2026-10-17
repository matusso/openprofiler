import { useEffect, useState } from "react";
import type { Session } from "./api/types";
import { useCurrentSession, useSessions, useSummary } from "./api/hooks";
import { CommandPalette } from "./components/CommandPalette";
import { Dialogs, useOpenFile } from "./components/Dialogs";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Inspector } from "./components/Inspector";
import { ContextMenu, Toasts } from "./components/Overlays";
import { NAV, Nav } from "./components/Nav";
import { TopBar } from "./components/TopBar";
import { formatCount, formatNanos } from "./lib/format";
import { onFileDrop } from "./lib/tauri";
import { client, queryClient, readonly, selectSession, store, useApp } from "./state";
import { CompareView } from "./views/CompareView";
import { CodeView } from "./views/CodeView";
import { CpuView } from "./views/CpuView";
import { EventsView } from "./views/EventsView";
import { HeapView } from "./views/HeapView";
import { LocksView } from "./views/LocksView";
import { MBeansView } from "./views/MBeansView";
import { MemoryView } from "./views/MemoryView";
import { OverviewView } from "./views/OverviewView";
import { ProbesView } from "./views/ProbesView";
import { ThreadsView } from "./views/ThreadsView";
import { Welcome } from "./views/Welcome";

function useServerEvents() {
  useEffect(
    () =>
      client.subscribe(
        (e) => {
          if (e.type === "sessions") queryClient.invalidateQueries({ queryKey: ["sessions"] });
          else if (e.type === "session") {
            const prev = queryClient.getQueryData<Session[]>(["sessions"])?.find((s) => s.id === e.id);
            queryClient.setQueryData<Session[]>(["sessions"], (list) =>
              list?.map((s) =>
                s.id === e.id ? { ...s, state: e.state, progress: e.progress, generation: e.generation } : s,
              ),
            );
            if (!prev || prev.state !== e.state) {
              queryClient.invalidateQueries({ queryKey: ["sessions"] });
              queryClient.invalidateQueries({ queryKey: ["s", e.id] });
            }
          } else if (e.type === "invalidate") {
            // Only active (visible) queries refetch; inactive ones are just marked stale.
            queryClient.invalidateQueries({ queryKey: ["s", e.id] });
          }
        },
        (connected) => store.set({ connected }),
      ),
    [],
  );
}

function useAutoSelect() {
  const { data } = useSessions();
  const sid = useApp((s) => s.sessionId);
  useEffect(() => {
    if (!data) return;
    if (sid && data.some((s) => s.id === sid)) return;
    const pick = data.find((s) => s.state === "live") ?? data.find((s) => s.state === "ready") ?? data[0];
    selectSession(pick?.id ?? null);
  }, [data, sid]);
}

/** Decompiler sessions only have the Code view; profiling sessions have everything else. */
function useViewForSession() {
  const s = useCurrentSession();
  const view = useApp((st) => st.view);
  useEffect(() => {
    if (!s) return;
    if (s.kind === "dex" && view !== "code") store.set({ view: "code" });
    else if (s.kind !== "dex" && view === "code") store.set({ view: "overview" });
  }, [s, view]);
}

function useShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement;
      const typing = target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        store.set({ paletteOpen: !store.get().paletteOpen });
      } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "o" && !readonly) {
        e.preventDefault();
        store.set({ dialog: "open" });
      } else if (!typing && !e.metaKey && !e.ctrlKey && !e.altKey) {
        const nav = NAV.find((n) => n.key === e.key);
        if (nav) store.set({ view: nav.id });
        else if (e.key === "/") {
          const search = document.querySelector<HTMLInputElement>("[data-view-search]");
          if (search) {
            e.preventDefault();
            search.focus();
          }
        } else if (e.key === "Escape" && store.get().range) store.set({ range: null });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

function useDrop() {
  const open = useOpenFile();
  const [over, setOver] = useState(false);
  useEffect(() => {
    if (readonly) return;
    let un: (() => void) | undefined;
    onFileDrop((paths) => paths[0] && open.mutate(paths[0]), setOver).then((u) => (un = u));
    // Browser fallback: accept dropped text paths.
    const dragOver = (e: DragEvent) => {
      if (e.dataTransfer?.types.includes("text/plain") || e.dataTransfer?.types.includes("Files")) {
        e.preventDefault();
        setOver(true);
      }
    };
    const leave = (e: DragEvent) => {
      if (!e.relatedTarget) setOver(false);
    };
    const drop = (e: DragEvent) => {
      setOver(false);
      const text = e.dataTransfer?.getData("text/plain")?.trim();
      e.preventDefault();
      if (text && text.startsWith("/")) open.mutate(text.replace(/^file:\/\//, ""));
      else if (e.dataTransfer?.files.length) store.set({ dialog: "open" });
    };
    window.addEventListener("dragover", dragOver);
    window.addEventListener("dragleave", leave);
    window.addEventListener("drop", drop);
    return () => {
      un?.();
      window.removeEventListener("dragover", dragOver);
      window.removeEventListener("dragleave", leave);
      window.removeEventListener("drop", drop);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return over;
}

function StatusBar() {
  const s = useCurrentSession();
  const connected = useApp((st) => st.connected);
  const summary = useSummary(s && s.state !== "loading" && s.state !== "failed" ? s.id : null);
  const total = summary.data ? Object.values(summary.data.sample_counts).reduce((a, b) => a + b, 0) : 0;
  return (
    <footer className="statusbar" role="status">
      <span>
        <span className={`dot${connected ? " on" : ""}`} />
        {connected ? "Connected" : "Disconnected"}
      </span>
      {s && <span>{s.path ?? s.name}</span>}
      {summary.data && summary.data.end > summary.data.start && (
        <span>{formatNanos(summary.data.end - summary.data.start)}</span>
      )}
      {total > 0 && <span>{formatCount(total)} samples</span>}
      {summary.data && summary.data.threads > 0 && <span>{summary.data.threads} threads</span>}
      <span className="grow" />
      {s && <span>gen {s.generation}</span>}
      <span>⌘K commands · 1–0 views · / search</span>
    </footer>
  );
}

function Main() {
  const view = useApp((s) => s.view);
  const s = useCurrentSession();
  if (!s && view !== "compare") return <Welcome />;
  if (s && s.state === "loading")
    return (
      <div className="empty">
        <h2>Loading {s.name}</h2>
        <div className="progress" aria-label="Loading progress">
          <span style={{ width: `${Math.round((s.progress ?? 0) * 100)}%` }} />
        </div>
        <div>{s.progress != null ? `${Math.round(s.progress * 100)} %` : "Indexing…"}</div>
      </div>
    );
  if (s && s.state === "failed")
    return (
      <div className="empty">
        <h2>Failed to open {s.name}</h2>
        <div className="error">{s.error}</div>
      </div>
    );
  switch (view) {
    case "overview":
      return <OverviewView />;
    case "cpu":
      return <CpuView />;
    case "memory":
      return <MemoryView />;
    case "heap":
      return <HeapView />;
    case "threads":
      return <ThreadsView />;
    case "locks":
      return <LocksView />;
    case "events":
      return <EventsView />;
    case "code":
      return <CodeView />;
    case "compare":
      return <CompareView />;
    case "probes":
      return <ProbesView />;
    case "mbeans":
      return <MBeansView />;
  }
}

function MainBoundary() {
  const view = useApp((s) => s.view);
  const sid = useApp((s) => s.sessionId);
  return (
    <ErrorBoundary key={`${view}:${sid}`}>
      <Main />
    </ErrorBoundary>
  );
}

export function App() {
  const theme = useApp((s) => s.theme);
  const palette = useApp((s) => s.palette);
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.palette = palette;
  }, [theme, palette]);
  useServerEvents();
  useAutoSelect();
  useViewForSession();
  useShortcuts();
  const dropping = useDrop();
  return (
    <div className="app">
      <Nav />
      <TopBar />
      <main className="main" id="main">
        <MainBoundary />
      </main>
      <Inspector />
      <StatusBar />
      <CommandPalette />
      <Dialogs />
      <ContextMenu />
      <Toasts />
      {dropping && <div className="dropzone">Drop to open</div>}
    </div>
  );
}
