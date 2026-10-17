import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { isTauri, pickFile, recentPaths, rememberPath } from "../lib/tauri";
import { useCurrentSession } from "../api/hooks";
import type { LocalJvm, RemoteContainer, RemotePod, RemoteTarget } from "../api/types";
import { DEFAULT_DEBUG_IMAGE, looksDistroless, parseSshSpec, remember } from "../lib/remote";
import { client, queryClient, selectSession, store, useApp } from "../state";
import { DataTable, type Column } from "./DataTable";
import { FileBrowser } from "./FileBrowser";
import { IconOpen } from "./icons";

function Modal({
  title,
  onClose,
  children,
  footer,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
  footer?: React.ReactNode;
}) {
  useEffect(() => {
    const k = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog" role="dialog" aria-modal="true" aria-label={title}>
        <div className="dialog-head">{title}</div>
        <div className="dialog-body">{children}</div>
        {footer && <div className="dialog-foot">{footer}</div>}
      </div>
    </div>
  );
}

export function useOpenFile() {
  return useMutation({
    mutationFn: (path: string) => client.open(path),
    onSuccess: (s, path) => {
      rememberPath(path);
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      selectSession(s.id);
      store.set({ dialog: null, view: s.kind === "dex" ? "code" : "overview" });
    },
  });
}

export function OpenDialog() {
  const [path, setPath] = useState("");
  const [browsing, setBrowsing] = useState(false);
  const open = useOpenFile();
  const inputRef = useRef<HTMLInputElement>(null);
  const close = () => store.set({ dialog: null });
  useEffect(() => inputRef.current?.focus(), []);
  const recent = recentPaths();
  const submit = (p = path) => p.trim() && open.mutate(p.trim());
  return (
    <Modal
      title="Open a profile, heap dump or app to decompile"
      onClose={close}
      footer={
        <>
          <button className="btn" onClick={close}>
            Cancel
          </button>
          <button className="btn primary" disabled={!path.trim() || open.isPending} onClick={() => submit()}>
            {open.isPending ? "Opening…" : "Open"}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        Supports <code>.opsnap</code> snapshots, <code>.jfr</code> recordings and <code>.hprof</code> heap
        dumps (optionally gzipped). Android apps (<code>.apk</code>, <code>.xapk</code>, <code>.apks</code>,{" "}
        <code>.aab</code>), <code>.dex</code> files and Java bytecode (<code>.jar</code>, <code>.war</code>,{" "}
        <code>.ear</code>, <code>.jmod</code>, <code>.class</code>) open in the decompiler. The path is
        resolved on the machine running the OpenProfiler daemon.
      </p>
      <div style={{ display: "flex", gap: 8 }}>
        <input
          ref={inputRef}
          className="input grow mono"
          placeholder="/path/to/recording.jfr"
          value={path}
          onChange={(e) => setPath(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          aria-label="File path"
        />
        <button
          className="btn"
          aria-pressed={browsing}
          onClick={() => setBrowsing((b) => !b)}
          title="Browse directories on the daemon's machine"
        >
          <IconOpen /> Browse…
        </button>
        {isTauri() && (
          <button
            className="btn"
            onClick={async () => {
              const p = await pickFile();
              if (p) {
                setPath(p);
                submit(p);
              }
            }}
          >
            System dialog…
          </button>
        )}
      </div>
      {browsing && (
        <FileBrowser
          start={path.trim() || undefined}
          selected={path}
          onSelect={setPath}
          onOpen={(p) => {
            setPath(p);
            submit(p);
          }}
        />
      )}
      {open.isError && <div className="error">{String(open.error.message)}</div>}
      {!browsing && recent.length > 0 && (
        <>
          <div className="section-title">Recent</div>
          <div className="list">
            {recent.map((p) => (
              <button key={p} className="list-item mono" onClick={() => submit(p)} title={p}>
                {p}
              </button>
            ))}
          </div>
        </>
      )}
    </Modal>
  );
}

const JVM_COLUMNS: Column<LocalJvm>[] = [
  { id: "pid", header: "PID", width: 70, render: (j) => <span className="mono">{j.pid}</span> },
  {
    id: "main",
    header: "Main class / args",
    flex: true,
    render: (j) => (
      <span title={j.args}>
        {j.main_class || "<unknown>"} <span className="faint">{j.args}</span>
      </span>
    ),
  },
  { id: "vm", header: "VM", width: 110, render: (j) => j.vm_version ?? "" },
  {
    id: "state",
    header: "",
    width: 90,
    render: (j) =>
      j.profiled ? (
        <span className="badge live">profiled</span>
      ) : j.attachable ? (
        <span className="badge">ready</span>
      ) : (
        <span className="badge warning">no attach</span>
      ),
  },
];

function JvmTable({
  jvms,
  loading,
  loadingText,
  label,
  selected,
  onSelect,
  onActivate,
  height = 300,
}: {
  jvms: LocalJvm[];
  loading: boolean;
  loadingText?: string;
  label: string;
  selected: number | null;
  onSelect: (pid: number) => void;
  onActivate: (pid: number) => void;
  height?: number;
}) {
  return (
    <div style={{ height }}>
      <DataTable
        label={label}
        rows={jvms}
        rowKey={(j) => j.pid}
        selected={selected}
        onSelect={(j) => j.attachable && onSelect(j.pid)}
        onActivate={(j) => j.attachable && onActivate(j.pid)}
        empty={
          <div className="empty">
            {loading ? (
              <span style={{ display: "flex", gap: 8, alignItems: "center" }}>
                <span className="spinner" /> {loadingText}
              </span>
            ) : (
              "No JVMs found."
            )}
          </div>
        }
        columns={JVM_COLUMNS}
      />
    </div>
  );
}

type AttachTab = "local" | "ssh" | "docker" | "kubernetes";

const ATTACH_TABS: { id: AttachTab; label: string }[] = [
  { id: "local", label: "Local" },
  { id: "ssh", label: "SSH" },
  { id: "docker", label: "Docker" },
  { id: "kubernetes", label: "Kubernetes" },
];

const LOCAL: RemoteTarget = { hops: [] };
const UPLOADING = "Uploading the relay and listing JVMs…";

/** What the Attach button does: attach a JVM, or list a container's/pod's JVMs and attach the only one. */
type Pick = { kind: "jvm"; pid: number; target?: RemoteTarget } | { kind: "target"; target: RemoteTarget };

export function AttachDialog() {
  const close = () => store.set({ dialog: null });
  const [tab, setTabState] = useState<AttachTab>(() => {
    const t = remember("tab");
    return ATTACH_TABS.some((x) => x.id === t) ? (t as AttachTab) : "local";
  });
  const [pick, setPick] = useState<Pick | null>(null);
  const attach = useMutation({
    mutationFn: ({ pid, target }: { pid: number; target?: RemoteTarget }) =>
      client.attach(pid, target?.hops.length ? { remote: target } : undefined),
    onSuccess: (s) => {
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      selectSession(s.id);
      store.set({ dialog: null, view: "overview" });
    },
  });
  // JVMs on a remote target; `auto` attaches when there is exactly one.
  const discovery = useMutation({
    mutationFn: ({ target }: { target: RemoteTarget; auto: boolean }) => client.discover(target, "jvms"),
    onSuccess: (jvms, { target, auto }) => {
      const ok = jvms.filter((j) => j.attachable);
      if (auto && ok.length === 1) attach.mutate({ pid: ok[0].pid, target });
    },
  });
  const setTab = (t: AttachTab) => {
    remember("tab", t);
    setTabState(t);
    setPick(null);
    discovery.reset();
    attach.reset();
  };
  const listJvms = (target: RemoteTarget, auto: boolean) => {
    setPick({ kind: "target", target });
    discovery.mutate({ target, auto });
  };
  const run = (p: Pick | null = pick) => {
    if (!p) return;
    if (p.kind === "jvm") attach.mutate({ pid: p.pid, target: p.target });
    else listJvms(p.target, true);
  };
  const busy = attach.isPending || discovery.isPending;
  const discovered = discovery.variables?.target;
  return (
    <Modal
      title="Attach to a JVM"
      onClose={close}
      footer={
        <>
          <button className="btn" onClick={close}>
            Cancel
          </button>
          <button className="btn primary" disabled={!pick || busy} onClick={() => run()}>
            {attach.isPending ? "Attaching…" : "Attach"}
          </button>
        </>
      }
    >
      <div className="seg" role="tablist" aria-label="Where the JVM runs" style={{ marginBottom: 10 }}>
        {ATTACH_TABS.map((t) => (
          <button key={t.id} role="tab" aria-pressed={tab === t.id} onClick={() => setTab(t.id)}>
            {t.label}
          </button>
        ))}
      </div>
      {tab === "local" && (
        <LocalJvms
          selected={pick?.kind === "jvm" ? pick.pid : null}
          onSelect={(pid) => setPick({ kind: "jvm", pid })}
          onActivate={(pid) => run({ kind: "jvm", pid })}
        />
      )}
      {tab === "ssh" && <SshTarget busy={busy} onList={(t) => listJvms(t, false)} />}
      {tab === "docker" && (
        <DockerTargets
          onSelect={(t) => setPick({ kind: "target", target: t })}
          onActivate={(t) => listJvms(t, true)}
        />
      )}
      {tab === "kubernetes" && (
        <KubeTargets
          onSelect={(t) => setPick({ kind: "target", target: t })}
          onActivate={(t) => listJvms(t, true)}
        />
      )}
      {tab !== "local" && discovered && (
        <>
          <div className="section-title">JVMs</div>
          <JvmTable
            label="Remote JVMs"
            height={tab === "ssh" ? 300 : 150}
            jvms={discovery.data ?? []}
            loading={discovery.isPending}
            loadingText={UPLOADING}
            selected={pick?.kind === "jvm" ? pick.pid : null}
            onSelect={(pid) => setPick({ kind: "jvm", pid, target: discovered })}
            onActivate={(pid) => run({ kind: "jvm", pid, target: discovered })}
          />
        </>
      )}
      {discovery.isError && <div className="error">{String(discovery.error.message)}</div>}
      {attach.isError && <div className="error">{String(attach.error.message)}</div>}
    </Modal>
  );
}

function LocalJvms(props: {
  selected: number | null;
  onSelect: (pid: number) => void;
  onActivate: (pid: number) => void;
}) {
  const jvms = useQuery({ queryKey: ["jvms"], queryFn: client.jvms, refetchInterval: 5000 });
  return (
    <>
      {jvms.isError && <div className="error">{String(jvms.error.message)}</div>}
      <JvmTable label="Local JVMs" jvms={jvms.data ?? []} loading={jvms.isLoading} {...props} />
    </>
  );
}

function SshTarget({ busy, onList }: { busy: boolean; onList: (t: RemoteTarget) => void }) {
  const [spec, setSpec] = useState(() => remember("ssh") ?? "");
  const hop = parseSshSpec(spec);
  const submit = () => {
    if (!hop || busy) return;
    remember("ssh", spec.trim());
    onList({ hops: [hop] });
  };
  return (
    <>
      <p className="muted" style={{ marginTop: 0 }}>
        Runs <code>ssh</code> on the daemon&apos;s machine with its keys and <code>~/.ssh/config</code>; the
        agent is reached through the SSH channel, no port is opened.
      </p>
      <div style={{ display: "flex", gap: 8 }}>
        <input
          className="input grow mono"
          placeholder="user@host:22"
          value={spec}
          autoFocus
          onChange={(e) => setSpec(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          aria-label="SSH host"
        />
        <button className="btn" disabled={!hop || busy} onClick={submit}>
          List JVMs
        </button>
      </div>
      {spec.trim() && !hop && <div className="faint">Expected [user@]host[:port]</div>}
    </>
  );
}

function DockerTargets({
  onSelect,
  onActivate,
}: {
  onSelect: (t: RemoteTarget) => void;
  onActivate: (t: RemoteTarget) => void;
}) {
  const containers = useQuery({
    queryKey: ["discover", "containers"],
    queryFn: () => client.discover(LOCAL, "containers"),
  });
  const [sel, setSel] = useState<string | null>(null);
  const target = (c: RemoteContainer): RemoteTarget => ({
    hops: [{ type: "docker", container: c.name || c.id }],
  });
  return (
    <>
      {containers.isError && <div className="error">{String(containers.error.message)}</div>}
      <div style={{ height: 180 }}>
        <DataTable
          label="Docker containers"
          rows={containers.data ?? []}
          rowKey={(c) => c.id}
          selected={sel}
          onSelect={(c) => (setSel(c.id), onSelect(target(c)))}
          onActivate={(c) => (setSel(c.id), onActivate(target(c)))}
          empty={
            <div className="empty">
              {containers.isLoading ? <div className="spinner" /> : "No running containers."}
            </div>
          }
          columns={[
            { id: "name", header: "Container", flex: true, render: (c) => c.name },
            {
              id: "image",
              header: "Image",
              width: 220,
              render: (c) => <span className="faint">{c.image}</span>,
            },
            { id: "status", header: "Status", width: 130, render: (c) => c.status },
          ]}
        />
      </div>
      <div className="faint">Double-click a container to attach to its JVM.</div>
    </>
  );
}

function KubeTargets({
  onSelect,
  onActivate,
}: {
  onSelect: (t: RemoteTarget) => void;
  onActivate: (t: RemoteTarget) => void;
}) {
  const contexts = useQuery({
    queryKey: ["discover", "contexts"],
    queryFn: () => client.discover(LOCAL, "contexts"),
  });
  const [context, setContext] = useState<string | null>(() => remember("context"));
  const ctx = contexts.data?.includes(context ?? "") ? context! : (contexts.data?.[0] ?? undefined);
  const [nsInput, setNsInput] = useState(() => remember("namespace") ?? "");
  const [ns, setNs] = useState(nsInput);
  const pods = useQuery({
    queryKey: ["discover", "pods", ctx, ns],
    queryFn: () => client.discover(LOCAL, "pods", { context: ctx, namespace: ns.trim() || undefined }),
    enabled: contexts.isSuccess,
  });
  const [sel, setSel] = useState<RemotePod | null>(null);
  const [container, setContainer] = useState<string | null>(null);
  const [debug, setDebug] = useState(false);
  const [image, setImage] = useState(() => remember("debug-image") ?? DEFAULT_DEBUG_IMAGE);
  const target = (p: RemotePod, c: string | null, dbg: boolean): RemoteTarget => {
    const name = c ?? p.containers[0];
    return {
      hops: [
        {
          type: "kubectl",
          ...(ctx ? { context: ctx } : {}),
          namespace: p.namespace,
          pod: p.name,
          ...(name ? { container: name } : {}),
          ...(dbg ? { debug_image: image.trim() || DEFAULT_DEBUG_IMAGE } : {}),
        },
      ],
    };
  };
  const choose = (p: RemotePod) => {
    const same = sel?.name === p.name && sel.namespace === p.namespace;
    const c = same ? container : null;
    const dbg = same ? debug : looksDistroless(p.images[0] ?? "");
    setSel(p);
    setContainer(c);
    setDebug(dbg);
    return target(p, c, dbg);
  };
  const applyNs = () => {
    remember("namespace", nsInput.trim());
    setNs(nsInput);
  };
  // Re-announce the selection when its options change.
  const update = (c: string | null, dbg: boolean) => sel && onSelect(target(sel, c, dbg));
  return (
    <>
      <div style={{ display: "flex", gap: 8, marginBottom: 8, alignItems: "center" }}>
        <select
          className="input"
          aria-label="kubectl context"
          value={ctx ?? ""}
          onChange={(e) => (remember("context", e.target.value), setContext(e.target.value))}
          disabled={!contexts.data?.length}
        >
          {(contexts.data ?? []).map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <input
          className="input grow mono"
          placeholder="namespace (all)"
          value={nsInput}
          onChange={(e) => setNsInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && applyNs()}
          onBlur={applyNs}
          aria-label="Namespace"
        />
        <button className="btn" onClick={() => pods.refetch()} disabled={pods.isFetching}>
          Refresh
        </button>
      </div>
      {contexts.isError && <div className="error">{String(contexts.error.message)}</div>}
      {pods.isError && <div className="error">{String(pods.error.message)}</div>}
      <div style={{ height: 200 }}>
        <DataTable
          label="Pods"
          rows={pods.data ?? []}
          rowKey={(p) => `${p.namespace}/${p.name}`}
          selected={sel ? `${sel.namespace}/${sel.name}` : null}
          onSelect={(p) => onSelect(choose(p))}
          onActivate={(p) => onActivate(choose(p))}
          empty={
            <div className="empty">
              {contexts.isLoading || pods.isLoading ? <div className="spinner" /> : "No pods."}
            </div>
          }
          columns={[
            { id: "ns", header: "Namespace", width: 130, render: (p) => p.namespace },
            { id: "name", header: "Pod", flex: true, render: (p) => p.name },
            { id: "phase", header: "Phase", width: 80, render: (p) => p.phase },
            {
              id: "images",
              header: "Images",
              width: 220,
              render: (p) => <span className="faint">{p.images.join(", ")}</span>,
              title: (p) => p.images.join("\n"),
            },
          ]}
        />
      </div>
      <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center", flexWrap: "wrap" }}>
        {sel && sel.containers.length > 1 && (
          <select
            className="input"
            aria-label="Container"
            value={container ?? sel.containers[0]}
            onChange={(e) => {
              const c = e.target.value;
              const dbg = looksDistroless(sel.images[sel.containers.indexOf(c)] ?? "");
              setContainer(c);
              setDebug(dbg);
              update(c, dbg);
            }}
          >
            {sel.containers.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        )}
        <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
          <input
            type="checkbox"
            checked={debug}
            onChange={(e) => (setDebug(e.target.checked), update(container, e.target.checked))}
          />
          Ephemeral debug container (distroless)
        </label>
        {debug && (
          <input
            className="input mono"
            style={{ width: 160 }}
            value={image}
            onChange={(e) => (remember("debug-image", e.target.value), setImage(e.target.value))}
            aria-label="Debug container image"
          />
        )}
      </div>
      <div className="faint">Double-click a pod to attach to its JVM.</div>
    </>
  );
}

const SHARE_TTLS: { label: string; secs?: number }[] = [
  { label: "1 hour", secs: 3600 },
  { label: "1 day", secs: 86400 },
  { label: "7 days", secs: 7 * 86400 },
  { label: "Never (until revoked or the daemon stops)" },
];

export function ShareDialog() {
  const close = () => store.set({ dialog: null });
  const session = useCurrentSession();
  const [ttl, setTtl] = useState(1);
  const [copied, setCopied] = useState(false);
  const shares = useQuery({ queryKey: ["shares"], queryFn: client.shares });
  const create = useMutation({
    mutationFn: (id: string) => client.share(id, SHARE_TTLS[ttl].secs),
    onSuccess: () => {
      setCopied(false);
      queryClient.invalidateQueries({ queryKey: ["shares"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (token: string) => client.revokeShare(token),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["shares"] }),
  });
  const link = (path: string) => `${location.origin}${path}`;
  const url = create.data ? link(create.data.path) : "";
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  const mine = (shares.data ?? []).filter((s) => s.session === session?.id);
  return (
    <Modal
      title="Share a read-only link"
      onClose={close}
      footer={
        <button className="btn" onClick={close}>
          Close
        </button>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        Anyone with the link can view <b>{session?.name ?? "this session"}</b> in the web UI. They can&apos;t
        see other sessions, attach, open files, control recording, take dumps or use MBeans.
      </p>
      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        <span className="label">Expires after</span>
        <select
          className="input"
          value={ttl}
          onChange={(e) => setTtl(Number(e.target.value))}
          aria-label="Expiry"
        >
          {SHARE_TTLS.map((t, i) => (
            <option key={t.label} value={i}>
              {t.label}
            </option>
          ))}
        </select>
        <button
          className="btn primary"
          disabled={!session || create.isPending}
          onClick={() => session && create.mutate(session.id)}
        >
          Create link
        </button>
      </div>
      {create.isError && <div className="error">{String(create.error.message)}</div>}
      {url && (
        <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
          <input
            className="input grow mono"
            readOnly
            value={url}
            onFocus={(e) => e.target.select()}
            aria-label="Share link"
          />
          <button className="btn" onClick={copy}>
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
      )}
      {mine.length > 0 && (
        <>
          <div className="section-title">Links for this session</div>
          <div className="list">
            {mine.map((s) => (
              <div
                key={s.token}
                className="list-item"
                style={{ display: "flex", gap: 8, alignItems: "center" }}
              >
                <span className="mono grow" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                  …{s.token.slice(-8)}
                </span>
                <span className="faint">
                  {s.expires ? `expires ${new Date(s.expires).toLocaleString()}` : "no expiry"}
                </span>
                <button
                  className="btn ghost"
                  disabled={revoke.isPending}
                  onClick={() => revoke.mutate(s.token)}
                >
                  Revoke
                </button>
              </div>
            ))}
          </div>
        </>
      )}
    </Modal>
  );
}

export function Dialogs() {
  const dialog = useApp((s) => s.dialog);
  if (dialog === "open") return <OpenDialog />;
  if (dialog === "attach") return <AttachDialog />;
  if (dialog === "share") return <ShareDialog />;
  return null;
}
