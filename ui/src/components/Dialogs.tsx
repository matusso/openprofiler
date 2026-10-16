import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { isTauri, pickFile, recentPaths, rememberPath } from "../lib/tauri";
import { client, queryClient, selectSession, store, useApp } from "../state";
import { DataTable } from "./DataTable";
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

export function AttachDialog() {
  const close = () => store.set({ dialog: null });
  const jvms = useQuery({ queryKey: ["jvms"], queryFn: client.jvms, refetchInterval: 5000 });
  const [sel, setSel] = useState<number | null>(null);
  const attach = useMutation({
    mutationFn: (pid: number) => client.attach(pid),
    onSuccess: (s) => {
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      selectSession(s.id);
      store.set({ dialog: null, view: "overview" });
    },
  });
  return (
    <Modal
      title="Attach to a local JVM"
      onClose={close}
      footer={
        <>
          <button className="btn" onClick={close}>
            Cancel
          </button>
          <button
            className="btn primary"
            disabled={sel === null || attach.isPending}
            onClick={() => sel !== null && attach.mutate(sel)}
          >
            {attach.isPending ? "Attaching…" : "Attach"}
          </button>
        </>
      }
    >
      {jvms.isError && <div className="error">{String(jvms.error.message)}</div>}
      <div style={{ height: 300 }}>
        <DataTable
          label="Local JVMs"
          rows={jvms.data ?? []}
          rowKey={(j) => j.pid}
          selected={sel}
          onSelect={(j) => j.attachable && setSel(j.pid)}
          onActivate={(j) => j.attachable && attach.mutate(j.pid)}
          empty={
            <div className="empty">{jvms.isLoading ? <div className="spinner" /> : "No JVMs found."}</div>
          }
          columns={[
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
          ]}
        />
      </div>
      {attach.isError && <div className="error">{String(attach.error.message)}</div>}
    </Modal>
  );
}

export function Dialogs() {
  const dialog = useApp((s) => s.dialog);
  if (dialog === "open") return <OpenDialog />;
  if (dialog === "attach") return <AttachDialog />;
  return null;
}
