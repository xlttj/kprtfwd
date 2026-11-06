# kprtfwd

A terminal UI and command line tool for Kubernetes port forwards.

kprtfwd keeps one list of all your forwards across all kubeconfig contexts.
A small background daemon does the port-forwarding itself, without `kubectl`
processes:

- A forward follows its service's endpoints, so pod restarts and scaling do
  not break it.
- After a VPN drop or an API server outage, forwards are ready again within
  seconds of the server coming back.
- Forwards to HTTP services keep a connection open ahead of time, so browsers
  and other clients with many short connections stay fast.

Version 2 is a rewrite in Rust. Version 1 (Go, `kubectl port-forward`) is in
the git history.

## Install

**Release binaries** for macOS (arm64, x86_64) and Linux (x86_64, arm64) are
attached to the [GitHub releases](https://github.com/xlttj/kprtfwd/releases)
from version 2.0.0 on; earlier releases contain version 1. Download the one
for your system (`kprtfwd-darwin-arm64`, `kprtfwd-darwin-amd64`,
`kprtfwd-linux-amd64` or `kprtfwd-linux-arm64`), for example:

```bash
curl -fLO https://github.com/xlttj/kprtfwd/releases/latest/download/kprtfwd-darwin-arm64
chmod +x kprtfwd-darwin-arm64
mv kprtfwd-darwin-arm64 /usr/local/bin/kprtfwd
```

The Linux binaries are static and run on any distribution.

The macOS binaries are not signed. macOS blocks a binary downloaded with a
browser when it first starts; a download with `curl` is not affected. After a
browser download, remove the quarantine flag before the first start:

```bash
xattr -d com.apple.quarantine kprtfwd-darwin-arm64
```

`checksums.txt` in each release lists the SHA-256 checksum of every binary.

**From source**, with a stable Rust toolchain:

```bash
cargo install --locked --git https://github.com/xlttj/kprtfwd kprtfwd
```

The Homebrew tap `xlttj/kprtfwd` still installs version 1.

kprtfwd reads the kubeconfig like `kubectl` does (`KUBECONFIG` or
`~/.kube/config`). `kubectl` itself is only needed if your kubeconfig logs in
through a plugin such as `kubectl oidc-login`.

## Usage

`kprtfwd` without arguments opens the TUI. It starts the daemon if none is
running.

| Key | Action |
|---|---|
| `↑` `↓` `j` `k` `PgUp` `PgDn` `g` `G` | move |
| `space` | switch the selected forward on or off |
| `/` | filter: all words must match; `@name` selects a project |
| `esc` | clear the filter |
| `+` `-` | switch all listed forwards on / off |
| `p` | projects |
| `o` `y` | open in the browser / copy `localhost:port` |
| `e` `a` `x` | edit / add / delete a forward |
| `d` | discover the services of a context and add forwards for them |
| `P` | pause or resume all forwards |
| `r` | reconnect the selected forward's context |
| `l` | events of the selected forward |
| `?` | help |
| `q` | quit |

Quitting the TUI switches all forwards off and stops the daemon, as in
version 1. To keep forwards running in the background, see
[Settings](#settings).

The INFO column says why a forward is not ready: the API server is
unreachable, a login is needed, no pod is ready, the local port is taken (and
by which process), and so on.

## Command line

| Command | |
|---|---|
| `kprtfwd status [--all] [--json]` | contexts and enabled forwards |
| `kprtfwd up <project or words>` | switch forwards on |
| `kprtfwd down <project or words>` | switch forwards off |
| `kprtfwd switch <project>` | switch a project's forwards on and all others off |
| `kprtfwd pause` / `resume` | close all listeners / bring them back |
| `kprtfwd reconnect <context>` | reconnect a context from scratch |
| `kprtfwd logs [-f] [project or words]` | events of forwards, or the daemon's log |
| `kprtfwd prune [--context C] [--namespace 'app-*'] [-y]` | delete forwards whose service is gone |
| `kprtfwd daemon status` / `stop` / `restart` / `run` | control the daemon; `run` keeps it in the foreground |
| `kprtfwd db list` | list forwards and projects without the daemon |

Words select the forwards that contain every one of them, ignoring case, in
their ID, context, `namespace/service:port` or local port, for example
`kprtfwd up shop api`. The TUI filter works the same way.

Exit codes, for scripts:

| Code | |
|---|---|
| 0 | ok |
| 1 | error |
| 2 | wrong usage, e.g. an unknown flag |
| 3 | `status` only: an enabled forward is not ready |
| 4 | the daemon is not reachable: it could not be started, does not answer, or went away during the command |

## Settings

kprtfwd keeps its files in `~/.config/kprtfwd`, or `$XDG_CONFIG_HOME/kprtfwd`
if that is set (`$KPRTFWD_HOME` overrides both):

| File | |
|---|---|
| `config.toml` | settings |
| `kprtfwd.db` | forwards and projects (SQLite) |
| `daemon.sock`, `daemon.pid` | the daemon's socket and process ID |
| `logs/` | the daemon's log, one file per day |

`config.toml` is created on the first start:

```toml
# kprtfwd settings

# What quitting the TUI does to the forwards:
#   false  switch them all off, also those switched on with `kprtfwd up`,
#          as kprtfwd v1 did
#   true   keep them running in the background
keep_forwards_running = false
```

On its first start, kprtfwd 2 copies the database of version 1 from
`~/.kprtfwd`. Version 1 keeps its directory and keeps working.

## Logins through exec plugins

Some kubeconfigs log in through an exec plugin, for example `kubectl
oidc-login`, which opens the browser. The daemon runs such a plugin only while
the API server answers, so nothing opens while you are off the VPN, and only
one login at a time across all contexts. While a login waits for you, the
context and its forwards show `logging in`. If a login fails or nobody
completes it, the forward shows `login needed`; press `r` (or run
`kprtfwd reconnect <context>`) to log in again.

## Troubleshooting

- `kprtfwd logs -f` follows the daemon's log; `l` in the TUI shows a forward's
  events.
- `KPRTFWD_LOG=debug kprtfwd daemon run` runs the daemon in the foreground
  with more detail. Stop the running daemon first (`kprtfwd daemon stop`).
- `kprtfwd daemon restart` starts the daemon fresh; enabled forwards come back.

## Development

```bash
cargo test --workspace
```

The cluster tests in `crates/engine/tests/cluster.rs` run against a
throwaway cluster, for example kind:

```bash
KPRTFWD_TEST_CONTEXT=kind-kprtfwd cargo test -p kprtfwd-engine --test cluster -- --test-threads=1
```

The design is described in [docs/architecture.md](docs/architecture.md).

## License

MIT, see [LICENSE](LICENSE).
