import type { Aggregation, NavigateRequest } from "../api/types";
import { client, toast } from "../state";

/**
 * Derives an IDE navigation target from a frame. Prefers explicit class/method; otherwise parses a
 * `pkg.Class.method(sig)` label according to the aggregation level. Packages are not navigable.
 */
export function sourceTarget(
  label: string,
  aggregation: Aggregation = "method",
  cls?: string,
  method?: string,
): NavigateRequest | null {
  if (cls) return method ? { class: cls, method } : { class: cls };
  if (aggregation === "package") return null;
  const base = label.split("(")[0].trim();
  if (!base || base.startsWith("<") || base === "All threads") return null;
  if (aggregation === "class") return { class: base };
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return null;
  return { class: base.slice(0, dot), method: base.slice(dot + 1) };
}

export async function goToSource(target: NavigateRequest | null) {
  if (!target) {
    toast("This frame has no source location.", "warning");
    return;
  }
  try {
    const r = await client.ideNavigate(target);
    const name = target.method ? `${target.class}.${target.method}` : target.class;
    if (!r.delivered)
      toast(
        "No IDE connected. Install the OpenProfiler plugin for IntelliJ or VS Code to jump to source.",
        "warning",
      );
    else toast(`Opened ${name} in your IDE`);
  } catch (e) {
    toast(`Go to source failed: ${(e as Error).message}`, "error");
  }
}
