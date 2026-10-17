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
| **Probes** | SQL statements with bind parameters, HTTP requests in and out (Servlet containers, Netty, Reactor Netty and WebFlux, Vert.x and Quarkus), JPA, Kafka, gRPC, exceptions, files and sockets: hot spots linked to call trees, events, timelines, connection and file leak detection, and call trees split by request, also across thread pools, `CompletableFuture`s, Reactor schedulers and coroutines. Custom probes with one annotation. Tested against the real libraries in a version matrix. |
| **Exact call counts** | Bytecode instrumentation of the packages you choose: invocation counts and times in the call tree, with auto-tuning of hot, trivial methods. |
| **Telemetry** | Heap, GC, CPU, threads and classes over time, with a global time range that filters every view. |
| **Snapshots** | Save sessions as `.opsnap` files, compare two snapshots, export to HTML, CSV, JSON, SVG, collapsed stacks or Arrow tables for DuckDB/Polars. |
| **Triggers** | Take heap or thread dumps and save recordings automatically on thresholds, OOM, timers, start, exit or when a method is called. |
| **Remote and containers** | Attach over ssh, `docker exec` and `kubectl exec`, including ephemeral debug containers for distroless images. The agent port is never exposed. (Daemon API today; UI and CLI support planned.) |
| **Automation** | A CLI, a CI regression gate (`--fail-if "cpu.total > +10%"`), and an MCP server so AI agents can profile and read results. |
| **Decompiler** | A built-in Java decompiler for Android apps and JVM bytecode: browse an `.apk`, `.dex`, `.jar`, `.war` or `.class` as Java source with a package tree, smali or bytecode view, go to declaration, find usages, name and full-text search, lambdas and records, and decoded `AndroidManifest.xml`, layouts and `resources.arsc`. Classes decompile on demand. |
| **IDE** | IntelliJ IDEA plugin and VS Code / VSCodium extension: CPU heat in the editor, navigation to source, launch with the agent. |
| **Security** | The agent link uses TLS 1.3 with a pinned fingerprint and a mandatory random token. |

Supported inputs: live JVMs (JDK 11+), `.jfr`, `.hprof`, `.hprof.gz`, OpenJ9 / IBM Semeru `.phd`
heap dumps (also gzipped; references and sizes only, see [design](docs/DESIGN.md)), `.opsnap`
snapshots and `.oplink` offline recordings. The decompiler opens `.apk` (and `.xapk`, `.apks`, `.aab`) and `.dex`
files, and JVM bytecode: `.jar`, `.war`, `.ear`, `.jmod`, `.aar` and `.class`.

## Install

```sh
brew install matusso/tap/openprofiler                 # macOS and Linux (amd64, arm64)
docker run --rm -p 7878:7878 ghcr.io/matusso/openprofiler   # daemon + web UI
```

Archives for macOS, Linux and Windows (amd64, arm64) are on the
[releases page](https://github.com/matusso/openprofiler/releases). Each one holds the
`openprofiler` CLI, the native agent, the web UI and the remote bundle; keep them together. On
Windows the agent samples CPU with the sync sampler (no async signal sampling), and local attach
works on x86-64 only; the windows-arm64 archive has no agent.

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
openprofiler record --pid <pid> --recorded-only -o app.opsnap         # ... only objects allocation recording sampled
openprofiler record --pid <pid> --triggers triggers.json -o app.opsnap  # dumps on thresholds, OOM, timers
openprofiler dump <pid> -o heap.hprof                 # heap dump only
```

In the UI, use **Attach** to pick a local JVM.

### Profile a JVM on another machine, in a container or in Kubernetes

The same commands take a remote target. Without a pid they pick the only JVM there.

```sh
openprofiler list --ssh me@build-01                         # JVMs on an ssh host
openprofiler attach --ssh bastion --ssh me@app-01:2222      # through a jump host
openprofiler attach --docker shop-api                       # docker exec into a container
openprofiler attach --ssh me@host --docker shop-api         # a container on an ssh host
openprofiler record --kube prod/shop-api-0 -o shop.opsnap   # [namespace/]pod[/container]
openprofiler attach --kube prod/shop-api-0 --context prod-eu --debug-container   # distroless pod
openprofiler dump --kube prod/shop-api-0 -o heap.hprof
```

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

In the UI, **Attach** has Local, SSH, Docker and Kubernetes tabs. On the Kubernetes tab,
double-clicking a pod attaches to its JVM, so a pod on a remote cluster is three clicks away. The
debug container is preselected for distroless and Chainguard images.

### Share a session read-only

In web mode (`openprofiler daemon --web`), **Share read-only link…** in the session menu creates a
link that opens just that session, optionally expiring after an hour, a day or a week. The
holder can browse every view and export, but can't attach, record, dump, open files, use MBeans
or see other sessions. Links are revoked from the same dialog or when the daemon stops.

### Start a JVM with the agent

```sh
java -agentpath:/path/to/libopenprofiler_agent.so=port=0 -jar app.jar
```

The agent listens on loopback and publishes its port and token in
`$TMPDIR/openprofiler-<pid>.json`; `openprofiler attach <pid>` and the UI pick it up. To record
without a daemon, use offline mode: `=mode=offline,file=app.oplink`. Options are also read from
`OPENPROFILER_AGENT_OPTIONS`. See [the agent-link spec](docs/spec/agent-link.md) for all options
and the trigger file format.

### Probes and exact call counts

Probes are on by default (`probes=jdbc:http_server` picks some, `probes=none` turns them off) and
work after a dynamic attach too. Exact call counts are opt-in per package:

```sh
java -agentpath:/path/to/libopenprofiler_agent.so=port=0,instrument=com.acme.*:org.example.* -jar app.jar
```

In the UI, the **Probes** view shows each probe's hot spots (select a payload for its call tree),
events with the request they belong to, a timeline, control objects such as open connections
(with leaks: objects garbage collected without close), and telemetry; for live sessions it also
edits the instrumented packages and enabled probes. Exact counts are the **Calls** kind of the CPU
view. To record your own operations, annotate methods with `dev.openprofiler.api.Probe` from
`openprofiler-api.jar`:

```java
@Probe(value = "orders", payload = "place order {0}")
void place(String orderId) { ... }
```

or call `OpenProfiler.probeEvent("orders", "place order " + id, nanos)`. HTTP and gRPC server
requests become root frames of the call tree (`[GET /owners/{id}]`), including work they hand to
executors, fork/join pools, `CompletableFuture`s, Reactor schedulers, new threads, Kotlin
coroutines and Quarkus' worker pool. Asynchronous requests (WebFlux, Quarkus, asynchronous
servlets) are timed until their response completes.

### Decompile Android apps and jars

The decompiler is built in (`crates/openprofiler-dex`, written in Rust; no JVM or external tool
needed). It reads Android apps (`.apk`, `.xapk`, `.apks`, `.aab`, `.dex`) and JVM bytecode (`.jar`,
`.war`, `.ear`, `.jmod`, `.aar`, `.class`, or a directory of class files such as `target/classes`):

```sh
openprofiler daemon --web app.apk                     # browse it in the UI (Code view)
openprofiler decompile app.apk                        # list classes
openprofiler decompile app.apk com.example.MainActivity          # print Java source (--smali for smali)
openprofiler decompile app.apk --search api_key --code           # search the decompiled source
openprofiler decompile app.apk -o out/                # write every class as .java (or .smali)
openprofiler decompile lib.jar com.example.Service --bytecode    # JVM bytecode of a class
```

Jars, wars and ears open with the classes at any depth (`BOOT-INF/classes`, `WEB-INF/classes`);
archives without their own classes, such as an `.ear`, load the jars inside them. In multi-release
jars the base version of a class wins. Java output covers lambdas and method references (lambda
bodies are printed in place), string concatenation, `switch` on strings, `finally` blocks and
try-with-resources, implicit boxing, records, enums with constant bodies, and generic types
(without debug info, local generics are inferred from the fields, methods and returns their
values flow through); the low-level tab shows the original bytecode.

In the UI, open the file like any other (⌘O or drop it); **Browse…** in the Open dialog
navigates the directories of the machine running the daemon, and can open a directory of
classes. The **Code** view has the package tree,
search over class, method and field names (tick *In code* to search source text), Java and smali
tabs, and a **Resources** tab with the decoded `AndroidManifest.xml`, binary XML layouts,
`resources.arsc` values and images. Click a reference to go to its declaration, click a
declaration to list its usages, and use Alt+← / Alt+→ to go back and forward.

Opening an app only indexes it; classes decompile on first view and are cached. A 55 MB app with
21,000 classes opens in about half a second and decompiles completely in about 2 s on a laptop.
Methods whose control flow can't be fully structured are still printed, with a
`/* Code restructure failed */` note; methods that can't be decompiled at all fall back to a
comment and an `UnsupportedOperationException`.

### Compare, export and gate in CI

```sh
openprofiler compare base.opsnap head.opsnap --fail-if "cpu.total > +10%"   # exit 1 on regression
openprofiler export app.opsnap --view flame --format svg -o flame.svg      # csv|json|html|svg|collapsed
openprofiler convert recording.jfr -o recording.opsnap
openprofiler arrow app.opsnap -o tables/     # Arrow IPC tables for DuckDB, Polars, pyarrow
```

### Use it from an AI agent (MCP)

```sh
openprofiler mcp        # MCP server on stdio
```

Register the command with any MCP client. Tools include `list_jvms`, `attach`, `load_snapshot`,
`hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`, `compare`, `thread_dump`,
`set_triggers`, `probes`, `instrumentation`, the MBean tools (`list_mbean_servers`, `connect_jmx`, `list_mbeans`,
`get_mbean`, `invoke_mbean`, `track_mbean`, `subscribe_mbean`, `mbean_notifications`), plus
`decompile`, `search_code` and `find_usages` for apps opened in the decompiler.

### Use it from your IDE

Both plugins find a running `openprofiler daemon` through `~/.openprofiler/daemon.json`. They tint
hot lines with their share of total CPU time, open the source when you pick a frame in the
profiler UI, and launch Java programs with the agent. A launched JVM opens in the daemon as a live
session, and the heat refreshes while it runs.

- **IntelliJ IDEA** (2025.3+): build with `(cd ide/intellij && ./gradlew buildPlugin)` and install
  `build/distributions/openprofiler-intellij-*.zip` with *Settings | Plugins | Install Plugin from
  Disk*. Use **Run | Profile with OpenProfiler** on any Java run configuration (application, JUnit,
  Spring Boot, …). Settings are under *Tools | OpenProfiler*, actions under *Tools | OpenProfiler*.
- **VS Code / VSCodium**: build with `(cd ide/vscode && npm ci && npm run package)` and install the
  `.vsix` (*Extensions: Install from VSIX…*). Launching needs the Debugger for Java extension (on
  the VS Code Marketplace and Open VSX). Run **OpenProfiler: Launch with Agent**, or add
  `"openprofiler": true` to a Java launch configuration in `launch.json`.

Both use the agent next to the daemon; set the agent path in the plugin settings to use another one.

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
scripts/build-bundle.sh               # Linux relay + agents (x86_64, aarch64, armv7) → dist/bundle; needs cargo-zigbuild
scripts/build-bundle.sh --docker      # the same in a builder container (e.g. on macOS)
```

**Tests**

```sh
cargo test --workspace                                                   # incl. live agent end-to-end tests (need java)
MAT_HOME=/path/to/mat cargo test -p openprofiler-heap --test mat_parity  # retained sizes vs Eclipse MAT (Docker)
java/matrix/run.sh                    # probes against real libraries and versions (Docker, JDK 17+)
java/bench/run.sh                     # agent overhead on Spring Petclinic and Quarkus (java/bench/linux.sh: on Linux)
scripts/smoke-bundle.sh               # run the bundle's relays and agents under emulation (Docker)
scripts/e2e-remote.sh                 # attach over ssh, docker exec and to a distroless pod in kind
(cd ui && npm run lint && npm run typecheck && npm test)
```

**Releases**

Bump `version` in `Cargo.toml`, then push a matching `vX.Y.Z` tag. The release workflow builds
the archives, publishes the GitHub release with `SHA256SUMS`, pushes
`ghcr.io/matusso/openprofiler`, and updates the
[Homebrew tap](https://github.com/matusso/homebrew-tap) (needs the `HOMEBREW_TAP_DEPLOY_KEY`
secret). Pre-release tags (`v0.2.0-rc1`) skip the tap and the `latest` image tag.

## Repository layout

| Path | Contents |
|---|---|
| `crates/openprofiler-agent`, `crates/jvmti-sys` | Native JVMTI agent and bindings |
| `crates/openprofiler-model` | Symbols, stacks, samples; call tree, hot spots, flame, call graph, diff |
| `crates/openprofiler-jfr` | JFR parser |
| `crates/openprofiler-hprof`, `crates/openprofiler-phd`, `crates/openprofiler-heap` | HPROF and PHD parsers, heap analysis engine |
| `crates/openprofiler-snapshot` | `.opsnap` reader and writer ([spec](docs/spec/snapshot-format.md)) |
| `crates/openprofiler-proto`, `crates/openprofiler-link` | Agent-link protocol ([spec](docs/spec/agent-link.md)) and its daemon-side client |
| `crates/openprofiler-attach` | JVM discovery and dynamic attach, including containers |
| `crates/openprofiler-remote`, `crates/openprofiler-relay` | ssh, docker and kubectl transports; the static relay binary |
| `crates/openprofiler-daemon` | Query API server, insights, exporters, MCP, IDE bridge |
| `crates/openprofiler-cli` | The `openprofiler` binary |
| `crates/openprofiler-dex` | DEX/APK and class-file/JAR readers, smali and bytecode views, the Java decompiler, Android resource decoding |
| `java/` | Boot classes, public `openprofiler-api`, test workloads, the probe version matrix (`java/matrix`) and the overhead benchmark (`java/bench`) |
| `ui/` | React + TypeScript UI, Tauri desktop shell |
| `ide/intellij` | IntelliJ IDEA plugin (Kotlin) |
| `ide/vscode` | VS Code / VSCodium extension |

## Project status

OpenProfiler is in early development. The offline analyzer, the live agent, probes and bytecode
instrumentation, and most of remote attach work today. The probes are tested against 49 library
and version combinations (`java/matrix`). On Spring Petclinic the agent with probes costs about
1% throughput and no measurable CPU per request; on the Quarkus quickstart about 3% throughput and
24% CPU per request (its requests take 0.1 ms; see the
[benchmark](docs/DESIGN.md#4-roadmap)).

| Phase | State |
|---|---|
| Foundations | Done |
| Offline analyzer (HPROF, JFR, UI, CLI, MCP) | Done |
| Live agent (sampling, allocations, locks, telemetry, dumps, triggers) | Done |
| Probes and instrumentation | Done (Quarkus CPU overhead open) |
| Remote and cloud | Mostly done |
| Polish and ecosystem | Partial |

Planned work is tracked in [GitHub issues](https://github.com/matusso/openprofiler/issues).

## Licence

Apache-2.0, see [LICENSE](LICENSE). The native agent (`crates/openprofiler-agent`) is MPL-2.0, see
[its LICENSE](crates/openprofiler-agent/LICENSE).
