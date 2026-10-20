# Architecture

This page gives an overview of how OpenProfiler fits together and where the code lives. For the
full design and roadmap, read [DESIGN.md](DESIGN.md).

## How it works

```
 UI (browser / Tauri) ──Query API──► openprofiler-daemon ──agent-link (TLS + token)──► agent in the JVM
                                      ├─ openprofiler-jfr       JFR import
                                      ├─ openprofiler-heap      out-of-core heap analysis
                                      ├─ openprofiler-snapshot  .opsnap files
                                      ├─ openprofiler-dex       .apk / .dex / .jar decompiler
                                      └─ openprofiler-remote    ssh / docker / kubectl ──► openprofiler-relay
```

- **Agent** (`libopenprofiler_agent`): a native JVMTI agent plus small boot classes. It samples
  stacks, records allocation, monitor and park events, collects telemetry and takes dumps, then
  pushes compact deltas to the daemon.
- **Daemon**: owns sessions and serves one [Query API](spec/query-api.md) for live sessions
  and files alike. The UI, CLI, MCP server and IDE bridge are all clients of it.
- **Relay**: a static helper binary that is uploaded into remote hosts and containers to attach
  and tunnel the agent link over stdio.

### Remote attach

No port is opened on the target. OpenProfiler runs `ssh`, `docker exec -i` and `kubectl exec -i`
with your own credentials and configuration. It uploads a small static helper
(`openprofiler-relay`) and the agent through stdin, checks their SHA-256, and tunnels the agent
link through the helper's stdio. Distroless and other shell-less images are reached with
`--debug-container`. It adds an ephemeral debug container (`busybox:1.37` by default, or
`--debug-container <image>`) that shares the pod's process namespace and has `CAP_SYS_PTRACE`
(`kubectl debug --profile=general`). Later attaches reuse it, since Kubernetes can't remove
ephemeral containers. Pods under the `restricted` Pod Security level reject such a container.

The relay and agents come from the bundle directory: `bundle/` next to the `openprofiler` binary
in release archives, `$OPENPROFILER_BUNDLE_DIR`, `--bundle-dir`, or `dist/bundle` built by
`scripts/build-bundle.sh` (Linux x86_64, aarch64 and armv7, musl and glibc).

### Specifications

- [Agent link](spec/agent-link.md): wire protocol, agent options and the trigger file format
- [Query API](spec/query-api.md): the daemon API used by every client
- [Snapshot format](spec/snapshot-format.md): the `.opsnap` file format

## Repository layout

| Path | Contents |
|---|---|
| `crates/openprofiler-agent`, `crates/jvmti-sys` | Native JVMTI agent and bindings |
| `crates/openprofiler-model` | Symbols, stacks, samples; call tree, hot spots, flame, call graph, diff |
| `crates/openprofiler-jfr` | JFR parser |
| `crates/openprofiler-hprof`, `crates/openprofiler-phd`, `crates/openprofiler-heap` | HPROF and PHD parsers, heap analysis engine |
| `crates/openprofiler-snapshot` | `.opsnap` reader and writer ([spec](spec/snapshot-format.md)) |
| `crates/openprofiler-proto`, `crates/openprofiler-link` | Agent-link protocol ([spec](spec/agent-link.md)) and its daemon-side client |
| `crates/openprofiler-attach` | JVM discovery and dynamic attach, including containers |
| `crates/openprofiler-remote`, `crates/openprofiler-relay` | ssh, docker and kubectl transports; the static relay binary |
| `crates/openprofiler-daemon` | Query API server, insights, exporters, MCP, IDE bridge |
| `crates/openprofiler-cli` | The `openprofiler` binary |
| `crates/openprofiler-dex` | DEX/APK and class-file/JAR readers, smali and bytecode views, the Java decompiler, Android resource decoding |
| `java/` | Boot classes, public `openprofiler-api`, test workloads, the probe version matrix (`java/matrix`) and the overhead benchmark (`java/bench`) |
| `ui/` | React + TypeScript UI, Tauri desktop shell |
| `ide/intellij` | IntelliJ IDEA plugin (Kotlin) |
| `ide/vscode` | VS Code / VSCodium extension |
