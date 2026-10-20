# Building from source

## Requirements

- Rust stable (via rustup)
- JDK 11+ (21+ recommended) for the boot classes and tests
- Node 22+ and npm for the UI
- Docker (optional) for Linux builds, the remote bundle and the MAT parity test

## Build

```sh
cargo build --release                 # all crates, incl. target/release/libopenprofiler_agent.*
java/build.sh                         # openprofiler-boot.jar / openprofiler-api.jar (optional; the agent embeds the boot classes)
(cd ui && npm ci && npm run build)    # web UI → ui/dist
(cd ui && npm run tauri build)        # desktop app (embeds the daemon)
scripts/build-bundle.sh               # Linux relay + agents (x86_64, aarch64, armv7) → dist/bundle; needs cargo-zigbuild
scripts/build-bundle.sh --docker      # the same in a builder container (e.g. on macOS)
```

Try the UI with a synthetic demo session:

```sh
target/release/openprofiler daemon --web --demo
# → open the printed http://127.0.0.1:7878/?token=… URL
```

## Tests

```sh
cargo test --workspace                                                   # incl. live agent end-to-end tests (need java)
MAT_HOME=/path/to/mat cargo test -p openprofiler-heap --test mat_parity  # retained sizes vs Eclipse MAT (Docker)
java/matrix/run.sh                    # probes against real libraries and versions (Docker, JDK 17+)
java/bench/run.sh                     # agent overhead on Spring Petclinic and Quarkus (java/bench/linux.sh: on Linux)
java/bench/suites.sh                  # agent overhead at 100 Hz async sampling on Renaissance and DaCapo (Linux; else in Docker)
java/bench/soak.sh                    # 24 h soak under load: resident memory, open files, crashes (SOAK_HOURS=0.25 for a quick one)
crates/openprofiler-heap/scripts/bench.sh 1 10 50   # heap engine on synthetic 1, 10 and 50 GB dumps
scripts/smoke-bundle.sh               # run the bundle's relays and agents under emulation (Docker)
scripts/e2e-remote.sh                 # attach over ssh, docker exec and to a distroless pod in kind
(cd ui && npm run lint && npm run typecheck && npm test)
```

The overhead, soak and heap benchmarks run weekly in the `Benchmarks` workflow, which tracks the
results in the `gh-pages` branch (`bench/`). Hosted runners soak for 5 h and index a 1 GB dump;
for the full 24 h soak and the 10 and 50 GB dumps, run the workflow by hand on a self-hosted
runner (`runner`, `soak_hours`, `heap_sizes` inputs).

## IDE plugins

- **IntelliJ IDEA**: `(cd ide/intellij && ./gradlew buildPlugin)`, then install
  `build/distributions/openprofiler-intellij-*.zip` with *Settings | Plugins | Install Plugin from
  Disk*.
- **VS Code / VSCodium**: `(cd ide/vscode && npm ci && npm run package)`, then install the `.vsix`
  (*Extensions: Install from VSIX…*).

## Releases

Bump `version` in `Cargo.toml`, then push a matching `vX.Y.Z` tag. The release workflow builds
the archives, publishes the GitHub release with `SHA256SUMS`, pushes
`ghcr.io/matusso/openprofiler`, and updates the
[Homebrew tap](https://github.com/matusso/homebrew-tap) (needs the `HOMEBREW_TAP_DEPLOY_KEY`
secret). Pre-release tags (`v0.2.0-rc1`) skip the tap and the `latest` image tag.
