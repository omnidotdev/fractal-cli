use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde::Deserialize;

/// Partial representation of a k3d cluster entry from `k3d cluster list -o json`
#[derive(Debug, Deserialize)]
struct K3dCluster {
    name: String,
}

/// Partial representation of a k3d registry entry from `k3d registry list -o json`
#[derive(Debug, Deserialize)]
struct K3dRegistry {
    name: String,
}

/// Check whether a k3d cluster with the given name already exists.
///
/// Parses the JSON output of `k3d cluster list` and looks for an
/// exact name match.
///
/// # Errors
///
/// Returns an error if the `k3d` command fails or its output cannot
/// be parsed.
pub fn k3d_cluster_exists(name: &str) -> Result<bool> {
    let output = Command::new("k3d")
        .args(["cluster", "list", "-o", "json"])
        .output()
        .context("failed to run `k3d cluster list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("k3d cluster list failed: {stderr}");
    }

    let clusters: Vec<K3dCluster> =
        serde_json::from_slice(&output.stdout).context("failed to parse k3d cluster list JSON")?;

    Ok(clusters.iter().any(|c| c.name == name))
}

/// Check whether a k3d registry with the given name already exists.
///
/// # Errors
///
/// Returns an error if the `k3d` command fails or its output cannot
/// be parsed.
pub fn k3d_registry_exists(name: &str) -> Result<bool> {
    let output = Command::new("k3d")
        .args(["registry", "list", "-o", "json"])
        .output()
        .context("failed to run `k3d registry list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("k3d registry list failed: {stderr}");
    }

    let registries: Vec<K3dRegistry> =
        serde_json::from_slice(&output.stdout).context("failed to parse k3d registry list JSON")?;

    let registry_name = format!("k3d-{name}-registry");
    Ok(registries.iter().any(|r| r.name == registry_name))
}

/// Create a local k3d registry for development.
///
/// Idempotent: skips creation if the registry already exists.
///
/// # Errors
///
/// Returns an error if the `k3d` command fails.
pub fn create_k3d_registry(name: &str) -> Result<()> {
    let registry_name = format!("{name}-registry");

    if k3d_registry_exists(name)? {
        println!(
            "{} k3d registry '{}' already exists, skipping",
            "\u{2713}".green(),
            registry_name.cyan()
        );
        return Ok(());
    }

    println!(
        "{} Creating k3d registry '{}'",
        "\u{2022}".cyan(),
        registry_name.cyan()
    );

    let status = Command::new("k3d")
        .args(["registry", "create", &registry_name, "--port", "5000"])
        .status()
        .context("failed to run `k3d registry create`")?;

    if !status.success() {
        bail!("k3d registry create failed");
    }

    println!(
        "{} k3d registry '{}' created on port 5000",
        "\u{2713}".green(),
        registry_name.cyan()
    );

    Ok(())
}

/// Create a local k3d cluster for development.
///
/// Checks for an existing cluster first (bails if one exists),
/// creates the cluster with standard port mappings, and merges the
/// kubeconfig into the default location. When `registry` is provided,
/// the cluster is configured to use the given k3d registry.
///
/// # Errors
///
/// Returns an error if the cluster already exists, if any `k3d`
/// invocation fails, or if kubeconfig merging fails.
pub fn create_k3d_cluster(name: &str, registry: Option<&str>) -> Result<()> {
    println!(
        "{} Checking for existing k3d cluster '{}'",
        "\u{2022}".cyan(),
        name.cyan()
    );

    if k3d_cluster_exists(name)? {
        bail!("k3d cluster '{name}' already exists");
    }

    println!(
        "{} Creating k3d cluster '{}'",
        "\u{2022}".cyan(),
        name.cyan()
    );

    let mut args = vec![
        "cluster",
        "create",
        name,
        "--api-port",
        "6443",
        "--port",
        "8080:80@loadbalancer",
        "--wait",
    ];

    let registry_use = registry.map(|r| format!("k3d-{r}-registry.localhost:5000"));
    if let Some(ref reg) = registry_use {
        args.push("--registry-use");
        args.push(reg);
    }

    let status = Command::new("k3d")
        .args(&args)
        .status()
        .context("failed to run `k3d cluster create`")?;

    if !status.success() {
        bail!("k3d cluster create failed");
    }

    println!("{} k3d cluster '{}' created", "\u{2713}".green(), name.cyan());

    // Merge kubeconfig so kubectl can reach the new cluster
    println!("{} Merging kubeconfig", "\u{2022}".cyan());

    let merge_status = Command::new("k3d")
        .args([
            "kubeconfig",
            "merge",
            name,
            "--kubeconfig-merge-default",
        ])
        .status()
        .context("failed to run `k3d kubeconfig merge`")?;

    if !merge_status.success() {
        bail!("k3d kubeconfig merge failed");
    }

    println!(
        "{} Kubeconfig merged for cluster '{}'",
        "\u{2713}".green(),
        name.cyan()
    );

    Ok(())
}

/// Delete a k3d cluster by name.
///
/// # Errors
///
/// Returns an error if the `k3d cluster delete` command fails.
pub fn delete_k3d_cluster(name: &str) -> Result<()> {
    println!(
        "{} Deleting k3d cluster '{}'",
        "\u{2022}".cyan(),
        name.cyan()
    );

    let status = Command::new("k3d")
        .args(["cluster", "delete", name])
        .status()
        .context("failed to run `k3d cluster delete`")?;

    if !status.success() {
        bail!("k3d cluster delete failed for '{name}'");
    }

    println!(
        "{} k3d cluster '{}' deleted",
        "\u{2713}".green(),
        name.cyan()
    );

    Ok(())
}

/// Install k3s for production/homelab use.
///
/// Downloads the official k3s install script and runs it with Traefik
/// disabled (Fractal manages its own ingress). Waits for the
/// kubeconfig to appear, then copies it to `~/.kube/config`.
///
/// # Errors
///
/// Returns an error if the install script fails, the kubeconfig never
/// appears, or copying the kubeconfig fails.
pub fn install_k3s() -> Result<()> {
    println!("{} Installing k3s", "\u{2022}".cyan());

    // Pipe the installer through sh with Traefik disabled
    let curl = Command::new("curl")
        .args(["-sfL", "https://get.k3s.io"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to start curl for k3s installer")?;

    let curl_stdout = curl
        .stdout
        .context("failed to capture curl stdout")?;

    let sh_status = Command::new("sh")
        .stdin(curl_stdout)
        .env("INSTALL_K3S_EXEC", "--disable=traefik")
        .status()
        .context("failed to run k3s install script")?;

    if !sh_status.success() {
        bail!("k3s install script failed");
    }

    println!("{} k3s installed", "\u{2713}".green());

    // Wait for the kubeconfig to appear
    let k3s_kubeconfig = std::path::Path::new("/etc/rancher/k3s/k3s.yaml");
    let max_wait = Duration::from_secs(60);
    let poll_interval = Duration::from_secs(2);
    let mut elapsed = Duration::ZERO;

    println!(
        "{} Waiting for k3s kubeconfig at {}",
        "\u{2022}".cyan(),
        k3s_kubeconfig.display()
    );

    while !k3s_kubeconfig.exists() {
        if elapsed >= max_wait {
            bail!(
                "timed out waiting for k3s kubeconfig at {}",
                k3s_kubeconfig.display()
            );
        }
        thread::sleep(poll_interval);
        elapsed += poll_interval;
    }

    // Copy kubeconfig to the standard location
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let kube_dir = std::path::PathBuf::from(&home).join(".kube");
    std::fs::create_dir_all(&kube_dir).context("failed to create ~/.kube directory")?;

    let dest = kube_dir.join("config");

    std::fs::copy(k3s_kubeconfig, &dest).context("failed to copy k3s kubeconfig to ~/.kube/config")?;

    println!(
        "{} Kubeconfig written to {}",
        "\u{2713}".green(),
        dest.display()
    );

    Ok(())
}
