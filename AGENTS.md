This repository keeps agent knowledge and run state in gledger. Run `gledger guide` before working here and follow it.

## Project Overview

**kprtfwd** manages Kubernetes port forwards from a terminal UI and a CLI. Version 2 is written in Rust: a background daemon does the port-forwarding natively through the Kubernetes API (no `kubectl` processes), and the TUI and CLI talk to it over a Unix socket. Data lives in SQLite. Version 1 (Go, Bubble Tea, `kubectl port-forward`) is only in the git history.

The design, decisions and status are in `docs/architecture.md`.

## Development Commands

### Git Commit

Always use conventional commit messages with a maximum line length of 72 characters.
Always use "axel@tietje.eu" as the commiters email adress.
Always use "Axel Tietje" as the commiters name.

### Building and Running
```bash
# Rust comes from Homebrew's keg-only rustup
export PATH=/opt/homebrew/opt/rustup/bin:$PATH

cargo build --release            # target/release/kprtfwd
target/release/kprtfwd           # TUI; starts the daemon on demand
target/release/kprtfwd status    # CLI

# Daemon in the foreground, logging to the terminal with more detail
target/release/kprtfwd daemon stop
KPRTFWD_LOG=debug target/release/kprtfwd daemon run --foreground
```

### Checks (as in CI)
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo audit
```

### Cluster Tests
They run only when `KPRTFWD_TEST_CONTEXT` names a throwaway cluster, e.g. kind. `KUBECONFIG` must be absolute, since cargo runs the tests in `crates/engine`:
```bash
kind create cluster --name kprtfwd --kubeconfig target/kind-kubeconfig
KUBECONFIG=$PWD/target/kind-kubeconfig KPRTFWD_TEST_CONTEXT=kind-kprtfwd \
  KPRTFWD_TEST_PAUSE='docker pause kprtfwd-control-plane' \
  KPRTFWD_TEST_RESUME='docker unpause kprtfwd-control-plane' \
  cargo test -p kprtfwd-engine --test cluster -- --test-threads=1 --nocapture
kind delete cluster --name kprtfwd
```
Never touch `~/.kube/config` for tests.

## Architecture Overview

Cargo workspace at the repository root:

- `crates/core`: domain types, state derivation, filter, SQLite store with the v1 migration, settings (`config.toml`), paths. No async, no Kubernetes.
- `crates/engine`: one actor per kubeconfig context (client, health probe, logins through exec plugins, watch caches of Services and EndpointSlices) and one per forward (listener, pod choice, warm stream); own implementation of the `v4.channel.k8s.io` port-forward protocol on top of kube-rs.
- `crates/proto`: IPC messages (line-delimited JSON over the Unix socket), protocol version, client with `connect_or_start` and `stop_daemon`.
- `crates/daemon`: process lifecycle, IPC server, store <-> engine wiring, kubeconfig watching, discovery and prune.
- `crates/tui`: ratatui app (`App::update` for keys, `view.rs` for drawing).
- `crates/cli`: clap commands, thin clients of the daemon.
- `kprtfwd`: the binary; dispatches to TUI, CLI or daemon. `kprtfwd/tests/daemon.rs` tests the daemon's life cycle through the real binary.

Data directory: `$KPRTFWD_HOME`, else `$XDG_CONFIG_HOME/kprtfwd`, else `~/.config/kprtfwd` (database, `config.toml`, socket, PID file, logs). The first start copies the v1 database from `~/.kprtfwd`.

## Development Guidelines

- **Test isolation:** tests must set `KPRTFWD_HOME` (or `HOME`) to a temporary directory and never use the real data directory or the user's running daemon. Never call `connect_or_start` from a test process; start daemons through the binary with an isolated environment.
- **Socket paths** may be at most 103 bytes (macOS). Keep test directories short, e.g. under `CARGO_TARGET_TMPDIR`.
- **Exec plugins** (e.g. `kubectl oidc-login`) are only run by the context's health probe, one at a time and only while the API server answers; never let kube-rs run them inside requests. Messages must never contain the plugin's arguments (client secrets).
- **No internal names:** fixtures and docs use neutral names (shop, frontend, staging); never names of real clusters, hosts or services.
- **UI:** one list across all contexts, single keys, information inline; no panes, mouse, palettes or notifications.
- **TUI tests:** key-by-key tests of `App::update` and `TestBackend` drawings; for whole flows `expect` with `set stty_init "rows 40 cols 140"`, single-word patterns and Ctrl+L before checking (ratatui only redraws changed cells).

## Release Process

1. Set `version` in the root `Cargo.toml` to the release version, update `Cargo.lock` with a build, and merge to `main`.
2. Tag that commit `v<version>` on `main` and push the tag. `.github/workflows/release.yml` checks that the tag matches the crate version, runs the whole CI on the tagged commit, builds `kprtfwd` for macOS (arm64, x86_64) and static musl Linux (x86_64, arm64), and creates a GitHub release with the binaries and `checksums.txt`. Changes to the workflow, `Cargo.lock` or a `Cargo.toml` are built (not released) in pull requests.
3. Update the Homebrew tap by hand: `Formula/kprtfwd.rb` in `xlttj/homebrew-tap` (`brew install xlttj/tap/kprtfwd`) gets the new version, URLs and the SHA-256 sums from `checksums.txt`.
