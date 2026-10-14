import { keepPreviousData, useQuery } from "@tanstack/react-query";
import type { Session, TelemetryRequest } from "./types";
import { client, useApp } from "../state";

/** Query keys are scoped by session so `invalidate` events can refetch only that session's views. */
export const sk = (id: string | null | undefined, ...rest: unknown[]) => ["s", id ?? "-", ...rest];

export function useSessions() {
  return useQuery({ queryKey: ["sessions"], queryFn: client.sessions, refetchInterval: 15_000 });
}

export function useCurrentSession(): Session | undefined {
  const id = useApp((s) => s.sessionId);
  const { data } = useSessions();
  return data?.find((s) => s.id === id);
}

export function useSummary(id: string | null) {
  return useQuery({
    queryKey: sk(id, "summary"),
    queryFn: () => client.summary(id!),
    enabled: !!id,
  });
}

export function useTelemetry(id: string | null, req: TelemetryRequest, enabled = true) {
  return useQuery({
    queryKey: sk(id, "telemetry", req),
    queryFn: () => client.telemetry(id!, req),
    enabled: !!id && enabled,
    placeholderData: keepPreviousData,
  });
}

export function useThreads(id: string | null, enabled = true) {
  return useQuery({
    queryKey: sk(id, "threads"),
    queryFn: () => client.threads(id!),
    enabled: !!id && enabled,
  });
}

export function useHas(cap: string): boolean {
  const s = useCurrentSession();
  return !!s?.capabilities.includes(cap as Session["capabilities"][number]);
}
