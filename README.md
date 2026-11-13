<div align="center">

  <h1 align="center">fractal-cli</h1>

[Website](https://fractal.omni.dev) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**fractal-cli** is the command-line client for [Fractal](https://fractal.omni.dev), the GitOps platform for deploying applications to Kubernetes. The installed binary is named `fractal`.

It talks to two things: your cluster's Kubernetes apiserver (via your kubeconfig) for day-to-day operations on `FractalProject` and `FractalService` resources, and the Fractal control-plane API for auth and cluster onboarding. It is the primary entry point for self-hosters bringing their own cluster (BYOC).

## Installation

### Prebuilt binary (recommended)

Every push to `master` publishes `fractal` binaries for Linux (glibc) and macOS (amd64 + arm64) to this repo's [GitHub Releases](https://github.com/omnidotdev/fractal-cli/releases), tagged `cli-v<build-number>`. Download the one for your platform, mark it executable, and put it on your `PATH`.

```sh
# Linux (amd64)
curl -sSfL https://github.com/omnidotdev/fractal-cli/releases/latest/download/fractal-linux-amd64 -o fractal

# Linux (arm64)
curl -sSfL https://github.com/omnidotdev/fractal-cli/releases/latest/download/fractal-linux-arm64 -o fractal

# macOS (Apple Silicon / arm64)
curl -sSfL https://github.com/omnidotdev/fractal-cli/releases/latest/download/fractal-darwin-arm64 -o fractal

# macOS (Intel / amd64)
curl -sSfL https://github.com/omnidotdev/fractal-cli/releases/latest/download/fractal-darwin-amd64 -o fractal

chmod +x fractal
sudo mv fractal /usr/local/bin/fractal
fractal --version
```

To pin a specific build, swap `latest/download` for `download/cli-v<build-number>` (e.g. `download/cli-v42`); the available builds are listed on the releases page.

Homebrew, AUR, and cargo-registry packages are not yet available.

### Build from source

The CLI lives in the `fractal-stack` monorepo under `services/fractal-cli` (it has a path dependency on the sibling `services/fractal-operator` crate, so build it from inside the checkout, not as a standalone clone). The monorepo and operator are private, so building from source requires Omni org access; everyone else should use the prebuilt binaries above.

```sh
git clone git@github.com:omnidotdev/fractal-stack.git
cd fractal-stack/services/fractal-cli
cargo build --release
# binary will be at target/release/fractal (relative to this directory)
```

## Quick Start

```sh
# authenticate against the Fractal control plane
fractal auth login

# list what is deployed in the current namespace
fractal services

# tail a service's logs
fractal logs my-service --follow
```

Run `fractal --help` (or `fractal <command> --help`) for the full command list, which includes `scale`, `restart`, `rollback`, `destroy`/`restore`, `env`, `logs`, `exec`, `port-forward`, `db-connect`, `secrets-sync`, `members`, `projects`, `services`, `deploy`, `canary`, `tokens`, `cluster`, and shell `completions`.

## Machine-readable output (`--json`)

Pass the global `--json` flag to get structured JSON instead of the human-formatted tables, for scripting and agent/CI use. It is honored by the read commands:

```sh
fractal --json services                 # JSON array of services
fractal --json projects list            # JSON array of projects
fractal --json projects get my-project  # JSON object for one project
fractal --json auth whoami              # { authenticated, source, email, ... }
fractal --json logs my-service          # stream metadata line, then NDJSON log lines
```

The `services` JSON rows surface a few fields the human table omits, so CI can discover a service's shape without cluster access to the raw `FractalService`: `serviceType` (`web`, `worker`, `cronJob`, `staticSite`, `database`), the monorepo build subdir `rootDir`, the webhook `watchPaths`, and the cron `schedule`. Commands without a `--json` implementation keep their human output.

> The CLI reads and operates existing services (including monorepo and cron ones); it does not create them. Declare a new service's `serviceType: cronJob` + `schedule`, or its monorepo `rootDir`/`watchPaths`, in a repo-committed `fractal.yaml` or through the dashboard, then deploy it. The read commands above then expose those fields for scripting.

## Authentication

`fractal auth login` prompts for your email and password, exchanges them for a bearer token, and persists it (mode 0600 on Unix) at `$XDG_CONFIG_HOME/fractal/config.toml` (defaulting to `~/.config/fractal/config.toml`). `fractal auth whoami` prints the stored identity and `fractal auth logout` clears the token.

### CI and automation

Set `FRACTAL_AUTH_TOKEN` to a service-account token (mint one with `fractal tokens create`) instead of logging in interactively. The env token takes precedence over any persisted login and is picked up by every command that calls the control-plane API (`deploy`, `cluster connect`, and so on). When it is set, `fractal auth login` detects it and returns immediately without prompting, so it is safe to call unattended; `fractal auth whoami` reports `source: "env"`. A typical CI step:

```sh
export FRACTAL_AUTH_TOKEN="$FRACTAL_CI_TOKEN"
fractal --json auth whoami   # sanity-check the token is accepted
fractal deploy --namespace fractal-myproject --project myproject --service my-api
```

## Configuration

The CLI is driven by environment variables so it stays stateless outside the auth config file:

| Variable | Purpose | Default |
| --- | --- | --- |
| `FRACTAL_API_URL` | Base URL of the Fractal control-plane API (used to mint install tokens and trigger rebuilds). | `https://api.fractal.omni.dev` |
| `FRACTAL_AUTH_URL` | Base URL of the identity provider used by `fractal auth login`. Point at a non-prod instance for dev/staging. | `https://identity.omni.dev` |
| `FRACTAL_AUTH_TOKEN` | Bearer token override for CI/automation. Takes precedence over the persisted login token. | (unset) |
| `KUBECONFIG` | Kubeconfig path used for cluster operations. The first entry wins when colon-separated. | `~/.kube/config` |

## Self-hosting: connect a cluster

`fractal cluster connect` onboards a Kubernetes cluster to Fractal. It:

1. Resolves your kubeconfig (from `--kubeconfig`, then `KUBECONFIG`, then `~/.kube/config`), validates it, and sanity-pings the apiserver to confirm connectivity and report reachable nodes.
2. When authenticated (`fractal auth login`, or `FRACTAL_AUTH_TOKEN`), mints a single-use install token from the control-plane API and prints a signed install URL. Otherwise it falls back to the static open-source install bundle URL.
3. Prints the `kubectl apply` command to install the fractal-operator into the `fractal-system` namespace.

```sh
# onboard the current kubeconfig context
fractal cluster connect --name us-east-prod

# bring your own registry (the pull credential stays with you and never transits Omni)
fractal cluster connect --name us-east-prod --registry registry.example.com/myorg
```

After applying the printed manifest, verify the operator is running:

```sh
fractal cluster status
```

Related cluster commands: `fractal cluster list-tokens`, `fractal cluster revoke-token <id>`, and `fractal cluster disconnect`.

## Development

### Prerequisites

- [Rust](https://rustup.rs) 1.85+

### Commands

```sh
cargo build            # build
cargo run -- --help    # run
cargo test             # test
cargo clippy           # lint (pedantic + all denied)
cargo fmt              # format
```

## License

The code in this repository is licensed under Apache 2.0, &copy; [Omni LLC](https://omni.dev). See [LICENSE.md](LICENSE.md) for more information.
