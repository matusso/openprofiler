import { useMutation } from "@tanstack/react-query";
import { OpenProfilerLogo, IconOpen, IconPlug } from "../components/icons";
import { recentPaths } from "../lib/tauri";
import { client, queryClient, readonly, selectSession, store } from "../state";
import { useOpenFile } from "../components/Dialogs";
import { OpenProfilerWordmark } from "../components/Brand";

export function Welcome() {
  const open = useOpenFile();
  const demo = useMutation({
    mutationFn: () => client.post<{ id: string }>("/sessions/demo"),
    onSuccess: (s) => {
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      selectSession(s.id);
    },
  });
  const recent = recentPaths();
  if (readonly)
    return (
      <div className="view-body">
        <div className="welcome">
          <h1>
            <OpenProfilerWordmark />
          </h1>
          <div className="muted">The shared session is no longer available. Ask for a new link.</div>
        </div>
      </div>
    );
  return (
    <div className="view-body">
      <div className="welcome">
        <div className="welcome-brand">
          <OpenProfilerLogo className="welcome-logo" width={64} height={64} aria-hidden="true" />
          <div>
            <h1>
              <OpenProfilerWordmark />
            </h1>
            <div className="muted">Open-source JVM profiler: CPU, memory, heap, threads and locks.</div>
          </div>
        </div>
        <div className="welcome-grid">
          <button
            className="card"
            onClick={() => store.set({ dialog: "open" })}
            style={{ textAlign: "left" }}
          >
            <h3>
              <IconOpen width={14} /> Open a file
            </h3>
            <div className="muted">
              .opsnap snapshots, JFR recordings, HPROF heap dumps, or an Android app to decompile. Drop a file
              anywhere.
            </div>
          </button>
          <button
            className="card"
            onClick={() => store.set({ dialog: "attach" })}
            style={{ textAlign: "left" }}
          >
            <h3>
              <IconPlug width={14} /> Attach to a JVM
            </h3>
            <div className="muted">
              Profile a running JVM without restarting it: local, over SSH, in a Docker container or a
              Kubernetes pod.
            </div>
          </button>
        </div>
        {recent.length > 0 && (
          <>
            <div className="section-title">Recent files</div>
            <div className="list">
              {recent.map((p) => (
                <button key={p} className="list-item mono" onClick={() => open.mutate(p)}>
                  {p}
                </button>
              ))}
            </div>
          </>
        )}
        <p className="faint" style={{ marginTop: 24 }}>
          No data handy?{" "}
          <button className="link" onClick={() => demo.mutate()}>
            Open the synthetic demo session
          </button>
          .
        </p>
      </div>
    </div>
  );
}
