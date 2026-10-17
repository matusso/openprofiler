import { QueryClient } from "@tanstack/react-query";
import { OpenProfilerClient, resolveConfig } from "./api/client";
import type {
  Aggregation,
  Filter,
  Insight,
  ObjectSetDto,
  ThreadDto,
  ThreadStateName,
  TreeKind,
  TreeNodeDto,
} from "./api/types";
import { createStore } from "./lib/store";

export type ViewId =
  | "overview"
  | "cpu"
  | "memory"
  | "heap"
  | "threads"
  | "locks"
  | "probes"
  | "methods"
  | "events"
  | "mbeans"
  | "code"
  | "compare";

/** What the decompiler view shows: a class (optionally at a node or offset) or a resource file. */
export type CodeLocation =
  | { type: "class"; cls: string; view: "java" | "smali"; node?: number; pos?: number }
  | { type: "resource"; name: string };

export type Theme = "dark" | "light" | "contrast";

export type InspectorItem =
  | { type: "node"; kind: TreeKind; node: TreeNodeDto; total: number; unit: string }
  | { type: "object"; id: number }
  | { type: "thread"; thread: ThreadDto }
  | { type: "insight"; insight: Insight };

/** Request to jump to a method/class in a tree view (from the command palette). */
export interface FocusRequest {
  view: ViewId;
  kind: TreeKind;
  key: number;
  label: string;
  /** callees = cumulated outgoing calls, callers = back traces. */
  mode: "callees" | "callers";
  nonce: number;
}

let focusNonce = 0;

export function focusMethod(
  view: ViewId,
  kind: TreeKind,
  key: number,
  label: string,
  mode: FocusRequest["mode"],
) {
  store.set({ view, focus: { view, kind, key, label, mode, nonce: ++focusNonce } });
}

export interface Toast {
  id: number;
  text: string;
  tone: "info" | "warning" | "error";
}

export interface MenuItem {
  label: string;
  run: () => void;
  disabled?: boolean;
}

export interface AppState {
  sessionId: string | null;
  view: ViewId;
  /** Global time range brushed in the scrubber; applies to every view. */
  range: [number, number] | null;
  theme: Theme;
  palette: "default" | "cb";
  inspector: InspectorItem | null;
  inspectorOpen: boolean;
  paletteOpen: boolean;
  dialog: null | "open" | "attach" | "share";
  threads: number[] | null;
  states: ThreadStateName[] | null;
  aggregation: Aggregation;
  collapseRecursion: boolean;
  compactJava: boolean;
  heapSet: ObjectSetDto | null;
  heapTrail: ObjectSetDto[];
  heapObject: number | null;
  heapTab: "classes" | "objects" | "biggest" | "references" | "inspections";
  heapInspection: string | null;
  compareA: string | null;
  compareB: string | null;
  code: CodeLocation | null;
  codeBack: CodeLocation[];
  codeForward: CodeLocation[];
  codeSide: "classes" | "resources";
  focus: FocusRequest | null;
  connected: boolean;
  toasts: Toast[];
  contextMenu: { x: number; y: number; items: MenuItem[] } | null;
}

export const client = new OpenProfilerClient(resolveConfig());

/** Opened through a read-only share link: no attach, open, recording or JVM control. */
export const readonly = !!client.config.readonly;

const storedTheme = (typeof localStorage !== "undefined" &&
  localStorage.getItem("openprofiler.theme")) as Theme | null;
const storedPalette = (typeof localStorage !== "undefined" &&
  localStorage.getItem("openprofiler.palette")) as "default" | "cb" | null;

export const store = createStore<AppState>({
  sessionId: client.config.session ?? null,
  view: "overview",
  range: null,
  theme:
    storedTheme ??
    (typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark"),
  palette: storedPalette ?? "default",
  inspector: null,
  inspectorOpen: true,
  paletteOpen: false,
  dialog: null,
  threads: null,
  states: null,
  aggregation: "method",
  collapseRecursion: false,
  compactJava: false,
  heapSet: null,
  heapTrail: [],
  heapObject: null,
  heapTab: "classes",
  heapInspection: null,
  compareA: null,
  compareB: null,
  code: null,
  codeBack: [],
  codeForward: [],
  codeSide: "classes",
  focus: null,
  connected: false,
  toasts: [],
  contextMenu: null,
});

let toastSeq = 0;

export function toast(text: string, tone: Toast["tone"] = "info", ms = 4500) {
  const id = ++toastSeq;
  store.set((s) => ({ toasts: [...s.toasts, { id, text, tone }] }));
  setTimeout(() => store.set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })), ms);
}

export function openContextMenu(
  e: { clientX: number; clientY: number; preventDefault: () => void },
  items: MenuItem[],
) {
  e.preventDefault();
  store.set({ contextMenu: { x: e.clientX, y: e.clientY, items } });
}

export const useApp = store.use;

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 30_000, retry: 1, refetchOnWindowFocus: false },
  },
});

/** The global filter shared by all tree-like views. */
export function currentFilter(s: AppState): Filter {
  const f: Filter = {};
  if (s.range) f.range = s.range;
  if (s.threads?.length) f.threads = s.threads;
  if (s.states?.length) f.states = s.states;
  return f;
}

export function selectSession(id: string | null) {
  store.set({
    sessionId: id,
    range: null,
    threads: null,
    states: null,
    inspector: null,
    heapSet: null,
    heapTrail: [],
    heapObject: null,
    code: null,
    codeBack: [],
    codeForward: [],
  });
}

/** Opens a location in the decompiler view, recording history for back/forward. */
export function openCode(loc: CodeLocation) {
  store.set((s) => ({
    view: "code",
    code: loc,
    codeBack: s.code ? [...s.codeBack.slice(-99), s.code] : s.codeBack,
    codeForward: [],
    codeSide: loc.type === "resource" ? "resources" : s.codeSide,
  }));
}

export function codeHistory(dir: -1 | 1) {
  store.set((s) => {
    const from = dir < 0 ? s.codeBack : s.codeForward;
    if (!from.length || !s.code) return {};
    const target = from[from.length - 1];
    const rest = from.slice(0, -1);
    return dir < 0
      ? { code: target, codeBack: rest, codeForward: [...s.codeForward, s.code] }
      : { code: target, codeForward: rest, codeBack: [...s.codeBack, s.code] };
  });
}

export function setTheme(theme: Theme) {
  localStorage.setItem("openprofiler.theme", theme);
  store.set({ theme });
}

export function setPalette(palette: "default" | "cb") {
  localStorage.setItem("openprofiler.palette", palette);
  store.set({ palette });
}

/** Opens an insight's target view (heap inspections) or shows it in the inspector. */
export function openInsight(insight: Insight) {
  const m = insight.view?.match(/^heap\.inspection:(.+)$/);
  if (m) store.set({ view: "heap", heapTab: "inspections", heapInspection: m[1] });
  inspect({ type: "insight", insight });
}

export function inspect(item: InspectorItem | null) {
  store.set({ inspector: item, inspectorOpen: item ? true : store.get().inspectorOpen });
}
