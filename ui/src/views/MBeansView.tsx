import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useMemo, useState } from "react";
import type {
  MBeanAttribute,
  MBeanDetail,
  MBeanOperation,
  MBeanServerInfo,
  MBeanSummary,
  TrackedAttribute,
  Unit,
} from "../api/types";
import { useCurrentSession, useSummary, useTelemetry } from "../api/hooks";
import { IconChevron, IconClose, IconSearch } from "../components/icons";
import { TimeChart } from "../components/TimeChart";
import { formatClock, formatValue } from "../lib/format";
import { beanLabel, guessUnit } from "../lib/mbeans";
import { client, store, toast, useApp } from "../state";

const UNITS: Unit[] = ["count", "bytes", "nanos", "percent", "per_second"];

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function useBeans(sid: string, server: string) {
  return useQuery({
    queryKey: ["mbeans", sid, server],
    queryFn: () => client.mbeans(sid, server),
    staleTime: 30_000,
    retry: false,
  });
}

function useTrack(sid: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: Parameters<typeof client.mbeanTrack>[1]) => client.mbeanTrack(sid, req),
    onSuccess: (r) => {
      qc.setQueriesData({ queryKey: ["mbeans", sid] }, (old: { beans: MBeanSummary[] } | undefined) =>
        old ? { ...old, tracked: r.tracked } : old,
      );
      qc.invalidateQueries({ queryKey: ["s", sid, "telemetry"] });
    },
    onError: (e) => toast(errorText(e), "error"),
  });
}

// ---------- tree ----------

function BeanTree({
  beans,
  selected,
  onSelect,
}: {
  beans: MBeanSummary[];
  selected: string | null;
  onSelect: (name: string) => void;
}) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const groups = useMemo(() => {
    const f = filter.trim().toLowerCase();
    const m = new Map<string, MBeanSummary[]>();
    for (const b of beans) {
      if (f && !b.name.toLowerCase().includes(f) && !b.class_name.toLowerCase().includes(f)) continue;
      const list = m.get(b.domain) ?? [];
      list.push(b);
      m.set(b.domain, list);
    }
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [beans, filter]);
  const toggle = (d: string) =>
    setCollapsed((c) => {
      const n = new Set(c);
      if (n.has(d)) n.delete(d);
      else n.add(d);
      return n;
    });
  return (
    <div className="code-side">
      <div className="code-side-tools">
        <div className="code-search">
          <IconSearch width={13} />
          <input
            className="input grow"
            data-view-search
            placeholder="Filter MBeans…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setFilter("")}
            aria-label="Filter MBeans"
          />
        </div>
      </div>
      <div className="code-side-list" role="tree" aria-label="MBeans">
        {groups.map(([domain, list]) => {
          const open = !!filter.trim() || !collapsed.has(domain);
          return (
            <div key={domain} role="group">
              <button
                className="code-tree-item"
                role="treeitem"
                aria-expanded={open}
                onClick={() => toggle(domain)}
                title={domain}
              >
                <IconChevron width={11} style={{ transform: open ? "rotate(90deg)" : undefined }} />
                <span className="text">{domain}</span>
                <span className="code-count">{list.length}</span>
              </button>
              {open &&
                list.map((b) => (
                  <button
                    key={b.name}
                    className="code-tree-item"
                    role="treeitem"
                    aria-selected={b.name === selected}
                    style={{ paddingLeft: 26 }}
                    onClick={() => onSelect(b.name)}
                    title={`${b.name}\n${b.class_name}`}
                  >
                    <span className="text mono">{beanLabel(b.name)}</span>
                  </button>
                ))}
            </div>
          );
        })}
        {groups.length === 0 && <div className="code-hint">No MBeans match.</div>}
      </div>
    </div>
  );
}

// ---------- values ----------

function Scalar({ v }: { v: unknown }) {
  if (v === null || v === undefined) return <span className="faint">null</span>;
  if (typeof v === "number")
    return <span className="num mono">{Number.isInteger(v) ? v.toLocaleString() : v}</span>;
  if (typeof v === "boolean") return <span className="mono">{String(v)}</span>;
  return <span className="mono mb-str">{String(v)}</span>;
}

/** A JSON value; `extra` adds controls after the top-level entries of an object. */
function Value({
  v,
  depth = 0,
  extra,
}: {
  v: unknown;
  depth?: number;
  extra?: (key: string, value: unknown) => ReactNode;
}) {
  const [open, setOpen] = useState(depth === 0);
  if (v === null || typeof v !== "object") return <Scalar v={v} />;
  const entries = Array.isArray(v) ? v.map((x, i) => [String(i), x] as const) : Object.entries(v);
  const brackets = Array.isArray(v) ? `[${v.length}]` : `{${entries.length}}`;
  if (!entries.length) return <span className="faint mono">{Array.isArray(v) ? "[]" : "{}"}</span>;
  if (!open)
    return (
      <button className="link mono" onClick={() => setOpen(true)}>
        {brackets} …
      </button>
    );
  return (
    <div className="mb-value">
      {depth > 0 && (
        <button className="link mono" onClick={() => setOpen(false)}>
          {brackets}
        </button>
      )}
      {entries.slice(0, 200).map(([k, x]) => (
        <div key={k} className="mb-entry">
          <span className="faint mono">{k}</span>
          <Value v={x} depth={depth + 1} />
          {extra?.(k, x)}
        </div>
      ))}
      {entries.length > 200 && <div className="faint">… {entries.length - 200} more</div>}
    </div>
  );
}

function AttributeEditor({
  attr,
  onSave,
  onCancel,
}: {
  attr: MBeanAttribute;
  onSave: (v: string) => void;
  onCancel: () => void;
}) {
  const initial = typeof attr.value === "string" ? attr.value : JSON.stringify(attr.value ?? "");
  const [text, setText] = useState(initial);
  if (attr.type === "boolean" || attr.type === "java.lang.Boolean")
    return (
      <span className="mb-edit">
        <select className="select" value={text} onChange={(e) => setText(e.target.value)} autoFocus>
          <option value="true">true</option>
          <option value="false">false</option>
        </select>
        <button className="btn primary" onClick={() => onSave(text)}>
          Set
        </button>
        <button className="btn ghost" onClick={onCancel}>
          Cancel
        </button>
      </span>
    );
  return (
    <span className="mb-edit">
      <input
        className="input mono grow"
        value={text}
        autoFocus
        aria-label={`New value of ${attr.name}`}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") onSave(text);
          else if (e.key === "Escape") onCancel();
        }}
      />
      <button className="btn primary" onClick={() => onSave(text)}>
        Set
      </button>
      <button className="btn ghost" onClick={onCancel}>
        Cancel
      </button>
    </span>
  );
}

function Attributes({
  sid,
  server,
  bean,
  attrs,
  tracked,
}: {
  sid: string;
  server: string;
  bean: string;
  attrs: MBeanAttribute[];
  tracked: TrackedAttribute[];
}) {
  const qc = useQueryClient();
  const track = useTrack(sid);
  const [editing, setEditing] = useState<string | null>(null);
  const set = useMutation({
    mutationFn: (r: { attribute: string; value: string }) =>
      client.mbeanSet(sid, { server, name: bean, ...r }),
    onSuccess: () => {
      setEditing(null);
      qc.invalidateQueries({ queryKey: ["mbean", sid, server, bean] });
    },
    onError: (e) => toast(errorText(e), "error"),
  });
  const isTracked = (attribute: string, item?: string) =>
    tracked.some(
      (t) =>
        t.server === server &&
        t.name === bean &&
        t.attribute === attribute &&
        (t.item ?? "") === (item ?? ""),
    );
  const plotButton = (attribute: string, item?: string) => {
    const on = isTracked(attribute, item);
    return (
      <button
        className={`btn ghost mb-btn${on ? " on" : ""}`}
        aria-pressed={on}
        aria-label={`Plot ${attribute}${item ? `.${item}` : ""}`}
        title={on ? "Stop plotting" : "Plot as telemetry (sampled once per second; also shown in Overview)"}
        onClick={() =>
          track.mutate({
            server,
            name: bean,
            attribute,
            item,
            unit: guessUnit(attribute, item),
            enabled: !on,
          })
        }
      >
        Plot
      </button>
    );
  };
  if (!attrs.length) return <div className="faint">No attributes.</div>;
  return (
    <div className="mb-table" role="table" aria-label="Attributes">
      {attrs.map((a) => {
        const scalar = !a.error && typeof a.value === "number";
        return (
          <div key={a.name} className="mb-row" role="row">
            <div className="mb-name" role="cell" title={a.description || a.name}>
              <span className="mono">{a.name}</span>
              <span className="faint mono mb-type">{a.type}</span>
            </div>
            <div className="mb-val" role="cell">
              {editing === a.name ? (
                <AttributeEditor
                  attr={a}
                  onCancel={() => setEditing(null)}
                  onSave={(value) => set.mutate({ attribute: a.name, value })}
                />
              ) : a.error ? (
                <span className="mb-error">{a.error}</span>
              ) : !a.readable ? (
                <span className="faint">write-only</span>
              ) : (
                <Value
                  v={a.value}
                  extra={(k, x) => typeof x === "number" && !Array.isArray(a.value) && plotButton(a.name, k)}
                />
              )}
            </div>
            <div className="mb-actions" role="cell">
              {scalar && plotButton(a.name)}
              {a.writable && editing !== a.name && (
                <button className="btn ghost mb-btn" onClick={() => setEditing(a.name)}>
                  Edit
                </button>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

function Operation({
  sid,
  server,
  bean,
  op,
}: {
  sid: string;
  server: string;
  bean: string;
  op: MBeanOperation;
}) {
  const [args, setArgs] = useState<string[]>(() => op.params.map(() => ""));
  const invoke = useMutation({
    mutationFn: () =>
      client.mbeanInvoke(sid, {
        server,
        name: bean,
        operation: op.name,
        signature: op.params.map((p) => p.type),
        args,
      }),
  });
  const shortType = (t: string) => t.replace(/^java\.lang\./, "");
  return (
    <div className="card mb-op">
      <div className="mb-op-head">
        <span className="mono">
          <span className="faint">{shortType(op.return_type)} </span>
          <b>{op.name}</b>({op.params.map((p) => `${shortType(p.type)} ${p.name}`).join(", ")})
        </span>
        {op.impact !== "info" && <span className="badge">{op.impact.replace("_", "/")}</span>}
      </div>
      {op.description && op.description !== op.name && <div className="faint">{op.description}</div>}
      <div className="mb-op-form">
        {op.params.map((p, i) => (
          <input
            key={i}
            className="input mono"
            placeholder={`${p.name}: ${shortType(p.type)}`}
            title={p.description}
            aria-label={`${op.name} ${p.name}`}
            value={args[i]}
            onChange={(e) => setArgs((a) => a.map((x, j) => (j === i ? e.target.value : x)))}
            onKeyDown={(e) => e.key === "Enter" && invoke.mutate()}
          />
        ))}
        <button className="btn" disabled={invoke.isPending} onClick={() => invoke.mutate()}>
          {invoke.isPending ? "Invoking…" : "Invoke"}
        </button>
      </div>
      {invoke.isError && <div className="mb-error">{errorText(invoke.error)}</div>}
      {invoke.isSuccess && (
        <div className="mb-result">
          {op.return_type === "void" ? <span className="faint">Done.</span> : <Value v={invoke.data.value} />}
        </div>
      )}
    </div>
  );
}

function BeanDetail({
  sid,
  server,
  name,
  tracked,
}: {
  sid: string;
  server: string;
  name: string;
  tracked: TrackedAttribute[];
}) {
  const [auto, setAuto] = useState(true);
  const [tab, setTab] = useState<"attributes" | "operations" | "notifications">("attributes");
  const q = useQuery({
    queryKey: ["mbean", sid, server, name],
    queryFn: () => client.mbean(sid, server, name),
    refetchInterval: auto ? 2000 : false,
    retry: false,
  });
  if (q.isLoading)
    return (
      <div className="empty">
        <div className="spinner" />
      </div>
    );
  if (q.isError) return <div className="error">{errorText(q.error)}</div>;
  const b = q.data!;
  return (
    <div className="mb-detail">
      <div className="mb-detail-head">
        <div className="mono mb-title">{b.name}</div>
        <div className="faint mono">{b.class_name}</div>
        {b.description && b.description !== b.class_name && <div className="muted">{b.description}</div>}
      </div>
      <div className="tabs" role="tablist">
        <button
          className="tab"
          role="tab"
          aria-selected={tab === "attributes"}
          onClick={() => setTab("attributes")}
        >
          Attributes ({b.attributes.length})
        </button>
        <button
          className="tab"
          role="tab"
          aria-selected={tab === "operations"}
          onClick={() => setTab("operations")}
        >
          Operations ({b.operations.length})
        </button>
        {b.notifications.length > 0 && (
          <button
            className="tab"
            role="tab"
            aria-selected={tab === "notifications"}
            onClick={() => setTab("notifications")}
          >
            Notifications
          </button>
        )}
        <span className="grow" />
        {tab === "attributes" && (
          <label className="check" title="Re-read the attributes every 2 seconds">
            <input type="checkbox" checked={auto} onChange={(e) => setAuto(e.target.checked)} />
            Auto-refresh
          </label>
        )}
      </div>
      <div className="mb-detail-body">
        {tab === "attributes" ? (
          <Attributes sid={sid} server={server} bean={b.name} attrs={b.attributes} tracked={tracked} />
        ) : tab === "notifications" ? (
          <Notifications sid={sid} server={server} bean={b} />
        ) : b.operations.length ? (
          b.operations.map((o, i) => (
            <Operation key={`${o.name}/${i}`} sid={sid} server={server} bean={b.name} op={o} />
          ))
        ) : (
          <div className="faint">No operations.</div>
        )}
      </div>
    </div>
  );
}

// ---------- notifications ----------

function Notifications({ sid, server, bean }: { sid: string; server: string; bean: MBeanDetail }) {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["mbean-notifications", sid],
    queryFn: () => client.mbeanNotifications(sid),
    refetchInterval: 2000,
  });
  const subscribe = useMutation({
    mutationFn: (enabled: boolean) => client.mbeanSubscribe(sid, { server, name: bean.name, enabled }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["mbean-notifications", sid] }),
    onError: (e) => toast(errorText(e), "error"),
  });
  const subscribed = !!q.data?.subscriptions.some((s) => s.server === server && s.name === bean.name);
  const received = (q.data?.notifications ?? [])
    .filter((n) => n.server === server && n.name === bean.name)
    .reverse();
  return (
    <>
      <div className="mb-notif-head">
        <button
          className={`btn${subscribed ? "" : " primary"}`}
          disabled={subscribe.isPending}
          onClick={() => subscribe.mutate(!subscribed)}
        >
          {subscribed ? "Unsubscribe" : "Subscribe"}
        </button>
        <span className="faint">
          {subscribed
            ? "Listening. The agent keeps the last 1000 notifications of all subscribed MBeans."
            : "Subscribe to receive this MBean's notifications."}
        </span>
      </div>
      <div className="mb-notif-types">
        {bean.notifications.map((n, i) => (
          <div key={i} title={n.description}>
            <span className="mono">{n.types.join(", ") || n.name}</span>
            {n.description && n.description !== n.name && <span className="faint"> — {n.description}</span>}
          </div>
        ))}
      </div>
      {q.isError && <div className="mb-error">{errorText(q.error)}</div>}
      {received.length > 0 && (
        <div className="mb-table" role="table" aria-label="Notifications">
          {received.map((n) => (
            <div key={n.seq} className="mb-row mb-notif" role="row">
              <div className="mb-name" role="cell">
                <span className="mono">{formatClock(n.time)}</span>
                <span className="faint mono mb-type">#{n.sequence_number}</span>
              </div>
              <div className="mb-val" role="cell">
                <div className="mono">{n.type}</div>
                {n.message && <div className="mb-str">{n.message}</div>}
                {n.data !== null && n.data !== undefined && <Value v={n.data} depth={1} />}
              </div>
            </div>
          ))}
        </div>
      )}
      {subscribed && !received.length && <div className="faint">No notifications yet.</div>}
    </>
  );
}

// ---------- servers ----------

function ServerBar({
  sid,
  server,
  onSelect,
}: {
  sid: string;
  server: string;
  onSelect: (id: string) => void;
}) {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["mbean-servers", sid],
    queryFn: () => client.mbeanServers(sid),
    staleTime: 10_000,
  });
  const [form, setForm] = useState<{ url: string; username: string; password: string } | null>(null);
  const connect = useMutation({
    mutationFn: (r: { url: string; username?: string; password?: string; enabled: boolean }) =>
      client.mbeanConnect(sid, r),
    onSuccess: (r, req) => {
      qc.setQueryData(["mbean-servers", sid], r);
      qc.invalidateQueries({ queryKey: ["mbeans", sid] });
      if (req.enabled) {
        setForm(null);
        onSelect(req.url.trim());
      } else if (req.url === server) onSelect("");
    },
    onError: (e) => toast(errorText(e), "error"),
  });
  const servers: MBeanServerInfo[] = q.data?.servers ?? [];
  const current = servers.find((s) => s.id === server);
  return (
    <>
      <select
        className="select mb-server"
        aria-label="MBean server"
        value={server}
        onChange={(e) => onSelect(e.target.value)}
      >
        {servers.length === 0 && <option value="">Platform MBean server</option>}
        {servers.map((s) => (
          <option key={s.id} value={s.id}>
            {s.kind === "remote" ? "Remote: " : s.kind === "local" ? "Local: " : ""}
            {s.label}
            {s.error ? " (disconnected)" : ""}
          </option>
        ))}
      </select>
      {current?.kind === "remote" && (
        <button
          className="btn ghost"
          disabled={connect.isPending}
          onClick={() => connect.mutate({ url: current.id, enabled: false })}
        >
          Disconnect
        </button>
      )}
      <button
        className="btn ghost"
        aria-expanded={!!form}
        onClick={() => setForm(form ? null : { url: "", username: "", password: "" })}
      >
        Connect JMX…
      </button>
      {form && (
        <form
          className="card mb-connect"
          onSubmit={(e) => {
            e.preventDefault();
            connect.mutate({ ...form, enabled: true });
          }}
        >
          <div className="faint">
            The profiled JVM connects to the remote server; credentials are kept in that JVM's memory only.
          </div>
          <input
            className="input mono"
            placeholder="service:jmx:rmi:///jndi/rmi://host:9010/jmxrmi"
            aria-label="JMX service URL"
            autoFocus
            value={form.url}
            onChange={(e) => setForm({ ...form, url: e.target.value })}
          />
          <div className="mb-connect-row">
            <input
              className="input grow"
              placeholder="Username (optional)"
              aria-label="Username"
              autoComplete="off"
              value={form.username}
              onChange={(e) => setForm({ ...form, username: e.target.value })}
            />
            <input
              className="input grow"
              type="password"
              placeholder="Password"
              aria-label="Password"
              autoComplete="off"
              value={form.password}
              onChange={(e) => setForm({ ...form, password: e.target.value })}
            />
          </div>
          <div className="mb-connect-row">
            <span className="grow" />
            <button type="button" className="btn ghost" onClick={() => setForm(null)}>
              Cancel
            </button>
            <button className="btn primary" disabled={!form.url.trim() || connect.isPending}>
              {connect.isPending ? "Connecting…" : "Connect"}
            </button>
          </div>
        </form>
      )}
    </>
  );
}

// ---------- tracked charts ----------

function TrackedCharts({ sid, tracked }: { sid: string; tracked: TrackedAttribute[] }) {
  const range = useApp((s) => s.range);
  const summary = useSummary(sid);
  const tel = useTelemetry(sid, { keys: tracked.map((t) => t.key), points: 300 }, tracked.length > 0);
  const track = useTrack(sid);
  if (!tracked.length) return null;
  const byKey = new Map((tel.data?.series ?? []).map((s) => [s.key, s]));
  const sm = summary.data;
  const domain: [number, number] | undefined = sm ? [sm.start, Math.max(sm.end, sm.start + 1)] : undefined;
  return (
    <div className="mb-charts">
      {tracked.map((t, i) => {
        const s = byKey.get(t.key);
        const last = s && s.v.length ? s.v[s.v.length - 1] : undefined;
        return (
          <div className="card chart-card" key={t.key}>
            <h3>
              <span title={t.key}>
                {t.label}
                {t.server && <span className="faint"> @ {t.server}</span>}
              </span>
              <span className="latest">{last !== undefined ? formatValue(last, t.unit) : ""}</span>
              <select
                className="select mb-unit"
                aria-label={`Unit of ${t.label}`}
                value={t.unit}
                onChange={(e) =>
                  track.mutate({
                    server: t.server,
                    name: t.name,
                    attribute: t.attribute,
                    item: t.item,
                    label: t.label,
                    unit: e.target.value as Unit,
                  })
                }
              >
                {UNITS.map((u) => (
                  <option key={u} value={u}>
                    {u.replace("_", " ")}
                  </option>
                ))}
              </select>
              <button
                className="btn ghost icon"
                aria-label={`Stop plotting ${t.label}`}
                title="Stop plotting"
                onClick={() =>
                  track.mutate({
                    server: t.server,
                    name: t.name,
                    attribute: t.attribute,
                    item: t.item,
                    enabled: false,
                  })
                }
              >
                <IconClose width={12} />
              </button>
            </h3>
            {s && s.t.length ? (
              <TimeChart
                series={[{ label: t.label, t: s.t, v: s.v, color: `var(--c${(i % 8) + 1})` }]}
                unit={t.unit}
                height={120}
                domain={domain}
                bookmarks={tel.data?.bookmarks}
                range={range}
                onSelectRange={(r) => store.set({ range: r })}
              />
            ) : (
              <div className="faint mb-chart-wait">Waiting for samples…</div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ---------- view ----------

export function MBeansView() {
  const s = useCurrentSession()!;
  const has = s.capabilities.includes("mbeans");
  const [server, setServer] = useState("");
  const list = useBeans(s.id, server);
  const [selected, setSelected] = useState<string | null>(null);
  const selectServer = (id: string) => {
    setServer(id);
    setSelected(null);
  };
  if (!has)
    return (
      <div className="empty">
        <h2>MBeans</h2>
        <p style={{ maxWidth: 480 }}>
          The MBean browser needs a live session: attach to a running JVM or start one with the agent.
        </p>
      </div>
    );
  const beans = list.data?.beans ?? [];
  const tracked = list.data?.tracked ?? [];
  const current = selected ?? beans.find((b) => b.name === "java.lang:type=Memory")?.name ?? null;
  return (
    <div className="view">
      <div className="view-header">
        <h2 className="view-title">MBeans</h2>
        <ServerBar sid={s.id} server={server} onSelect={selectServer} />
        {list.data && <span className="faint">{beans.length} beans</span>}
        <span className="grow" />
        <button className="btn ghost" onClick={() => list.refetch()}>
          Reload list
        </button>
      </div>
      <TrackedCharts sid={s.id} tracked={tracked} />
      {list.isLoading ? (
        <div className="empty">
          <div className="spinner" />
        </div>
      ) : list.isError ? (
        <div className="error">{errorText(list.error)}</div>
      ) : (
        <div className="code-layout">
          <BeanTree beans={beans} selected={current} onSelect={setSelected} />
          {current ? (
            <BeanDetail
              key={`${server}\n${current}`}
              sid={s.id}
              server={server}
              name={current}
              tracked={tracked}
            />
          ) : (
            <div className="empty">Select an MBean.</div>
          )}
        </div>
      )}
    </div>
  );
}
