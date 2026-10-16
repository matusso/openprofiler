import type { ReactNode } from "react";
import type { Capability } from "../api/types";
import { useCurrentSession } from "../api/hooks";
import { store, useApp, type ViewId } from "../state";
import {
  OpenProfilerLogo,
  IconBean,
  IconCode,
  IconCompare,
  IconCpu,
  IconEvents,
  IconHeap,
  IconLock,
  IconMemory,
  IconOverview,
  IconProbe,
  IconThreads,
} from "./icons";

export interface NavEntry {
  id: ViewId;
  label: string;
  short: string;
  icon: ReactNode;
  key: string;
  /** View is enabled when the session has any of these capabilities (empty = always). */
  needs: Capability[];
}

export const NAV: NavEntry[] = [
  { id: "overview", label: "Overview", short: "Overview", icon: <IconOverview />, key: "1", needs: [] },
  {
    id: "cpu",
    label: "CPU",
    short: "CPU",
    icon: <IconCpu />,
    key: "2",
    needs: ["cpu", "wall", "io", "exception"],
  },
  {
    id: "memory",
    label: "Memory (allocations)",
    short: "Memory",
    icon: <IconMemory />,
    key: "3",
    needs: ["alloc"],
  },
  { id: "heap", label: "Heap walker", short: "Heap", icon: <IconHeap />, key: "4", needs: ["heap"] },
  {
    id: "threads",
    label: "Threads",
    short: "Threads",
    icon: <IconThreads />,
    key: "5",
    needs: ["threads", "thread_dumps"],
  },
  {
    id: "locks",
    label: "Monitors & locks",
    short: "Locks",
    icon: <IconLock />,
    key: "6",
    needs: ["monitor", "wait"],
  },
  { id: "probes", label: "Probes", short: "Probes", icon: <IconProbe />, key: "7", needs: [] },
  {
    id: "events",
    label: "Events",
    short: "Events",
    icon: <IconEvents />,
    key: "8",
    needs: ["gc", "telemetry"],
  },
  { id: "mbeans", label: "MBeans", short: "MBeans", icon: <IconBean />, key: "9", needs: ["mbeans"] },
  {
    id: "code",
    label: "Code (decompiler)",
    short: "Code",
    icon: <IconCode />,
    key: "j",
    needs: ["decompile"],
  },
  { id: "compare", label: "Compare", short: "Compare", icon: <IconCompare />, key: "0", needs: [] },
];

export function Nav() {
  const view = useApp((s) => s.view);
  const s = useCurrentSession();
  const caps = s?.capabilities ?? [];
  return (
    <nav className="nav" aria-label="Views">
      <OpenProfilerLogo className="nav-logo" />
      {NAV.map((n) => {
        // Decompiler sessions carry no profiling data: only the Code view applies.
        const enabled =
          s?.kind === "dex" ? n.id === "code" : n.needs.length === 0 || n.needs.some((c) => caps.includes(c));
        return (
          <button
            key={n.id}
            className="nav-item"
            aria-current={view === n.id ? "page" : undefined}
            title={`${n.label} (${n.key})`}
            disabled={!enabled && !!s}
            onClick={() => store.set({ view: n.id })}
          >
            {n.icon}
            <span>{n.short}</span>
          </button>
        );
      })}
      <span className="nav-spacer" />
    </nav>
  );
}
