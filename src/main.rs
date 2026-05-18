mod destroy;
mod env;
mod exec;
mod logs;
mod restart;
mod restore;
mod rollback;
mod scale;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
