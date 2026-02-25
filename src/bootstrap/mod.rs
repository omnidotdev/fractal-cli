pub mod cluster;
pub mod crd;
pub mod manifests;
pub mod preflight;

use anyhow::Result;
use colored::Colorize;

/// Bootstrap mode determines how the Fractal platform is set up
#[derive(Clone, Debug, clap::ValueEnum)]
pub enum BootstrapMode {
    /// Local dev (k3d, operator runs out-of-cluster)
    Dev,
    /// Homelab/production (k3s, operator runs in-cluster)
    Production,
    /// Connect to existing cluster (install CRDs + operator only)
    Connect,
}

/// Set up the Fractal platform for the given mode.
///
/// Runs preflight checks (unless skipped), then provisions the
/// cluster, applies CRDs, and deploys the operator as appropriate
/// for the selected mode.
///
/// # Errors
///
/// Returns an error if preflight checks fail or any provisioning
/// step encounters a problem.
pub async fn bootstrap(
    mode: &BootstrapMode,
    name: &str,
    skip_checks: bool,
    registry: Option<&str>,
) -> Result<()> {
    if !skip_checks {
        preflight::check(mode)?;
    }

    match mode {
        BootstrapMode::Dev => {
            let default_registry = format!("k3d-{name}-registry.localhost:5000");
            let registry = registry.unwrap_or(&default_registry);
            cluster::create_k3d_registry(name)?;
            cluster::create_k3d_cluster(name, Some(name))?;
            let client = kube::Client::try_default().await?;
            crd::apply_crds(&client).await?;
            manifests::apply_operator_manifests(registry)?;
        }
        BootstrapMode::Production => {
            let registry = registry.unwrap_or("ghcr.io/omnidotdev");
            cluster::install_k3s()?;
            let client = kube::Client::try_default().await?;
            crd::apply_crds(&client).await?;
            manifests::apply_operator_manifests(registry)?;
        }
        BootstrapMode::Connect => {
            let registry = registry.unwrap_or("ghcr.io/omnidotdev");
            let client = kube::Client::try_default().await?;
            crd::apply_crds(&client).await?;
            manifests::apply_operator_manifests(registry)?;
        }
    }

    let mode_label = match mode {
        BootstrapMode::Dev => "dev",
        BootstrapMode::Production => "production",
        BootstrapMode::Connect => "connect",
    };

    println!(
        "\n{} Fractal bootstrap complete ({})",
        "\u{2713}".green(),
        mode_label.cyan()
    );

    Ok(())
}
