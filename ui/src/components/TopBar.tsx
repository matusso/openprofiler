import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import type { Session } from "../api/types";
import { useCurrentSession, useSessions, useSummary } from "../api/hooks";
import {
  client,
  queryClient,
  readonly,
  selectSession,
  setPalette,
  setTheme,
  store,
  useApp,
  type Theme,
} from "../state";
import {
  IconBookmark,
  IconCamera,
  IconChevron,
  IconClose,
  IconContrast,
  IconMoon,
  IconOpen,
  IconPanel,
  IconPlug,
  IconSearch,
  IconShare,
  IconSun,
  IconTrash,
} from "./icons";
import { TimeScrubber } from "./TimeScrubber";

const KIND_LABEL: Record<Session["kind"], string> = {
  hprof: "HPROF",
  jfr: "JFR",
  snapshot: "Snapshot",
  live: "Live",
  dex: "Decompiler",
};

export function SessionBadge({ s }: { s: Session }) {
  if (s.state === "loading")
    return (
      <span className="badge accent">
        loading {s.progress != null ? `${Math.round(s.progress * 100)}%` : ""}
      </span>
    );
  if (s.state === "failed") return <span className="badge critical">failed</span>;
  if (s.state === "live") return <span className="badge live">● live</span>;
  if (s.state === "detached") return <span className="badge warning">detached</span>;
  return <span className="badge">{KIND_LABEL[s.kind]}</span>;
}

function SessionPicker() {
  const { data: sessions } = useSessions();
  const current = useCurrentSession();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const version = useQuery({ queryKey: ["version"], queryFn: client.version, staleTime: Infinity });
  const web = !!version.data?.web;
  useEffect(() => {
    if (!open) return;
    const h = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    window.addEventListener("mousedown", h);
    return () => window.removeEventListener("mousedown", h);
  }, [open]);
  const close = useMutation({
    mutationFn: (id: string) => client.close(id),
    onSuccess: (_r, id) => {
      if (store.get().sessionId === id) selectSession(null);
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
    },
  });
  return (
    <div ref={ref} style={{ position: "relative", minWidth: 0 }}>
      <button
        className="btn"
        style={{ maxWidth: 380 }}
        onClick={() => setOpen(!open)}
        aria-haspopup="listbox"
        aria-expanded={open}
      >
        {current ? (
          <>
            <SessionBadge s={current} />
            <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>{current.name}</span>
          </>
        ) : (
          <span className="muted">No session</span>
        )}
        <IconChevron style={{ transform: "rotate(90deg)", width: 12 }} />
      </button>
      {open && (
        <div className="menu" role="listbox" aria-label="Sessions">
          {(sessions ?? []).length === 0 && (
            <div className="faint" style={{ padding: 10 }}>
              No open sessions
            </div>
          )}
          {sessions?.map((s) => (
            <div key={s.id} style={{ display: "flex", alignItems: "center" }}>
              <button
                className="menu-item"
                role="option"
                aria-selected={s.id === current?.id}
                onClick={() => {
                  selectSession(s.id);
                  setOpen(false);
                }}
                title={s.path}
              >
                <SessionBadge s={s} />
                <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>{s.name}</span>
              </button>
              {!readonly && (
                <button
                  className="btn ghost icon"
                  aria-label={`Close ${s.name}`}
                  title="Close session"
                  onClick={() => close.mutate(s.id)}
                >
                  <IconTrash />
                </button>
              )}
            </div>
          ))}
          {!readonly && (
            <>
              <div style={{ borderTop: "1px solid var(--border)", margin: "4px 0" }} />
              <button className="menu-item" onClick={() => (setOpen(false), store.set({ dialog: "open" }))}>
                <IconOpen width={15} /> Open file…
              </button>
              <button className="menu-item" onClick={() => (setOpen(false), store.set({ dialog: "attach" }))}>
                <IconPlug width={15} /> Attach to JVM…
              </button>
              {web && current && (
                <button
                  className="menu-item"
                  onClick={() => (setOpen(false), store.set({ dialog: "share" }))}
                >
                  <IconShare width={15} /> Share read-only link…
                </button>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}

const RECORDINGS = [
  { kind: "cpu", label: "CPU" },
  { kind: "alloc", label: "Alloc" },
  { kind: "monitor", label: "Locks" },
] as const;

function LiveControls({ s }: { s: Session }) {
  const [on, setOn] = useState<Record<string, boolean>>({ cpu: true, alloc: true, monitor: true });
  const rec = useMutation({
    mutationFn: ({ kind, enabled }: { kind: string; enabled: boolean }) =>
      client.setRecording(s.id, kind, enabled),
    onSuccess: (_r, v) => setOn((o) => ({ ...o, [v.kind]: v.enabled })),
  });
  const heap = useMutation({
    mutationFn: (recordedOnly: boolean) =>
      client.heapDump(s.id, recordedOnly ? { dumper: "native", recorded_only: true } : undefined),
    onSuccess: (ns) => {
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      selectSession(ns.id);
      store.set({ view: "heap" });
    },
  });
  const dump = useMutation({
    mutationFn: () => client.captureThreadDump(s.id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["s", s.id, "thread-dumps"] }),
  });
  const mark = useMutation({
    mutationFn: () => client.bookmark(s.id, `Bookmark ${new Date().toLocaleTimeString()}`),
  });
  if (s.state !== "live") return null;
  return (
    <>
      <span className="sep" />
      <span className="label">Record</span>
      <div className="seg" role="group" aria-label="Recordings">
        {RECORDINGS.map((r) => (
          <button
            key={r.kind}
            aria-pressed={on[r.kind]}
            onClick={() => rec.mutate({ kind: r.kind, enabled: !on[r.kind] })}
          >
            {r.label}
          </button>
        ))}
      </div>
      <button
        className="btn ghost icon"
        title="Add bookmark"
        aria-label="Add bookmark"
        onClick={() => mark.mutate()}
      >
        <IconBookmark />
      </button>
      <button className="btn ghost" title="Capture thread dump" onClick={() => dump.mutate()}>
        <IconCamera /> Thread dump
      </button>
      <button
        className="btn ghost"
        title="Heap dump"
        disabled={heap.isPending}
        onClick={() => heap.mutate(false)}
      >
        {heap.isPending && !heap.variables ? <span className="spinner" /> : null} Heap dump
      </button>
      <button
        className="btn ghost"
        title="Heap dump of the objects recorded by allocation recording, with their paths from the GC roots"
        disabled={heap.isPending || !on.alloc}
        onClick={() => heap.mutate(true)}
      >
        {heap.isPending && heap.variables ? <span className="spinner" /> : null} Recorded objects
      </button>
      <button className="btn ghost" title="Run garbage collector" onClick={() => client.gc(s.id)}>
        Run GC
      </button>
    </>
  );
}

const THEMES: { t: Theme; icon: React.ReactNode; label: string }[] = [
  { t: "dark", icon: <IconMoon />, label: "Dark theme" },
  { t: "light", icon: <IconSun />, label: "Light theme" },
  { t: "contrast", icon: <IconContrast />, label: "High-contrast theme" },
];

export function TopBar() {
  const current = useCurrentSession();
  const theme = useApp((s) => s.theme);
  const palette = useApp((s) => s.palette);
  const inspectorOpen = useApp((s) => s.inspectorOpen);
  const summary = useSummary(
    current && current.state !== "loading" && current.state !== "failed" ? current.id : null,
  );
  const next = THEMES[(THEMES.findIndex((x) => x.t === theme) + 1) % THEMES.length];
  const showScrubber =
    summary.data && current && !(current.kind === "hprof") && summary.data.end > summary.data.start;
  return (
    <header className="topbar">
      <div className="topbar-row">
        <SessionPicker />
        {readonly ? (
          <span className="badge info" title="Opened through a read-only share link">
            Read-only link
          </span>
        ) : (
          <>
            <button className="btn ghost" onClick={() => store.set({ dialog: "open" })}>
              <IconOpen /> Open
            </button>
            <button className="btn ghost" onClick={() => store.set({ dialog: "attach" })}>
              <IconPlug /> Attach
            </button>
            {current && <LiveControls s={current} />}
          </>
        )}
        <span className="grow" />
        <button
          className="btn"
          style={{ minWidth: 200, justifyContent: "flex-start", color: "var(--fg-faint)" }}
          onClick={() => store.set({ paletteOpen: true })}
          aria-label="Open command palette"
        >
          <IconSearch /> Search or run a command <span className="kbd">⌘K</span>
        </button>
        <button
          className={`btn ghost icon${palette === "cb" ? " on" : ""}`}
          title="Colour-blind-safe palette"
          aria-label="Toggle colour-blind-safe palette"
          aria-pressed={palette === "cb"}
          onClick={() => setPalette(palette === "cb" ? "default" : "cb")}
        >
          <span style={{ fontSize: 11, fontWeight: 700 }}>CB</span>
        </button>
        <button
          className="btn ghost icon"
          title={next.label}
          aria-label={next.label}
          onClick={() => setTheme(next.t)}
        >
          {THEMES.find((x) => x.t === theme)?.icon}
        </button>
        <button
          className={`btn ghost icon${inspectorOpen ? " on" : ""}`}
          title="Toggle inspector"
          aria-label="Toggle inspector"
          aria-pressed={inspectorOpen}
          onClick={() => store.set({ inspectorOpen: !inspectorOpen })}
        >
          <IconPanel />
        </button>
      </div>
      {showScrubber && <TimeScrubber summary={summary.data!} />}
    </header>
  );
}

export function CloseIcon() {
  return <IconClose />;
}
