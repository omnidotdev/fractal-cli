mod bootstrap;
mod config;
mod deploy;
mod env;
mod error;
mod logs;
mod restart;
mod scale;
mod status;

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
    /// Initialize a new Fractal project
    Init {
        /// Project name
        name: String,
    },
    /// Deploy services to the cluster
    Deploy {
        /// Path to fractal.toml (defaults to ./fractal.toml)
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
        /// Dry run - show what would be applied
        #[arg(long)]
        dry_run: bool,
        /// Override image for a service (SERVICE=IMAGE:TAG, repeatable)
        #[arg(long, value_name = "SERVICE=IMAGE:TAG")]
        image: Vec<String>,
    },
    /// Show status of deployed services
    Status {
        /// Filter by project name
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Stream logs from a running service
    Logs {
        /// Service name
        service: String,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Bootstrap a Fractal cluster
    Bootstrap {
        /// Bootstrap mode (dev, production, connect)
        #[arg(value_enum, default_value_t = bootstrap::BootstrapMode::Dev)]
        mode: bootstrap::BootstrapMode,
        /// Cluster name
        #[arg(short, long, default_value = "fractal")]
        name: String,
        /// Skip preflight checks
        #[arg(long)]
        skip_checks: bool,
        /// Container registry URL
        #[arg(long)]
        registry: Option<String>,
    },
    /// Open the Fractal dashboard
    Dashboard {
        /// Port for the kubectl proxy
        #[arg(short, long, default_value_t = 8080)]
        port: u16,
    },
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
    /// Manage environment variables for a service
    Env {
        #[command(subcommand)]
        action: EnvCommands,
    },
    /// Tear down a Fractal cluster
    Destroy {
        /// Cluster name
        #[arg(short, long, default_value = "fractal")]
        name: String,
        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
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

    let result = match cli.command {
        Commands::Init { name } => cmd_init(&name),
        Commands::Deploy {
            file,
            dry_run,
            image,
        } => cmd_deploy(file.as_deref(), dry_run, &image),
        Commands::Status { project } => cmd_status(project.as_deref()),
        Commands::Logs { service, follow } => cmd_logs(&service, follow),
        Commands::Bootstrap {
            mode,
            name,
            skip_checks,
            registry,
        } => cmd_bootstrap(&mode, &name, skip_checks, registry.as_deref()),
        Commands::Dashboard { port } => cmd_dashboard(port),
        Commands::Scale { service, replicas } => cmd_scale(&service, replicas),
        Commands::Restart { service } => cmd_restart(&service),
        Commands::Env { action } => cmd_env(action),
        Commands::Destroy { name, yes } => cmd_destroy(&name, yes),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_init(name: &str) -> anyhow::Result<()> {
    use colored::Colorize;

    let path = std::path::Path::new("fractal.toml");
    if path.exists() {
        anyhow::bail!("fractal.toml already exists in this directory");
    }

    let content = config::generate_template(name);
    std::fs::write(path, &content)?;

    println!("{} Created fractal.toml for project '{name}'", "\u{2713}".green());
    println!("  Edit the file to configure your services, then run:");
    println!("  {} to deploy", "fractal deploy".cyan());

    Ok(())
}

fn cmd_deploy(
    file: Option<&std::path::Path>,
    dry_run: bool,
    image_overrides: &[String],
) -> anyhow::Result<()> {
    let path = file.unwrap_or_else(|| std::path::Path::new("fractal.toml"));
    if !path.exists() {
        anyhow::bail!("No fractal.toml found. Run 'fractal init <name>' first.");
    }

    let content = std::fs::read_to_string(path)?;
    let config: config::FractalConfig = toml::from_str(&content)?;
    let overrides = deploy::parse_image_overrides(image_overrides)?;

    tokio::runtime::Runtime::new()?.block_on(deploy::deploy(&config, dry_run, &overrides))
}

fn cmd_status(project: Option<&str>) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(status::show_status(project))
}

fn cmd_logs(service: &str, follow: bool) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(logs::stream_logs(service, follow))
}

fn cmd_bootstrap(
    mode: &bootstrap::BootstrapMode,
    name: &str,
    skip_checks: bool,
    registry: Option<&str>,
) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(bootstrap::bootstrap(mode, name, skip_checks, registry))
}

fn cmd_dashboard(port: u16) -> anyhow::Result<()> {
    use anyhow::Context;
    use colored::Colorize;

    println!(
        "{} Port-forwarding Fractal dashboard on port {port}",
        "\u{2022}".cyan()
    );

    let url = format!("http://localhost:{port}");
    println!("  Dashboard: {}", url.cyan());

    // Open browser
    open::that(&url).ok();

    // Port-forward the dashboard service (blocks until terminated)
    let status = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            "svc/fractal-dashboard",
            "-n",
            "fractal-system",
            &format!("{port}:80"),
        ])
        .status()
        .context("failed to start kubectl port-forward")?;

    if !status.success() {
        anyhow::bail!("kubectl port-forward exited with error");
    }

    Ok(())
}

fn cmd_scale(service: &str, replicas: i32) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(scale::scale_service(service, replicas))
}

fn cmd_restart(service: &str) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(restart::restart_service(service))
}

fn cmd_env(action: EnvCommands) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    match action {
        EnvCommands::List { service } => rt.block_on(env::list_env(&service)),
        EnvCommands::Set { service, pair } => rt.block_on(env::set_env(&service, &pair)),
        EnvCommands::SetSecret {
            service,
            name,
            secret,
            key,
        } => rt.block_on(env::set_secret_env(&service, &name, &secret, &key)),
        EnvCommands::Unset { service, name } => rt.block_on(env::unset_env(&service, &name)),
    }
}

fn cmd_destroy(name: &str, yes: bool) -> anyhow::Result<()> {
    use colored::Colorize;

    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Destroy cluster '{name}'? This cannot be undone"
            ))
            .default(false)
            .interact()?;

        if !confirmed {
            println!("Aborted");
            return Ok(());
        }
    }

    bootstrap::cluster::delete_k3d_cluster(name)?;
    println!(
        "\n{} Cluster '{}' destroyed",
        "\u{2713}".green(),
        name.cyan()
    );

    Ok(())
}
