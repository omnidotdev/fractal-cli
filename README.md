<div align="center">

  <h1 align="center">fractal-cli</h1>

[Website](https://fractal.omni.dev) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**fractal-cli** is the command-line client for [Fractal](https://fractal.omni.dev), the GitOps platform for deploying applications to Kubernetes. The installed binary is named `fractal`.

It works two ways, called transports:

- **`api`** (hosted Fractal): every command goes through the Fractal API with a bearer token. No kubeconfig needed.
- **`kube`** (bring your own cluster, BYOC, and platform admins): commands talk straight to your cluster's Kubernetes apiserver through your kubeconfig.

See [Transports](#transports) for how the CLI picks one.

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

## Quick Start (hosted)

1. Log in. Your browser opens to the Omni sign-in page; approve, then return to the terminal:

```sh
fractal login
```

2. Go:

```sh
# list your services across every project you can access
fractal services

# follow a service's logs
fractal logs my-service --follow

# rebuild and redeploy a service
fractal deploy --service my-service
```

For CI and other non-interactive use, set `FRACTAL_API_TOKEN` to an API token instead (see [CI and automation](#ci-and-automation)).

A service is addressed by name. When two projects have a service with the same name, pick one with `--project <name-or-slug>` (or `FRACTAL_PROJECT`).

### What works on hosted workspaces

| Over the API | Not available yet on hosted workspaces |
| --- | --- |
| `services`, `projects list`/`get`, `logs` (incl. `--follow`), `deploy`, `env list`/`set`/`set-secret`/`unset`, `scale`, `tokens list`/`create`/`revoke`, `members list`/`add`/`remove`, `login`/`whoami`/`logout` | `exec` (use the Shell tab in the dashboard), `restart` (use `fractal deploy`), `rollback`, `destroy`, `restore`, `canary` (use the service's page in the dashboard), `db-connect` (copy the connection string from the dashboard), `port-forward`, `secrets-sync`, `tokens delete` (use `tokens revoke`) |

The unavailable commands exit immediately with a short message saying what to use instead.

Hosted-only options:

- `fractal env set my-service API_KEY=... --secret` stores the value as a managed secret: it is sent write-only and never shown again (`env list` shows a masked reference).
- `fractal logs` takes `--timestamps` to keep each line's timestamp and `--previous` to read the previous (crashed) container. Logs cover up to 10 pods, newest first. A service that is sleeping or scaled to zero has no pods; `logs` says so instead of failing, and `--follow` waits for it to start.
- `fractal tokens` commands work on your workspace. After `fractal login` it is read from your account when you belong to exactly one workspace, otherwise inferred from your projects. Pass `--workspace <id>` (or set `FRACTAL_WORKSPACE`) if you belong to more than one.

## Quick Start (BYOC / kube)

```sh
# onboard your cluster; this also sets the transport to kube on this machine
fractal cluster connect --name us-east-prod

# list what is deployed in the current kubeconfig namespace
fractal services

# tail a service's logs
fractal logs my-service --follow
```

Run `fractal --help` (or `fractal <command> --help`) for the full command list, which includes `scale`, `restart`, `rollback`, `destroy`/`restore`, `env`, `logs`, `exec`, `port-forward`, `db-connect`, `secrets-sync`, `members`, `projects`, `services`, `deploy`, `canary`, `tokens`, `cluster`, and shell `completions`.

## Transports

The CLI picks a transport per command, highest precedence first:

1. The `--transport api|kube` flag.
2. The `FRACTAL_TRANSPORT` env var (`api` or `kube`).
3. `transport` in `~/.config/fractal/config.toml`. `fractal cluster connect` writes `transport = "kube"`, so a BYOC machine keeps talking to its own cluster even after an API login.
4. Otherwise `api` when a credential is present (`FRACTAL_API_TOKEN`, `FRACTAL_AUTH_TOKEN`, or a saved login) and `kube` when none is.

`deploy` always goes through the API, and the `cluster` commands always use kubeconfig, whatever the transport. Run `fractal whoami` to see which transport is active.

## Machine-readable output (`--json`)

Pass the global `--json` flag to get structured JSON instead of the human-formatted tables, for scripting and agent/CI use. It is honored by the read commands:

```sh
fractal --json services                 # JSON array of services
fractal --json projects list            # JSON array of projects
fractal --json projects get my-project  # JSON object for one project
fractal --json whoami                   # { authenticated, source, kind, email, workspace, ... }
fractal --json logs my-service          # stream metadata line, then NDJSON log lines
```

The `services` JSON rows surface a few fields the human table omits, so CI can discover a service's shape without cluster access to the raw `FractalService`: `serviceType` (`web`, `worker`, `cronJob`, `staticSite`, `database`), the monorepo build subdir `rootDir`, the webhook `watchPaths`, and the cron `schedule`. Commands without a `--json` implementation keep their human output.

> The CLI reads and operates existing services (including monorepo and cron ones); it does not create them. Declare a new service's `serviceType: cronJob` + `schedule`, or its monorepo `rootDir`/`watchPaths`, in a repo-committed `fractal.yaml` or through the dashboard, then deploy it. The read commands above then expose those fields for scripting.

## Authentication

### Log in

```sh
fractal login
```

`fractal login` (also `fractal auth login`) runs the standard browser login flow: it opens your browser to the Omni sign-in page, you approve, and the CLI receives the result on a temporary `127.0.0.1` listener. It saves an access token and a refresh token, then refreshes the access token silently before it expires, so you stay logged in without signing in again. If the browser doesn't finish within 5 minutes, the login times out; run `fractal login` again.

On a machine with no browser (an SSH session, or Linux with no display), or with `--no-browser`, the CLI prints the sign-in URL instead. Open it in any browser and approve. The browser then lands on a `127.0.0.1` address that may fail to load; copy the full URL from its address bar and paste it into the terminal.

When a session can no longer be refreshed (for example after a long time unused, or after you sign out everywhere), commands report `Session expired, run fractal login`.

### Check and clear the login

- `fractal whoami` (also `fractal auth whoami`) prints who you are logged in as, your workspace, the credential's source, and the transport. On the `api` transport it also checks the credential against the API and reports how many projects it can see. With `--json`, it emits the same fields (`email`, `name`, `userId`, `workspace`, `workspaces`, `kind`, `source`, ...).
- `fractal logout` (also `fractal auth logout`) revokes the browser session with the identity provider and clears the saved credentials.

### Credential precedence

The CLI resolves its credential in this order:

1. `FRACTAL_API_TOKEN`: a Fractal API token (`ftk_...`). The recommended option for CI.
2. `FRACTAL_AUTH_TOKEN`: the older name for the same override, still honored.
3. The saved login in `$XDG_CONFIG_HOME/fractal/config.toml` (defaulting to `~/.config/fractal/config.toml`, mode 0600 on Unix).

To save an API token instead of logging in through the browser, run `fractal login --token ftk_...` (or `--token -` to read it from stdin, keeping it out of shell history). The CLI checks the token against the API before saving it.

### CI and automation

Set `FRACTAL_API_TOKEN` to an API token (create one in the dashboard or with `fractal tokens create`) instead of logging in interactively. The env token takes precedence over any saved login. When it is set, `fractal login` detects it and returns immediately without opening a browser, so it is safe to call unattended; `fractal whoami` reports `source: "env"`. A typical CI step:

```sh
export FRACTAL_API_TOKEN="$FRACTAL_CI_TOKEN"
fractal --json whoami   # sanity-check the token is accepted
fractal deploy --project myproject --service my-api
```

`deploy` reads the service's namespace from the API. Pass `--namespace` too to skip that lookup, and `--env <name>` to rebuild only one environment.

## Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `FRACTAL_API_TOKEN` | Fractal API token. Takes precedence over any saved login. | (unset) |
| `FRACTAL_AUTH_TOKEN` | Older name for the token override, used when `FRACTAL_API_TOKEN` is unset. | (unset) |
| `FRACTAL_API_URL` | Base URL of the Fractal API. Overrides `api_url` in the config file. | `https://api.fractal.omni.dev` |
| `FRACTAL_TRANSPORT` | `api` or `kube`. Overrides the config file; the `--transport` flag overrides this. | (auto) |
| `FRACTAL_PROJECT` | Default project for service commands on the `api` transport. The `--project` flag overrides it. | (unset) |
| `FRACTAL_WORKSPACE` | Workspace id for `tokens` commands on the `api` transport, also shown as the active workspace by `whoami`. The `--workspace` flag overrides it. | (inferred) |
| `FRACTAL_AUTH_URL` | Base URL of the identity provider used by `fractal login`. Endpoints are read from its discovery document. | `https://identity.omni.dev` |
| `FRACTAL_OAUTH_CLIENT_ID` | OAuth client id for `fractal login`, for development against a non-production identity provider. | (built in) |
| `KUBECONFIG` | Kubeconfig path used by the `kube` transport and `cluster` commands. The first entry wins when colon-separated. | `~/.kube/config` |

The config file can also hold `transport = "api" | "kube"` and `api_url = "..."` at the top level.

## Self-hosting: connect a cluster

`fractal cluster connect` onboards a Kubernetes cluster to Fractal. It:

1. Resolves your kubeconfig (from `--kubeconfig`, then `KUBECONFIG`, then `~/.kube/config`), validates it, and sanity-pings the apiserver to confirm connectivity and report reachable nodes.
2. When authenticated (`fractal login`, or `FRACTAL_API_TOKEN`), mints a single-use install token from the control-plane API and prints a signed install URL. Otherwise it falls back to the static open-source install bundle URL.
3. Prints the `kubectl apply` command to install the fractal-operator into the `fractal-system` namespace.
4. Saves `transport = "kube"` in the CLI config, so later commands on this machine use your kubeconfig even if you also log in.

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
