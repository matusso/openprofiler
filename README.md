# Ember

An open-source JVM profiler: CPU, allocations, heap analysis, threads, locks, telemetry,
snapshots, comparisons, remote and container attach, and an MCP server. It is written in Rust
(agent, engines, daemon, CLI) with a Java boot shim and a React/Tauri UI. The design is in
[docs/OPEN_PROFILER_PLAN.md](docs/OPEN_PROFILER_PLAN.md).

```
ember-ui (Tauri / browser) ──Query API (HTTP+WS)──► ember-daemon ──agent-link (TLS+token)──► libember_agent + boot classes (in the JVM)
                                                    ├─ ember-jfr      JFR import
                                                    ├─ ember-heap     out-of-core HPROF analysis (MAT-exact dominators)
                                                    ├─ ember-snapshot .ember container
                                                    └─ ember-remote   ssh / docker / kubectl ──► ember-relay (static helper)
```

## Requirements

- Rust (stable, via rustup). With Homebrew's keg-only rustup, run `fish_add_path /opt/homebrew/opt/rustup/bin` (or add it to `PATH`).
- JDK 11+ for the boot classes and the tests (JDK 21+ recommended).
- Node 22+ and npm for the UI.
- Docker (optional) for Linux builds, the remote bundle and the MAT parity test.

## Build and run

```sh
cargo build --release                       # every crate, incl. the agent (target/release/libember_agent.*)
java/build.sh                               # ember-boot.jar / ember-api.jar (optional; the agent embeds the boot classes)
(cd ui && npm ci && npm run build)          # web UI → ui/dist

target/release/ember daemon --web --demo    # prints http://127.0.0.1:7878/?token=…
(cd ui && npm run tauri build)              # desktop app (embeds the daemon)
scripts/build-bundle.sh                     # Linux relay + agents for remote/container attach → dist/bundle
```

## CLI

```sh
ember open recording.jfr                    # summary (JFR, HPROF, .hprof.gz, .ember, .emberlink)
ember hotspots recording.jfr --kind alloc
ember export app.ember --view flame --format svg -o flame.svg   # csv|json|html|svg|collapsed
ember analyze heap.hprof                    # build the heap index + print insights
ember compare base.ember head.ember --fail-if "cpu.total > +10%"   # CI regression gate (exit 1)
ember convert recording.jfr -o recording.ember
ember list                                  # local JVMs
ember attach <pid> --duration 10s           # live hot spots
ember record --pid <pid> --duration 60s --heap -o out.ember
ember dump <pid> -o heap.hprof
ember mcp                                   # MCP server on stdio
ember daemon [--web] [--demo] [files…]
```

To start a JVM with the agent directly, use `-agentpath:/path/libember_agent.so=port=0`. Offline
mode is `=mode=offline,file=app.emberlink`. See [docs/spec/agent-link.md](docs/spec/agent-link.md).

## Layout

| Path | What |
|---|---|
| `crates/ember-model` | Symbols, stack trie, columnar samples; call tree, hot spots, flame, call graph, diff engine |
| `crates/ember-jfr` | Metadata-driven JFR parser (about 650 MB/s) mapped to `ember-model` |
| `crates/ember-hprof`, `crates/ember-heap` | Zero-copy HPROF parser; mmap'd CSR graphs, Semi-NCA dominators, retained sizes, inspections, persistent index (10 GB dump: about 25 s cold, under 5 ms warm) |
| `crates/ember-snapshot` | `.ember` container ([spec](docs/spec/snapshot-format.md)) |
| `crates/jvmti-sys`, `crates/ember-agent` | JVMTI bindings; native agent (async sampler with AsyncGetCallTrace on Linux, sync sampler elsewhere, allocation and monitor events, telemetry, heap and thread dumps) |
| `crates/ember-proto`, `crates/ember-link` | Agent-link protocol (protobuf, zstd frames, TLS 1.3 with pinned fingerprint) and the daemon-side client |
| `crates/ember-attach` | hsperfdata discovery, HotSpot dynamic attach, container `NSpid` handling, `jcmd` |
| `crates/ember-remote`, `crates/ember-relay` | ssh, docker and kubectl transports (incl. ephemeral debug containers), verified uploads, stdio tunnels; the static helper binary |
| `crates/ember-daemon` | Sessions, Query API v1 ([spec](docs/spec/query-api.md)), insights, exporters, event browser, MCP, IDE bridge |
| `crates/ember-cli` | The `ember` binary |
| `java/` | Boot classes (telemetry, dumps, Controller), public `ember-api`, test workload |
| `ui/` | React 19 + TypeScript UI, Tauri 2 desktop shell |
| `ide/vscode` | VS Code extension (CPU gutter heat, navigate-to-source) |

## Tests

```sh
cargo test --workspace                      # incl. live agent end-to-end tests (need java)
MAT_HOME=/path/to/mat cargo test -p ember-heap --test mat_parity   # exact parity with Eclipse MAT (Docker)
(cd ui && npm run lint && npm run typecheck && npm test)
```

## Status against the plan

| Phase | State |
|---|---|
| 0 Foundations | Done: workspace, CI workflow, JVMTI bindings, protobuf specs, `.ember` spec |
| 1 Offline analyzer | Done: HPROF/heap engine with exact MAT parity, 10 GB under 3 min cold and under 5 s warm; JFR; Query API; UI; CLI; MCP |
| 2 Live agent | Mostly done: sampling (async on Linux, sync elsewhere), allocation and monitor events, telemetry, thread and heap dumps, TLS + token link, reconnect, local attach, offline recording. Missing: triggers, `j.u.c` park hooks, virtual-thread mount events, monitor owners |
| 3 Probes and instrumentation | Not started: no bytecode instrumentation, probes (JDBC, HTTP, …), or custom-probe API yet |
| 4 Remote and cloud | Mostly done: relay, ssh/docker/kubectl transports, ephemeral debug containers, verified upload, tunnelled attach (tested end to end with Docker), web mode with token. Not yet: shareable read-only links, armhf/amd64 bundles built in CI |
| 5 Polish | Partial: VS Code extension, insights, IDE bridge. Not yet: native classfile rewriter, native heap dumper, call tracer, MBean browser, IntelliJ plugin, Windows agent, PHD import, Arrow IPC sections |

## Licence

Apache-2.0. Clean-room: no code, names, formats or assets from commercial profilers.
