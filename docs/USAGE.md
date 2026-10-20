# Usage guide

- [Analyse a file](#analyse-a-file)
- [Profile a running JVM](#profile-a-running-jvm)
- [Profile a JVM on another machine, in a container or in Kubernetes](#profile-a-jvm-on-another-machine-in-a-container-or-in-kubernetes)
- [Share a session read-only](#share-a-session-read-only)
- [Start a JVM with the agent](#start-a-jvm-with-the-agent)
- [Probes and exact call counts](#probes-and-exact-call-counts)
- [Decompile Android apps and jars](#decompile-android-apps-and-jars)
- [Compare, export and gate in CI](#compare-export-and-gate-in-ci)
- [Use it from an AI agent (MCP)](#use-it-from-an-ai-agent-mcp)
- [Use it from your IDE](#use-it-from-your-ide)

## Analyse a file

```sh
openprofiler open recording.jfr                       # summary
openprofiler hotspots recording.jfr --kind alloc      # top allocation sites
openprofiler analyze heap.hprof                       # build the heap index, print insights
openprofiler daemon --web heap.hprof                  # explore it in the UI
```

Supported inputs: `.jfr`, `.hprof`, `.hprof.gz`, OpenJ9 / IBM Semeru `.phd` heap dumps (also
gzipped; references and sizes only, see [design](DESIGN.md)), `.opsnap` snapshots and `.oplink`
offline recordings.

## Profile a running JVM

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

## Profile a JVM on another machine, in a container or in Kubernetes

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

No port is opened on the target: OpenProfiler uses your own `ssh`, `docker` and `kubectl`
credentials and tunnels the agent link through a small relay. Distroless and other shell-less
images are reached with `--debug-container`. See [Architecture](ARCHITECTURE.md#remote-attach)
for the details.

In the UI, **Attach** has Local, SSH, Docker and Kubernetes tabs. On the Kubernetes tab,
double-clicking a pod attaches to its JVM, so a pod on a remote cluster is three clicks away. The
debug container is preselected for distroless and Chainguard images.

## Share a session read-only

In web mode (`openprofiler daemon --web`), **Share read-only link…** in the session menu creates a
link that opens just that session, optionally expiring after an hour, a day or a week. The
holder can browse every view and export, but can't attach, record, dump, open files, use MBeans
or see other sessions. Links are revoked from the same dialog or when the daemon stops.

## Start a JVM with the agent

```sh
java -agentpath:/path/to/libopenprofiler_agent.so=port=0 -jar app.jar
```

The agent listens on loopback and publishes its port and token in
`$TMPDIR/openprofiler-<pid>.json`; `openprofiler attach <pid>` and the UI pick it up. To record
without a daemon, use offline mode: `=mode=offline,file=app.oplink`. Options are also read from
`OPENPROFILER_AGENT_OPTIONS`. See [the agent-link spec](spec/agent-link.md) for all options
and the trigger file format.

Where profiling signals are not allowed, `sampler=jfr` takes CPU samples from a JFR stream
(JDK 14+; on JDK 25+ on Linux, JFR's CPU-time sampler) instead. On Linux the agent also switches
to it when the async sampler cannot start.

## Probes and exact call counts

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

## Decompile Android apps and jars

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

## Compare, export and gate in CI

```sh
openprofiler compare base.opsnap head.opsnap --fail-if "cpu.total > +10%"   # exit 1 on regression
openprofiler export app.opsnap --view flame --format svg -o flame.svg      # csv|json|html|svg|collapsed
openprofiler convert recording.jfr -o recording.opsnap
openprofiler arrow app.opsnap -o tables/     # Arrow IPC tables for DuckDB, Polars, pyarrow
```

## Use it from an AI agent (MCP)

```sh
openprofiler mcp        # MCP server on stdio
```

Register the command with any MCP client. Tools include `list_jvms`, `attach`, `load_snapshot`,
`hotspots`, `expand`, `heap_dump`, `heap_data`, `insights`, `compare`, `thread_dump`,
`set_triggers`, `probes`, `instrumentation`, the MBean tools (`list_mbean_servers`, `connect_jmx`, `list_mbeans`,
`get_mbean`, `invoke_mbean`, `track_mbean`, `subscribe_mbean`, `mbean_notifications`), plus
`decompile`, `search_code` and `find_usages` for apps opened in the decompiler.

## Use it from your IDE

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
