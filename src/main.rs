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
mod port_forward;
mod restart;
mod restore;
mod rollback;
mod scale;
mod secrets_sync;
mod services;
mod tokens;

use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

/// Fractal - deploy applications to Kubernetes
#[derive(Parser)]
#[command(name = "fractal", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
        /// Show logs since duration ago (e.g. 5m, 1h, 2d)
        #[arg(long)]
        since: Option<String>,
        /// Show only the last N lines per pod
        #[arg(short, long)]
        tail: Option<i64>,
        /// Filter lines containing this substring
        #[arg(short, long)]
        grep: Option<String>,
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
    /// List all FractalServices in a namespace
    Services {
        /// Namespace (defaults to current kubeconfig namespace)
        #[arg(long, short = 'n')]
        namespace: Option<String>,
    },
    /// Generate shell completion script for the given shell. Pipe to
    /// the shell's completion directory; e.g.:
    ///   fractal completions zsh > ~/.zfunc/_fractal
    ///   fractal completions bash | sudo tee /etc/bash_completion.d/fractal
    ///   fractal completions fish > ~/.config/fish/completions/fractal.fish
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Trigger a rebuild on a FractalService via fractal-api (CI use).
    ///
    /// Unlike `restart`, this goes through the authed HTTP endpoint
    /// (Gatekeeper JWT + project-developer role) instead of patching
    /// the K8s Deployment directly, so it works from CI runners that
    /// don't have cluster RBAC.
    Deploy {
        /// Namespace the FractalService lives in (e.g. fractal-myproject)
        #[arg(long)]
        namespace: String,
        /// Parent FractalProject name (used for RBAC)
        #[arg(long)]
        project: String,
        /// FractalService name to rebuild
        #[arg(long)]
        service: String,
        /// Optional environment to target (e.g. "production")
        #[arg(long)]
        env: Option<String>,
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
    /// Log in with email + password, persisting a bearer token at
    /// ~/.config/fractal/config.toml
    Login,
    /// Clear the persisted bearer token
    Logout,
    /// Print the currently logged-in identity
    Whoami,
}

#[derive(Subcommand)]
enum TokensCommands {
    /// List tokens in the current workspace
    List,
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
    },
    /// Revoke a token (sets spec.revokedAt; CR remains for audit)
    Revoke {
        /// Token label
        name: String,
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
        /// Human-readable label for the cluster (e.g. "us-east-prod").
        /// Used only when `FRACTAL_API_URL` + `FRACTAL_AUTH_TOKEN` are
        /// set so the CLI can mint a per-cluster install token via the
        /// API.
        #[arg(long = "name")]
        name: Option<String>,
    },
    /// Report whether the fractal-operator is installed and Ready
    Status,
    /// Revoke this cluster's dashboard registration (placeholder)
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

    let result = match cli.command {
        Commands::Scale { service, replicas } => {
            rt.block_on(scale::scale_service(&service, replicas))
        }
        Commands::Restart { service } => rt.block_on(restart::restart_service(&service)),
        Commands::Rollback { service } => rt.block_on(rollback::rollback_service(&service)),
        Commands::Destroy { service } => rt.block_on(destroy::destroy_service(&service)),
        Commands::Restore { service } => rt.block_on(restore::restore_service(&service)),
        Commands::Env { action } => match action {
            EnvCommands::List { service } => rt.block_on(env::list_env(&service)),
            EnvCommands::Set { service, pair } => rt.block_on(env::set_env(&service, &pair)),
            EnvCommands::SetSecret {
                service,
                name,
                secret,
                key,
            } => rt.block_on(env::set_secret_env(&service, &name, &secret, &key)),
            EnvCommands::Unset { service, name } => rt.block_on(env::unset_env(&service, &name)),
        },
        Commands::Logs {
            service,
            follow,
            since,
            tail,
            grep,
        } => rt.block_on(logs::tail_logs(
            &service,
            follow,
            since.as_deref(),
            tail,
            grep.as_deref(),
        )),
        Commands::Exec { service, tty, cmd } => rt.block_on(exec::exec_in_service(&service, &cmd, tty)),
        Commands::PortForward { service, ports } => match parse_ports(&ports) {
            Ok((local, remote)) => rt.block_on(port_forward::port_forward(&service, local, remote)),
            Err(e) => Err(e),
        },
        Commands::DbConnect { service, print_only } => {
            rt.block_on(db_connect::db_connect(&service, print_only))
        }
        Commands::SecretsSync {
            service,
            output,
            overwrite,
        } => rt.block_on(secrets_sync::sync_secrets(&service, &output, overwrite)),
        Commands::Members { action } => match action {
            MembersCommands::List { project } => rt.block_on(members::list_members(&project)),
            MembersCommands::Add {
                project,
                user,
                role,
            } => rt.block_on(members::add_member(&project, &user, &role)),
            MembersCommands::Remove { project, user } => {
                rt.block_on(members::remove_member(&project, &user))
            }
        },
        Commands::Cluster { action } => match action {
            ClusterCommands::Connect {
                kubeconfig,
                context,
                name,
            } => rt.block_on(cluster::connect(kubeconfig, context, name)),
            ClusterCommands::Status => rt.block_on(cluster::status()),
            ClusterCommands::Disconnect => rt.block_on(cluster::disconnect()),
            ClusterCommands::ListTokens => rt.block_on(cluster::list_tokens()),
            ClusterCommands::RevokeToken { id } => {
                rt.block_on(cluster::revoke_token(&id))
            }
        },
        Commands::Tokens { action } => match action {
            TokensCommands::List => rt.block_on(tokens::list_tokens()),
            TokensCommands::Create {
                name,
                subject,
                scopes,
                expires_in_days,
            } => rt.block_on(tokens::create_token(&name, &subject, scopes, expires_in_days)),
            TokensCommands::Revoke { name } => rt.block_on(tokens::revoke_token(&name)),
            TokensCommands::Delete { name } => rt.block_on(tokens::delete_token(&name)),
        },
        Commands::Auth { action } => match action {
            AuthCommands::Login => rt.block_on(auth::login()),
            AuthCommands::Logout => auth::logout(),
            AuthCommands::Whoami => auth::whoami(),
        },
        Commands::Deploy {
            namespace,
            project,
            service,
            env,
        } => rt.block_on(deploy::deploy(&namespace, &project, &service, env)),
        Commands::Services { namespace } => {
            rt.block_on(services::list_services(namespace))
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
            Ok(())
        }
        Commands::Canary { action } => match action {
            CanaryCommands::Start {
                service,
                image,
                weight,
            } => rt.block_on(canary::start_canary(&service, &image, weight)),
            CanaryCommands::Promote { service } => {
                rt.block_on(canary::promote_canary(&service))
            }
            CanaryCommands::Abort { service } => {
                rt.block_on(canary::abort_canary(&service))
            }
            CanaryCommands::Status { service } => {
                rt.block_on(canary::canary_status(&service))
            }
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
