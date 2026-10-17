import type { RemoteHop } from "../api/types";

/** Default image of the ephemeral debug container (needs `sh`, `uname` and `sha256sum`). */
export const DEFAULT_DEBUG_IMAGE = "busybox:1.37";

/** Parses `[user@]host[:port]` like the daemon's `Hop::ssh`; `null` when invalid. */
export function parseSshSpec(spec: string): Extract<RemoteHop, { type: "ssh" }> | null {
  const s = spec.trim();
  if (!s) return null;
  const at = s.lastIndexOf("@");
  const user = at >= 0 ? s.slice(0, at) : undefined;
  const rest = at >= 0 ? s.slice(at + 1) : s;
  let host = rest;
  let port: number | undefined;
  const colon = rest.lastIndexOf(":");
  // A single colon separates the port; more than one is an IPv6 address without a port.
  if (colon >= 0 && rest.indexOf(":") === colon) {
    host = rest.slice(0, colon);
    const p = rest.slice(colon + 1);
    if (!/^\d+$/.test(p) || Number(p) > 65535) return null;
    port = Number(p);
  }
  if (!host || (user !== undefined && !user)) return null;
  return { type: "ssh", host, ...(user ? { user } : {}), ...(port !== undefined ? { port } : {}) };
}

/** Whether an image looks shell-less (distroless, Chainguard), so it needs a debug container. */
export function looksDistroless(image: string): boolean {
  return /distroless|chainguard|cgr\.dev/i.test(image);
}

/** localStorage access that tolerates disabled storage. */
export function remember(key: string, value?: string): string | null {
  try {
    if (value !== undefined) localStorage.setItem(`openprofiler.remote.${key}`, value);
    return localStorage.getItem(`openprofiler.remote.${key}`);
  } catch {
    return null;
  }
}
