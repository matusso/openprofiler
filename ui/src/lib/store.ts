import { useSyncExternalStore } from "react";

/** A minimal external store with selector subscriptions. */
export function createStore<S extends object>(initial: S) {
  let state = initial;
  const listeners = new Set<() => void>();
  const get = () => state;
  const set = (patch: Partial<S> | ((s: S) => Partial<S>)) => {
    const next = typeof patch === "function" ? patch(state) : patch;
    state = { ...state, ...next };
    listeners.forEach((l) => l());
  };
  const subscribe = (l: () => void) => {
    listeners.add(l);
    return () => listeners.delete(l);
  };
  function use<T>(selector: (s: S) => T): T {
    return useSyncExternalStore(subscribe, () => selector(state));
  }
  return { get, set, subscribe, use };
}
