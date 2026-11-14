mod api;
mod auth;
mod canary;
mod cluster;
mod config;
mod db_connect;
mod deploy;
mod destroy;
mod env;
mod exec;
mod logs;
mod members;
mod oauth;
mod port_forward;
mod projects;
mod restart;
mod restore;
mod rollback;
mod scale;
mod secrets_sync;
mod services;
mod tokens;
mod transport;

use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

use transport::{Transport, unavailable};

/// Fractal - deploy applications to Kubernetes
#[derive(Parser)]
#[command(name = "fractal", version = env!("FRACTAL_VERSION"), about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Emit machine-readable JSON instead of human-formatted output.
    /// Honored by the read commands: `services`, `projects list`/`get`,
    /// `whoami`, and `logs` (stream metadata plus NDJSON lines).
    /// Other commands ignore it and keep their human output.
    #[arg(long, global = true)]
    json: bool,
    /// How to reach Fractal: `api` (hosted, bearer token) or `kube`
    /// (BYOC/admin, kubeconfig). Overrides `FRACTAL_TRANSPORT` and the
    /// transport saved in the config file (`~/.config/fractal/config.toml`
    /// on Linux, `~/Library/Application Support/fractal/config.toml` on
    /// macOS); `fractal cluster disconnect` clears the saved one.
    /// Defaults to `api` when a token is configured.
    #[arg(long, global = true, value_enum)]
    transport: Option<Transport>,
    /// Project (name or slug) that owns the service, for the api
    /// transport. Falls back to `FRACTAL_PROJECT`; when neither is set,
    /// the service is looked up across your projects.
    #[arg(long, global = true)]
    project: Option<String>,
}

/// Global options every dispatcher needs
struct Ctx<'a> {
    json: bool,
    transport: Option<Transport>,
    project: Option<String>,
    rt: &'a tokio::runtime::Runtime,
}

impl Ctx<'_> {
    /// Resolve the transport lazily, so commands that don't need it
    /// (completions, auth login) never read config they don't use
    fn transport(&self) -> anyhow::Result<Transport> {
        transport::resolve(self.transport)
    }

    fn is_api(&self) -> anyhow::Result<bool> {
        Ok(self.transport()? == Transport::Api)
    }

    fn project(&self) -> Option<String> {
        api::resolve::project_hint(self.project.as_deref())
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Scale a service to a specific replica count
    Scale {
        /// Service name
        service: String,
        /// Number of replicas
        #[arg(short, long)]
        replicas: i32,
    },
    /// Trigger a rolling restart of a service
    Restart {
        /// Service name
        service: String,
    },
    /// Roll back a service to its previous version
    Rollback {
        /// Service name
        service: String,
    },
    /// Delete a service (enters grace period before permanent deletion)
    Destroy {
        /// Service name
        service: String,
    },
    /// Restore a service from pending deletion
    Restore {
        /// Service name
        service: String,
    },
    /// Manage environment variables for a service
    Env {
        #[command(subcommand)]
        action: EnvCommands,
    },
    /// Tail logs from a service's pods
    Logs {
        /// Service name
        service: String,
        /// Follow the log stream (tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Show logs from this long ago: a whole number plus a unit,
        /// s, m, h, or d (e.g. 30s, 5m, 1h, 2d)
        #[arg(long)]
        since: Option<String>,
        /// Show only the last N lines per pod
        #[arg(short, long)]
        tail: Option<i64>,
        /// Filter lines containing this substring
        #[arg(short, long)]
        grep: Option<String>,
        /// Prefix each line with its timestamp (the kube transport always does)
        #[arg(long)]
        timestamps: bool,
        /// Show logs from the previous (crashed) container instance
        #[arg(long)]
        previous: bool,
    },
    /// Exec a command in a service's running pod
    Exec {
        /// Service name
        service: String,
        /// Allocate a TTY (interactive shells need this)
        #[arg(short = 't', long)]
        tty: bool,
        /// Command to run (defaults to /bin/sh)
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Forward a local port to a service's pod
    PortForward {
        /// Service name
        service: String,
        /// "local:remote" or just "port" for same on both
        ports: String,
    },
    /// Connect to a Database service via psql
    DbConnect {
        /// Service name (must be a Database service)
        service: String,
        /// Print connection info instead of execing psql
        #[arg(long)]
        print_only: bool,
    },
    /// Sync a service's env vars into a local .env file
    SecretsSync {
        /// Service name
        service: String,
        /// Output path (defaults to ./.env)
        #[arg(short, long, default_value = ".env")]
        output: std::path::PathBuf,
        /// Overwrite existing values in the output file
        #[arg(long)]
        overwrite: bool,
    },
    /// Manage per-project members (admin / developer / viewer)
    Members {
        #[command(subcommand)]
        action: MembersCommands,
    },
    /// Connect, inspect, or disconnect a Kubernetes cluster for BYOC
    Cluster {
        #[command(subcommand)]
        action: ClusterCommands,
    },
    /// Manage service-account API tokens (CI deploys, automation)
    Tokens {
        #[command(subcommand)]
        action: TokensCommands,
    },
    /// Log in through the browser. Saves a session that refreshes
    /// itself; `--token` saves a Fractal API token instead
    Login {
        #[command(flatten)]
        args: LoginArgs,
    },
    /// Log out: revoke the browser session and clear saved credentials
    Logout,
    /// Print the currently logged-in identity
    Whoami,
    /// Authenticate with the Fractal control plane
    Auth {
        #[command(subcommand)]
        action: AuthCommands,
    },
    /// Drive a canary rollout on a service: weight a candidate image
    /// alongside the primary, then promote or abort.
    Canary {
        #[command(subcommand)]
        action: CanaryCommands,
    },
    /// List all `FractalService`s in a namespace
    Services {
        /// Namespace (defaults to current kubeconfig namespace)
        #[arg(long, short = 'n')]
        namespace: Option<String>,
    },
    /// Inspect `FractalProject` resources
    Projects {
        #[command(subcommand)]
        action: ProjectsCommands,
    },
    /// Generate shell completion script for the given shell. Pipe to
    /// the shell's completion directory; e.g.:
    ///   fractal completions zsh > ~/.zfunc/_fractal
    ///   fractal completions bash | sudo tee `/etc/bash_completion.d/fractal`
    ///   fractal completions fish > ~/.config/fish/completions/fractal.fish
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Trigger a rebuild on a service via the Fractal API (CI use).
    ///
    /// Unlike `restart`, this goes through the authed HTTP endpoint
    /// (bearer token + project developer role) instead of patching the
    /// Deployment directly, so it works from CI runners and hosted
    /// workspaces with no cluster access. Pass `--project` to skip the
    /// lookup; `--namespace` is derived when omitted.
    Deploy {
        /// Namespace the service lives in (read from the API when
        /// omitted; pass `--env` to target an environment)
        #[arg(long)]
        namespace: Option<String>,
        /// Service name to rebuild
        #[arg(long)]
        service: String,
        /// Optional environment to target (e.g. "production")
        #[arg(long)]
        env: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProjectsCommands {
    /// List all `FractalProject` resources
    List,
    /// Show a single project's spec + status (namespaces, members, phase)
    Get {
        /// Project name (`FractalProject` metadata.name)
        name: String,
    },
}

#[derive(Subcommand)]
enum CanaryCommands {
    /// Start a canary on `service` with the given image and weight (0..=100)
    Start {
        /// Service name
        service: String,
        /// Canary image (e.g. `reg/svc:sha-abc1234`)
        #[arg(long)]
        image: String,
        /// Percentage of traffic to send to the canary
        #[arg(long, default_value_t = 10)]
        weight: u8,
    },
    /// Promote the active canary: ship its image, retire the primary
    Promote {
        /// Service name
        service: String,
    },
    /// Abort the active canary: route 100% back to primary, teardown canary
    Abort {
        /// Service name
        service: String,
    },
    /// Show whether a canary is active and its current weight
    Status {
        /// Service name
        service: String,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Log in through the browser (same as `fractal login`), or save a
    /// Fractal API token with `--token`
    Login {
        #[command(flatten)]
        args: LoginArgs,
    },
    /// Log out: revoke the browser session and clear saved credentials
    Logout,
    /// Print the currently logged-in identity
    Whoami,
}

#[derive(clap::Args)]
struct LoginArgs {
    /// Save a Fractal API token (`ftk_...`) instead of logging in
    /// through the browser. Pass `-` to read it from stdin so it stays
    /// out of shell history
    #[arg(long, conflicts_with = "no_browser")]
    token: Option<String>,
    /// Don't open a browser: print the sign-in URL and paste back the
    /// URL the browser lands on. Used automatically over SSH or with no
    /// display
    #[arg(long)]
    no_browser: bool,
}

#[derive(Subcommand)]
enum TokensCommands {
    /// List tokens in the current workspace
    List {
        /// Workspace id (api transport). Falls back to
        /// `FRACTAL_WORKSPACE`, then the workspace that owns your projects
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Mint a new token. The plaintext is printed once; only its hash is stored.
    Create {
        /// Human-readable label (e.g. "github-actions-ci")
        #[arg(long)]
        name: String,
        /// HIDRA user id / email the token acts as. Use "service-account"
        /// for tokens not bound to a real user.
        #[arg(long, default_value = "service-account")]
        subject: String,
        /// Project slugs the token can act on. Pass `--scope *` for
        /// workspace-wide access. Can be specified multiple times.
        #[arg(long = "scope", value_name = "PROJECT")]
        scopes: Vec<String>,
        /// Token lifetime in days. Omit for never-expires.
        #[arg(long)]
        expires_in_days: Option<i64>,
        /// Workspace id (api transport). Falls back to
        /// `FRACTAL_WORKSPACE`, then the workspace that owns your projects
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Revoke a token (kept for audit)
    Revoke {
        /// Token label
        name: String,
        /// Workspace id (api transport). Falls back to
        /// `FRACTAL_WORKSPACE`, then the workspace that owns your projects
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Permanently delete a token CR (audit trail goes with it; prefer revoke)
    Delete {
        /// Token label
        name: String,
    },
}

#[derive(Subcommand)]
enum ClusterCommands {
    /// Validate connectivity and print install instructions for a cluster
    Connect {
        /// Kubeconfig path (defaults to $KUBECONFIG or ~/.kube/config)
        #[arg(long)]
        kubeconfig: Option<std::path::PathBuf>,
        /// Kubeconfig context to use (defaults to the current context)
        #[arg(long)]
        context: Option<String>,
        /// Human-readable label for the cluster (e.g. "us-east-prod"),
        /// attached to the per-cluster install token minted through the
        /// API (needs `fractal login` or `FRACTAL_API_TOKEN`)
        #[arg(long = "name")]
        name: Option<String>,
        /// BYOC: registry (host + namespace, e.g. registry.example.com/myorg)
        /// the operator pushes builds to and pulls runtime images from. Omit
        /// to deploy onto Omni's hosted cluster. You supply the registry pull
        /// credential yourself (see the printed instructions); it never
        /// transits Omni.
        #[arg(long)]
        registry: Option<String>,
        /// BYOC: treat the registry as plaintext HTTP (default is HTTPS).
        #[arg(long = "registry-insecure")]
        registry_insecure: bool,
    },
    /// Report whether the fractal-operator is installed and Ready
    Status,
    /// Point this machine back at the hosted API (clears the saved kube transport)
    ///
    /// Clears the `transport = "kube"` that `cluster connect` saved in
    /// the config file (`~/.config/fractal/config.toml` on Linux,
    /// `~/Library/Application Support/fractal/config.toml` on macOS),
    /// keeping your login, then prints how to remove the operator from
    /// the cluster. Workloads already on the cluster keep running.
    Disconnect,
    /// List install tokens you have issued (active, used, expired)
    ListTokens,
    /// Revoke an install token by id (from `cluster list-tokens`)
    RevokeToken {
        /// Token id to revoke
        id: String,
    },
}

#[derive(Subcommand)]
enum MembersCommands {
    /// List members of a project
    List {
        /// Project name
        project: String,
    },
    /// Add or update a member's role on a project
    Add {
        /// Project name
        project: String,
        /// User ID or email
        user: String,
        /// Role (admin / developer / viewer)
        #[arg(short, long, default_value = "developer")]
        role: String,
    },
    /// Remove a member from a project (owner cannot be removed)
    Remove {
        /// Project name
        project: String,
        /// User ID or email
        user: String,
    },
}

fn parse_ports(s: &str) -> anyhow::Result<(u16, u16)> {
    let parsed: Vec<u16> = s
        .split(':')
        .map(|p| {
            p.parse::<u16>()
                .map_err(|e| anyhow::anyhow!("invalid port '{p}': {e}"))
        })
        .collect::<anyhow::Result<_>>()?;
    match parsed.as_slice() {
        [single] => Ok((*single, *single)),
        [local, remote] => Ok((*local, *remote)),
        _ => anyhow::bail!("ports must be 'PORT' or 'LOCAL:REMOTE'"),
    }
}

#[derive(Subcommand)]
enum EnvCommands {
    /// List env vars for a service
    List {
        /// Service name
        service: String,
    },
    /// Set a literal env var (KEY=VALUE)
    Set {
        /// Service name
        service: String,
        /// Key=value pair
        pair: String,
        /// Store the value as a managed secret: it is sent write-only and
        /// never shown again (api transport)
        #[arg(long)]
        secret: bool,
    },
    /// Set an env var from a Kubernetes secret
    SetSecret {
        /// Service name
        service: String,
        /// Env var name
        name: String,
        /// Secret name
        #[arg(long)]
        secret: String,
        /// Key within the secret
        #[arg(long)]
        key: String,
    },
    /// Remove an env var
    Unset {
        /// Service name
        service: String,
        /// Env var name
        name: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let ctx = Ctx {
        json: cli.json,
        transport: cli.transport,
        project: cli.project,
        rt: &rt,
    };
    match dispatch(cli.command, &ctx) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Hint for commands that need direct cluster access
const NEEDS_CLUSTER: &str =
    "it needs direct cluster access (connect a BYOC cluster and use `--transport kube`)";

/// Hint for service actions the dashboard covers
const USE_SERVICE_PAGE: &str = "use the service's page in the dashboard";

/// Commands with no hosted equivalent yet, and what to do instead. The
/// api transport fails fast on these rather than reaching for a
/// kubeconfig the user doesn't have.
const fn hosted_gap(command: &Commands) -> Option<(&'static str, &'static str)> {
    Some(match command {
        Commands::Restart { .. } => (
            "restart",
            "run `fractal deploy --service <name>` to rebuild and roll it out",
        ),
        Commands::Rollback { .. } => ("rollback", USE_SERVICE_PAGE),
        Commands::Destroy { .. } => ("destroy", USE_SERVICE_PAGE),
        Commands::Restore { .. } => ("restore", "use the dashboard"),
        Commands::Exec { .. } => ("exec", "use the Shell tab in the dashboard"),
        Commands::PortForward { .. } => ("port-forward", NEEDS_CLUSTER),
        Commands::DbConnect { .. } => (
            "db-connect",
            "copy the connection string from the database's page in the dashboard",
        ),
        Commands::SecretsSync { .. } => (
            "secrets-sync",
            "run `fractal env list <service>` to see its variables",
        ),
        Commands::Canary { .. } => ("canary", USE_SERVICE_PAGE),
        Commands::Tokens {
            action: TokensCommands::Delete { .. },
        } => (
            "tokens delete",
            "run `fractal tokens revoke`, which keeps an audit trail",
        ),
        _ => return None,
    })
}

fn dispatch(command: Commands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    if let Some((name, hint)) = hosted_gap(&command) {
        if ctx.is_api()? {
            return Err(unavailable(name, hint));
        }
    }
    let rt = ctx.rt;
    let json = ctx.json;
    let project = ctx.project();
    let project = project.as_deref();
    match command {
        Commands::Scale { service, replicas } => {
            if ctx.is_api()? {
                rt.block_on(scale::scale_service_api(project, &service, replicas))
            } else {
                rt.block_on(scale::scale_service(&service, replicas))
            }
        }
        Commands::Restart { service } => rt.block_on(restart::restart_service(&service)),
        Commands::Rollback { service } => rt.block_on(rollback::rollback_service(&service)),
        Commands::Destroy { service } => rt.block_on(destroy::destroy_service(&service)),
        Commands::Restore { service } => rt.block_on(restore::restore_service(&service)),
        Commands::Env { action } => dispatch_env(action, ctx),
        Commands::Logs {
            service,
            follow,
            since,
            tail,
            grep,
            timestamps,
            previous,
        } => {
            let opts = logs::LogOptions {
                follow,
                since: since.as_deref(),
                tail,
                grep: grep.as_deref(),
                timestamps,
                previous,
                json,
            };
            dispatch_logs(&service, &opts, ctx)
        }
        Commands::Exec { service, tty, cmd } => {
            rt.block_on(exec::exec_in_service(&service, &cmd, tty))
        }
        Commands::PortForward { service, ports } => {
            let (local, remote) = parse_ports(&ports)?;
            rt.block_on(port_forward::port_forward(&service, local, remote))
        }
        Commands::DbConnect {
            service,
            print_only,
        } => rt.block_on(db_connect::db_connect(&service, print_only)),
        Commands::SecretsSync {
            service,
            output,
            overwrite,
        } => rt.block_on(secrets_sync::sync_secrets(&service, &output, overwrite)),
        Commands::Members { action } => dispatch_members(action, ctx),
        Commands::Cluster { action } => dispatch_cluster(action, rt),
        Commands::Tokens { action } => dispatch_tokens(action, ctx),
        Commands::Login { args } => dispatch_auth(AuthCommands::Login { args }, ctx),
        Commands::Logout => dispatch_auth(AuthCommands::Logout, ctx),
        Commands::Whoami => dispatch_auth(AuthCommands::Whoami, ctx),
        Commands::Auth { action } => dispatch_auth(action, ctx),
        Commands::Deploy {
            namespace,
            service,
            env,
        } => rt.block_on(deploy::deploy(
            namespace.as_deref(),
            project,
            &service,
            env.as_deref(),
        )),
        Commands::Services { namespace } => {
            if ctx.is_api()? {
                rt.block_on(services::list_services_api(
                    project,
                    namespace.as_deref(),
                    json,
                ))
            } else {
                rt.block_on(services::list_services(namespace, json))
            }
        }
        Commands::Projects { action } => dispatch_projects(action, ctx),
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
            Ok(())
        }
        Commands::Canary { action } => dispatch_canary(action, rt),
    }
}

fn dispatch_logs(service: &str, opts: &logs::LogOptions<'_>, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    if ctx.is_api()? {
        let project = ctx.project();
        return ctx
            .rt
            .block_on(logs::tail_logs_api(project.as_deref(), service, opts));
    }
    ctx.rt.block_on(logs::tail_logs(
        service,
        opts.follow,
        opts.since,
        opts.tail,
        opts.grep,
        opts.previous,
        opts.json,
    ))
}

fn dispatch_projects(action: ProjectsCommands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    let rt = ctx.rt;
    let json = ctx.json;
    match (action, ctx.is_api()?) {
        (ProjectsCommands::List, true) => rt.block_on(projects::list_projects_api(json)),
        (ProjectsCommands::List, false) => rt.block_on(projects::list_projects(json)),
        (ProjectsCommands::Get { name }, true) => {
            rt.block_on(projects::get_project_api(&name, json))
        }
        (ProjectsCommands::Get { name }, false) => rt.block_on(projects::get_project(&name, json)),
    }
}

fn dispatch_env(action: EnvCommands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    let rt = ctx.rt;
    let project = ctx.project();
    let project = project.as_deref();
    if ctx.is_api()? {
        return match action {
            EnvCommands::List { service } => rt.block_on(env::list_env_api(project, &service)),
            EnvCommands::Set {
                service,
                pair,
                secret,
            } => rt.block_on(env::set_env_api(project, &service, &pair, secret)),
            EnvCommands::SetSecret {
                service,
                name,
                secret,
                key,
            } => rt.block_on(env::set_secret_env_api(
                project, &service, &name, &secret, &key,
            )),
            EnvCommands::Unset { service, name } => {
                rt.block_on(env::unset_env_api(project, &service, &name))
            }
        };
    }
    match action {
        EnvCommands::List { service } => rt.block_on(env::list_env(&service)),
        EnvCommands::Set {
            service,
            pair,
            secret,
        } => {
            if secret {
                anyhow::bail!(
                    "--secret needs the api transport; on kube, create a Secret and use `fractal env set-secret`"
                );
            }
            rt.block_on(env::set_env(&service, &pair))
        }
        EnvCommands::SetSecret {
            service,
            name,
            secret,
            key,
        } => rt.block_on(env::set_secret_env(&service, &name, &secret, &key)),
        EnvCommands::Unset { service, name } => rt.block_on(env::unset_env(&service, &name)),
    }
}

fn dispatch_members(action: MembersCommands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    let rt = ctx.rt;
    if ctx.is_api()? {
        return match action {
            MembersCommands::List { project } => rt.block_on(members::list_members_api(&project)),
            MembersCommands::Add {
                project,
                user,
                role,
            } => rt.block_on(members::add_member_api(&project, &user, &role)),
            MembersCommands::Remove { project, user } => {
                rt.block_on(members::remove_member_api(&project, &user))
            }
        };
    }
    match action {
        MembersCommands::List { project } => rt.block_on(members::list_members(&project)),
        MembersCommands::Add {
            project,
            user,
            role,
        } => rt.block_on(members::add_member(&project, &user, &role)),
        MembersCommands::Remove { project, user } => {
            rt.block_on(members::remove_member(&project, &user))
        }
    }
}

fn dispatch_cluster(action: ClusterCommands, rt: &tokio::runtime::Runtime) -> anyhow::Result<()> {
    match action {
        ClusterCommands::Connect {
            kubeconfig,
            context,
            name,
            registry,
            registry_insecure,
        } => rt.block_on(cluster::connect(
            kubeconfig,
            context,
            name,
            registry,
            registry_insecure,
        )),
        ClusterCommands::Status => rt.block_on(cluster::status()),
        ClusterCommands::Disconnect => cluster::disconnect(),
        ClusterCommands::ListTokens => rt.block_on(cluster::list_tokens()),
        ClusterCommands::RevokeToken { id } => rt.block_on(cluster::revoke_token(&id)),
    }
}

/// `fractal login | logout | whoami`, also reachable under `fractal auth`
fn dispatch_auth(action: AuthCommands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    match action {
        AuthCommands::Login { args } => {
            ctx.rt
                .block_on(auth::login(ctx.json, args.token, args.no_browser))
        }
        AuthCommands::Logout => ctx.rt.block_on(auth::logout()),
        AuthCommands::Whoami => ctx.rt.block_on(auth::whoami(ctx.json, ctx.transport()?)),
    }
}

fn dispatch_tokens(action: TokensCommands, ctx: &Ctx<'_>) -> anyhow::Result<()> {
    let rt = ctx.rt;
    if ctx.is_api()? {
        return match action {
            TokensCommands::List { workspace } => {
                rt.block_on(tokens::list_tokens_api(workspace.as_deref()))
            }
            TokensCommands::Create {
                name,
                subject,
                scopes,
                expires_in_days,
                workspace,
            } => rt.block_on(tokens::create_token_api(
                &name,
                &subject,
                scopes,
                expires_in_days,
                workspace.as_deref(),
            )),
            TokensCommands::Revoke { name, workspace } => {
                rt.block_on(tokens::revoke_token_api(&name, workspace.as_deref()))
            }
            TokensCommands::Delete { .. } => unreachable!("handled by hosted_gap"),
        };
    }
    match action {
        TokensCommands::List { .. } => rt.block_on(tokens::list_tokens()),
        TokensCommands::Create {
            name,
            subject,
            scopes,
            expires_in_days,
            ..
        } => rt.block_on(tokens::create_token(
            &name,
            &subject,
            scopes,
            expires_in_days,
        )),
        TokensCommands::Revoke { name, .. } => rt.block_on(tokens::revoke_token(&name)),
        TokensCommands::Delete { name } => rt.block_on(tokens::delete_token(&name)),
    }
}

fn dispatch_canary(action: CanaryCommands, rt: &tokio::runtime::Runtime) -> anyhow::Result<()> {
    match action {
        CanaryCommands::Start {
            service,
            image,
            weight,
        } => rt.block_on(canary::start_canary(&service, &image, weight)),
        CanaryCommands::Promote { service } => rt.block_on(canary::promote_canary(&service)),
        CanaryCommands::Abort { service } => rt.block_on(canary::abort_canary(&service)),
        CanaryCommands::Status { service } => rt.block_on(canary::canary_status(&service)),
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_parse_after_subcommand() {
        let cli = Cli::try_parse_from([
            "fractal",
            "deploy",
            "--project",
            "demo",
            "--service",
            "web",
            "--transport",
            "api",
        ])
        .unwrap();
        assert_eq!(cli.project.as_deref(), Some("demo"));
        assert_eq!(cli.transport, Some(super::Transport::Api));
    }

    #[test]
    fn version_carries_the_build_stamp() {
        let cmd = Cli::command();
        let version = cmd.get_version().unwrap();
        assert!(
            version.starts_with(concat!(env!("CARGO_PKG_VERSION"), " (")),
            "{version}"
        );
        assert!(version.ends_with(')'), "{version}");
    }

    #[test]
    fn cluster_disconnect_parses_and_documents_the_transport_reset() {
        let cli = Cli::try_parse_from(["fractal", "cluster", "disconnect"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Cluster {
                action: super::ClusterCommands::Disconnect
            }
        ));
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("cluster")
            .and_then(|c| c.find_subcommand_mut("disconnect"))
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("transport"), "{help}");
    }

    #[test]
    fn logs_accepts_hosted_flags() {
        let cli =
            Cli::try_parse_from(["fractal", "logs", "web", "-f", "--timestamps", "--previous"])
                .unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Logs {
                follow: true,
                timestamps: true,
                previous: true,
                ..
            }
        ));
    }

    #[test]
    fn auth_login_takes_token() {
        let cli = Cli::try_parse_from(["fractal", "auth", "login", "--token", "-"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Auth {
                action: super::AuthCommands::Login { ref args }
            } if args.token.as_deref() == Some("-") && !args.no_browser
        ));
    }

    #[test]
    fn top_level_login_logout_whoami_parse() {
        let cli = Cli::try_parse_from(["fractal", "login", "--no-browser"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Login { ref args } if args.no_browser && args.token.is_none()
        ));
        let cli = Cli::try_parse_from(["fractal", "login"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Login { ref args } if !args.no_browser && args.token.is_none()
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "logout"]).unwrap().command,
            super::Commands::Logout
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "--json", "whoami"])
                .unwrap()
                .command,
            super::Commands::Whoami
        ));
    }

    #[test]
    fn token_and_no_browser_conflict() {
        assert!(Cli::try_parse_from(["fractal", "login", "--token", "x", "--no-browser"]).is_err());
    }

    #[test]
    fn hosted_gaps_are_actionable_and_never_mention_kubeconfig() {
        let cases = [
            vec!["fractal", "exec", "web"],
            vec!["fractal", "port-forward", "web", "8080"],
            vec!["fractal", "db-connect", "db"],
            vec!["fractal", "secrets-sync", "web"],
            vec!["fractal", "restart", "web"],
            vec!["fractal", "rollback", "web"],
            vec!["fractal", "destroy", "web"],
            vec!["fractal", "restore", "web"],
            vec!["fractal", "canary", "status", "web"],
            vec!["fractal", "tokens", "delete", "ci"],
        ];
        for argv in cases {
            let cli = Cli::try_parse_from(&argv).unwrap();
            let (name, hint) = super::hosted_gap(&cli.command)
                .unwrap_or_else(|| panic!("{argv:?} should be a hosted gap"));
            let msg = super::unavailable(name, hint).to_string();
            assert!(msg.starts_with(&format!("`fractal {name}` isn't available")));
            assert!(!msg.to_lowercase().contains("kubeconfig"), "{msg}");
        }
        let exec = Cli::try_parse_from(["fractal", "exec", "web"]).unwrap();
        assert_eq!(
            super::unavailable("exec", super::hosted_gap(&exec.command).unwrap().1).to_string(),
            "`fractal exec` isn't available for hosted workspaces yet; use the Shell tab in the dashboard"
        );
    }

    #[test]
    fn hosted_commands_are_not_gaps() {
        for argv in [
            vec!["fractal", "services"],
            vec!["fractal", "logs", "web"],
            vec!["fractal", "env", "list", "web"],
            vec!["fractal", "scale", "web", "-r", "2"],
            vec!["fractal", "tokens", "list"],
            vec!["fractal", "members", "list", "demo"],
            vec!["fractal", "deploy", "--service", "web"],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            assert!(super::hosted_gap(&cli.command).is_none(), "{argv:?}");
        }
    }

    #[test]
    fn rejects_unknown_transport() {
        assert!(Cli::try_parse_from(["fractal", "--transport", "ssh", "services"]).is_err());
    }
}
