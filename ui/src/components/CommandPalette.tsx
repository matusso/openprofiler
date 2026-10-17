import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { sk, useCurrentSession, useSessions } from "../api/hooks";
import {
  client,
  focusMethod,
  openCode,
  readonly,
  selectSession,
  setPalette,
  setTheme,
  store,
  useApp,
  type ViewId,
} from "../state";
import { NAV } from "./Nav";

export interface PaletteItem {
  id: string;
  group: string;
  label: string;
  hint?: string;
  run: () => void;
}

/** Simple fuzzy score: all query chars in order; bonus for contiguous and word-start matches. */
export function fuzzyScore(text: string, q: string): number {
  if (!q) return 1;
  const t = text.toLowerCase();
  const s = q.toLowerCase();
  const direct = t.indexOf(s);
  if (direct >= 0) return 1000 - direct;
  let ti = 0;
  let score = 0;
  let last = -2;
  for (const ch of s) {
    const i = t.indexOf(ch, ti);
    if (i < 0) return 0;
    score += i === last + 1 ? 5 : 1;
    if (i === 0 || ".$_ ".includes(t[i - 1])) score += 3;
    last = i;
    ti = i + 1;
  }
  return score;
}

export function CommandPalette() {
  const open = useApp((s) => s.paletteOpen);
  const theme = useApp((s) => s.theme);
  const palette = useApp((s) => s.palette);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const current = useCurrentSession();
  const { data: sessions } = useSessions();
  const sid = current?.id ?? null;
  const caps = current?.capabilities ?? [];

  const methods = useQuery({
    queryKey: sk(sid, "palette-hotspots"),
    queryFn: () => client.hotspots(sid!, { kind: "cpu", limit: 5000 }),
    enabled: open && !!sid && caps.includes("cpu") && q.length >= 2,
    staleTime: 60_000,
  });
  const classes = useQuery({
    queryKey: sk(sid, "palette-classes", q),
    queryFn: () => client.heapClasses(sid!, { filter: q, limit: 30, sort: "retained" }),
    enabled: open && !!sid && caps.includes("heap") && q.length >= 2,
  });
  const code = useQuery({
    queryKey: sk(sid, "palette-code", q),
    queryFn: () => client.codeSearch(sid!, { query: q, scope: "names", limit: 30 }),
    enabled: open && !!sid && caps.includes("decompile") && q.length >= 2,
    staleTime: 60_000,
  });

  useEffect(() => {
    if (open) {
      setQ("");
      setSel(0);
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  const close = () => store.set({ paletteOpen: false });

  const items = useMemo(() => {
    const go = (view: ViewId) => () => store.set({ view });
    const base: PaletteItem[] = [
      ...NAV.map((n) => ({
        id: `view-${n.id}`,
        group: "Views",
        label: `Go to ${n.label}`,
        hint: n.key,
        run: go(n.id),
      })),
      ...(readonly
        ? []
        : [
            {
              id: "open",
              group: "Actions",
              label: "Open file…",
              hint: "⌘O",
              run: () => store.set({ dialog: "open" }),
            },
            {
              id: "attach",
              group: "Actions",
              label: "Attach to a JVM…",
              run: () => store.set({ dialog: "attach" }),
            },
          ]),
      {
        id: "clear-range",
        group: "Actions",
        label: "Clear time range",
        run: () => store.set({ range: null }),
      },
      {
        id: "theme",
        group: "Actions",
        label: `Theme: switch to ${theme === "dark" ? "light" : theme === "light" ? "high contrast" : "dark"}`,
        run: () => setTheme(theme === "dark" ? "light" : theme === "light" ? "contrast" : "dark"),
      },
      {
        id: "palette",
        group: "Actions",
        label: palette === "cb" ? "Use default palette" : "Use colour-blind-safe palette",
        run: () => setPalette(palette === "cb" ? "default" : "cb"),
      },
      {
        id: "inspector",
        group: "Actions",
        label: "Toggle inspector panel",
        run: () => store.set({ inspectorOpen: !store.get().inspectorOpen }),
      },
    ];
    if (current?.state === "live") {
      base.push(
        {
          id: "tdump",
          group: "Live",
          label: "Capture thread dump",
          run: () => void client.captureThreadDump(current.id),
        },
        { id: "gc", group: "Live", label: "Run garbage collector", run: () => void client.gc(current.id) },
      );
    }
    for (const s of sessions ?? []) {
      base.push({
        id: `session-${s.id}`,
        group: "Sessions",
        label: `Switch to ${s.name}`,
        hint: s.kind,
        run: () => selectSession(s.id),
      });
    }
    const scored = base
      .map((it) => ({ it, s: fuzzyScore(it.label, q) }))
      .filter((x) => x.s > 0)
      .sort((a, b) => b.s - a.s)
      .map((x) => x.it);
    if (q.length >= 2 && methods.data) {
      const m = methods.data.hotspots
        .map((h) => ({ h, s: fuzzyScore(h.label, q) }))
        .filter((x) => x.s > 0)
        .sort((a, b) => b.s - a.s || b.h.total - a.h.total)
        .slice(0, 25)
        .map(({ h }): PaletteItem => ({
          id: `m-${h.key}`,
          group: "Methods",
          label: h.label,
          hint: "cumulated calls",
          run: () => focusMethod("cpu", "cpu", h.key, h.label, "callees"),
        }));
      scored.push(...m);
    }
    if (q.length >= 2 && classes.data) {
      for (const c of classes.data.rows) {
        if (c.class_id === undefined) continue;
        scored.push({
          id: `c-${c.class_id}`,
          group: "Heap classes",
          label: c.name,
          hint: `${c.instances} instances`,
          run: async () => {
            const set = await client.heapSelect(sid!, { step: { class_id: c.class_id! } });
            store.set({ view: "heap", heapSet: set, heapTrail: [set] });
          },
        });
      }
    }
    if (q.length >= 2 && code.data) {
      for (const r of code.data.results) {
        if (!("id" in r) || !r.cls) continue;
        const cls = r.cls;
        scored.push({
          id: `code-${r.id}`,
          group: "Decompiled code",
          label: r.full,
          hint: r.kind,
          run: () => openCode({ type: "class", cls, view: "java", node: r.id }),
        });
      }
    }
    return scored.slice(0, 80);
  }, [q, sessions, current, methods.data, classes.data, code.data, theme, palette, sid]);

  if (!open) return null;

  const run = (it: PaletteItem | undefined) => {
    if (!it) return;
    close();
    it.run();
  };

  let lastGroup = "";
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className="dialog" role="dialog" aria-modal="true" aria-label="Command palette">
        <input
          ref={inputRef}
          className="palette-input"
          placeholder="Type a command, view, method or class…"
          value={q}
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={items[sel] ? `pi-${items[sel].id}` : undefined}
          onChange={(e) => {
            setQ(e.target.value);
            setSel(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") close();
            else if (e.key === "ArrowDown") {
              setSel((s) => Math.min(items.length - 1, s + 1));
              e.preventDefault();
            } else if (e.key === "ArrowUp") {
              setSel((s) => Math.max(0, s - 1));
              e.preventDefault();
            } else if (e.key === "Enter") run(items[sel]);
          }}
        />
        <div className="palette-list" id="palette-list" role="listbox">
          {items.length === 0 && (
            <div className="faint" style={{ padding: 12 }}>
              No matches
            </div>
          )}
          {items.map((it, i) => {
            const header = it.group !== lastGroup ? <div className="palette-group">{it.group}</div> : null;
            lastGroup = it.group;
            return (
              <div key={it.id}>
                {header}
                <div
                  id={`pi-${it.id}`}
                  className="palette-item"
                  role="option"
                  aria-selected={i === sel}
                  onMouseEnter={() => setSel(i)}
                  onClick={() => run(it)}
                >
                  <span
                    className={it.group === "Methods" || it.group === "Heap classes" ? "mono" : undefined}
                  >
                    {it.label}
                  </span>
                  {it.hint && <span className="kbd">{it.hint}</span>}
                </div>
              </div>
            );
          })}
          {q.length >= 2 && methods.isFetching && (
            <div className="faint" style={{ padding: 8 }}>
              Searching methods…
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
