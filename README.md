# OpenProfiler

**An open-source profiler for the JVM.** It finds where your Java application spends CPU time,
what it allocates, what holds on to memory, and where threads wait on each other. It works on
running JVMs (local, remote, or in Docker and Kubernetes) and on saved files such as JFR
recordings and HPROF heap dumps.

OpenProfiler is written in Rust: a native JVMTI agent, fast analysis engines, a daemon and a
CLI. The UI is React, served in a browser or as a Tauri desktop app.

## Features

| Area | What you get |
|---|---|
| **CPU** | Call tree, hot spots with back traces, flame graph, call graph. Async sampling with `AsyncGetCallTrace` on Linux, safe sync sampling elsewhere. |
| **Memory** | Allocation call tree and hot spots, live versus collected objects, GC and heap telemetry. |
| **Heap dumps** | Out-of-core HPROF analysis: dominator tree, retained sizes (identical to Eclipse MAT), biggest objects, references, paths to GC roots, and leak and waste inspections. A 10 GB dump opens in about 25 s cold and under 5 ms warm. |
| **Threads and locks** | Thread history, states, thread dumps, monitor and `java.util.concurrent` contention with lock owners, virtual threads. |
| **Telemetry** | Heap, GC, CPU, threads and classes over time, with a global time range that filters every view. |
| **Snapshots** | Save sessions as `.opsnap` files, compare two snapshots, export to HTML, CSV, JSON, SVG or collapsed stacks. |
| **Triggers** | Take heap or thread dumps and save recordings automatically on thresholds, OOM, timers, start or exit. |
| **Remote and containers** | Attach over ssh, `docker exec` and `kubectl exec`, including ephemeral debug containers for distroless images. The agent port is never exposed. (Daemon API today; UI and CLI support planned.) |
| **Automation** | A CLI, a CI regression gate (`--fail-if "cpu.total > +10%"`), and an MCP server so AI agents can profile and read results. |
| **IDE** | VS Code extension with CPU heat in the gutter and navigation to source. |
| **Security** | The agent link uses TLS 1.3 with a pinned fingerprint and a mandatory random token. |

Supported inputs: live JVMs (JDK 11+), `.jfr`, `.hprof`, `.hprof.gz`, `.opsnap` snapshots and
`.oplink` offline recordings.

## Quick start

```sh
# Build (needs Rust, a JDK and Node; see "Building from source")
cargo build --release
(cd ui && npm ci && npm run build)

# Try the UI with a synthetic demo session
target/release/openprofiler daemon --web --demo
# → open the printed http://127.0.0.1:7878/?token=… URL
```

## Usage

### Analyse a file

```sh
openprofiler open recording.jfr                       # summary
openprofiler hotspots recording.jfr --kind alloc      # top allocation sites
openprofiler analyze heap.hprof                       # build the heap index, print insights
openprofiler daemon --web heap.hprof                  # explore it in the UI
```

### Profile a running JVM

```sh
openprofiler list                                     # local JVMs
openprofiler attach <pid> --duration 10s              # live hot spots in the terminal
openprofiler record --pid <pid> --duration 60s -o app.opsnap          # record a snapshot
openprofiler record --pid <pid> --heap -o app.opsnap                  # ... with a heap dump at the end
openprofiler record --pid <pid> --triggers triggers.json -o app.opsnap  # dumps on thresholds, OOM, timers
openprofiler dump <pid> -o heap.hprof                 # heap dump only
```

In the UI, use **Attach** to pick a local JVM. Remote attach over ssh, Docker and Kubernetes is
available through the daemon's Query API (`POST /sessions/attach` with `options.remote`, built
with `scripts/build-bundle.sh`); UI and CLI support for it is still planned.

### Start a JVM with the agent

```sh
java -agentpath:/path/to/libopenprofiler_agent.so=port=0 -jar app.jar
```

The agent listens on loopback and publishes its port and token in
`$TMPDIR/openprofiler-<pid>.json`; `openprofiler attach <pid>` and the UI pick it up. To record
without a daemon, use offline mode: `=mode=offline,file=app.oplink`. Options are also read from
`OPENPROFILER_AGENT_OPTIONS`. See [the agent-link spec](docs/spec/agent-link.md) for all options
and the trigger file format.

### Compare, export and gate in CI

```sh
openprofiler compare base.opsnap head.opsnap --fail-if "cpu.total > +10%"   # exit 1 on regression
openprofiler export app.opsnap --view flame --format svg -o flame.svg      # csv|json|html|svg|collapsed
openprofiler convert recording.jfr -o recording.opsnap
```

### Use it from an AI agent (MCP)

```sh
openprofiler mcp        # MCP server on stdio
```

Register the command with any MCP client. Tools include `list_jvms`, `attach`, `load_snapshot`,
`hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`, `compare`, `thread_dump` and
`set_triggers`.

## How it works

```
 UI (browser / Tauri) ──Query API──► openprofiler-daemon ──agent-link (TLS + token)──► agent in the JVM
                                      ├─ openprofiler-jfr       JFR import
                                      ├─ openprofiler-heap      out-of-core heap analysis
                                      ├─ openprofiler-snapshot  .opsnap files
                                      └─ openprofiler-remote    ssh / docker / kubectl ──► openprofiler-relay
```

- **Agent** (`libopenprofiler_agent`): a native JVMTI agent plus small boot classes. It samples
  stacks, records allocation, monitor and park events, collects telemetry and takes dumps, then
  pushes compact deltas to the daemon.
- **Daemon**: owns sessions and serves one [Query API](docs/spec/query-api.md) for live sessions
  and files alike. The UI, CLI, MCP server and IDE bridge are all clients of it.
- **Relay**: a static helper binary that is uploaded into remote hosts and containers to attach
  and tunnel the agent link over stdio.

Read [docs/DESIGN.md](docs/DESIGN.md) for the full architecture and roadmap.

## Building from source

**Requirements**
- Rust stable (via rustup)
- JDK 11+ (21+ recommended) for the boot classes and tests
- Node 22+ and npm for the UI
- Docker (optional) for Linux builds, the remote bundle and the MAT parity test

```sh
cargo build --release                 # all crates, incl. target/release/libopenprofiler_agent.*
java/build.sh                         # openprofiler-boot.jar / openprofiler-api.jar (optional; the agent embeds the boot classes)
(cd ui && npm ci && npm run build)    # web UI → ui/dist
(cd ui && npm run tauri build)        # desktop app (embeds the daemon)
scripts/build-bundle.sh               # Linux relay + agents for remote attach → dist/bundle
```

**Tests**

```sh
cargo test --workspace                                                   # incl. live agent end-to-end tests (need java)
MAT_HOME=/path/to/mat cargo test -p openprofiler-heap --test mat_parity  # retained sizes vs Eclipse MAT (Docker)
(cd ui && npm run lint && npm run typecheck && npm test)
```

## Repository layout

| Path | Contents |
|---|---|
| `crates/openprofiler-agent`, `crates/jvmti-sys` | Native JVMTI agent and bindings |
| `crates/openprofiler-model` | Symbols, stacks, samples; call tree, hot spots, flame, call graph, diff |
| `crates/openprofiler-jfr` | JFR parser |
| `crates/openprofiler-hprof`, `crates/openprofiler-heap` | HPROF parser and heap analysis engine |
| `crates/openprofiler-snapshot` | `.opsnap` reader and writer ([spec](docs/spec/snapshot-format.md)) |
| `crates/openprofiler-proto`, `crates/openprofiler-link` | Agent-link protocol ([spec](docs/spec/agent-link.md)) and its daemon-side client |
| `crates/openprofiler-attach` | JVM discovery and dynamic attach, including containers |
| `crates/openprofiler-remote`, `crates/openprofiler-relay` | ssh, docker and kubectl transports; the static relay binary |
| `crates/openprofiler-daemon` | Query API server, insights, exporters, MCP, IDE bridge |
| `crates/openprofiler-cli` | The `openprofiler` binary |
| `java/` | Boot classes, public `openprofiler-api`, test workload |
| `ui/` | React + TypeScript UI, Tauri desktop shell |
| `ide/vscode` | VS Code extension |

## Project status

OpenProfiler is in early development. The offline analyzer, the live agent and most of remote
attach work today. Bytecode instrumentation and probes (JDBC, HTTP, …) have not been started.

| Phase | State |
|---|---|
| Foundations | Done |
| Offline analyzer (HPROF, JFR, UI, CLI, MCP) | Done |
| Live agent (sampling, allocations, locks, telemetry, dumps, triggers) | Done |
| Probes and instrumentation | Not started |
| Remote and cloud | Mostly done |
| Polish and ecosystem | Partial |

Planned work is tracked in [GitHub issues](https://github.com/matusso/openprofiler/issues).

## Licence

Apache-2.0. See [LICENSE](LICENSE).
