<div align="center">

  <h1 align="center">fractal-cli</h1>

[Website](https://fractal.omni.dev) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**fractal-cli** is the command-line client for [Fractal](https://fractal.omni.dev), the GitOps platform for deploying applications to Kubernetes. The installed binary is named `fractal`.

It talks to two things: your cluster's Kubernetes apiserver (via your kubeconfig) for day-to-day operations on `FractalProject` and `FractalService` resources, and the Fractal control-plane API for auth and cluster onboarding. It is the primary entry point for self-hosters bringing their own cluster (BYOC).

## Installation

The crate is published to Omni's Manifold cargo registry. Add the registry to `~/.cargo/config.toml` once:

```toml
[registries.manifold]
index = "sparse+https://api.manifold.omni.dev/cargo/omnidotdev/index/"

[registry]
global-credential-providers = ["cargo:token"]
```

Then install:

```sh
cargo install fractal-cli --registry manifold
```

Homebrew and AUR packages are not yet available.

### Build from source

```sh
git clone https://github.com/omnidotdev/fractal-cli
cd fractal-cli
cargo build --release
# binary will be at target/release/fractal
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

## Authentication

`fractal auth login` prompts for your email and password, exchanges them for a bearer token, and persists it (mode 0600 on Unix) at `$XDG_CONFIG_HOME/fractal/config.toml` (defaulting to `~/.config/fractal/config.toml`). `fractal auth whoami` prints the stored identity and `fractal auth logout` clears the token.

For CI and automation, set `FRACTAL_AUTH_TOKEN` instead of logging in interactively.

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
