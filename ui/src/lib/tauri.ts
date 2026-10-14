/** Thin wrappers around Tauri APIs that degrade gracefully in a plain browser. */

export const isTauri = (): boolean => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export async function pickFile(): Promise<string | null> {
  if (!isTauri()) return null;
  const { open } = await import("@tauri-apps/plugin-dialog");
  const res = await open({
    multiple: false,
    directory: false,
    filters: [
      { name: "Profiles & heap dumps", extensions: ["opsnap", "jfr", "hprof", "gz", "oplink"] },
      { name: "All files", extensions: ["*"] },
    ],
  });
  return typeof res === "string" ? res : null;
}

/** Subscribes to native file drops (Tauri only). Returns an unsubscribe function. */
export async function onFileDrop(
  cb: (paths: string[]) => void,
  hover: (over: boolean) => void,
): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  const unlisten = await getCurrentWebview().onDragDropEvent((e) => {
    const p = e.payload;
    if (p.type === "over" || p.type === "enter") hover(true);
    else if (p.type === "leave") hover(false);
    else if (p.type === "drop") {
      hover(false);
      cb(p.paths);
    }
  });
  return unlisten;
}

const RECENT_KEY = "openprofiler.recent";

export function recentPaths(): string[] {
  try {
    return JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]") as string[];
  } catch {
    return [];
  }
}

export function rememberPath(path: string) {
  const list = [path, ...recentPaths().filter((p) => p !== path)].slice(0, 10);
  localStorage.setItem(RECENT_KEY, JSON.stringify(list));
}
