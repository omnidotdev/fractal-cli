use std::io::Write;

use anyhow::{bail, Context, Result};
use colored::Colorize;

const NAMESPACE_YAML: &str = include_str!("../../../fractal-operator/manifests/namespace.yaml");
const RBAC_YAML: &str = include_str!("../../../fractal-operator/manifests/rbac.yaml");
const DEPLOYMENT_YAML: &str = include_str!("../../../fractal-operator/manifests/deployment.yaml");
const SERVICE_YAML: &str = include_str!("../../../fractal-operator/manifests/service.yaml");
const DASHBOARD_YAML: &str = include_str!("../../../fractal-app/manifests/deployment.yaml");

/// Render the deployment YAML with the given registry URL
#[must_use]
pub fn render_deployment_yaml(registry: &str) -> String {
    DEPLOYMENT_YAML.replace("__FRACTAL_REGISTRY__", registry)
}

/// Apply operator manifests (namespace, RBAC, deployment) to the cluster.
///
/// Uses `kubectl apply -f -` with embedded YAML piped via stdin.
/// This installs the operator for in-cluster execution.
///
/// # Errors
///
/// Returns an error if `kubectl` is not found or any manifest
/// application fails.
pub fn apply_operator_manifests(registry: &str) -> Result<()> {
    println!("{} Deploying Fractal operator to cluster", "\u{2022}".cyan());

    let deployment = render_deployment_yaml(registry);

    for (label, yaml) in [
        ("namespace", NAMESPACE_YAML),
        ("RBAC", RBAC_YAML),
        ("deployment", deployment.as_str()),
        ("metrics service", SERVICE_YAML),
        ("dashboard", DASHBOARD_YAML),
    ] {
        apply_yaml(label, yaml)?;
    }

    println!(
        "\n{} Fractal operator and dashboard deployed to cluster",
        "\u{2713}".green()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_deployment_yaml_substitutes_registry() {
        let rendered = render_deployment_yaml("ghcr.io/omnidotdev");
        assert!(rendered.contains("ghcr.io/omnidotdev"));
        assert!(!rendered.contains("__FRACTAL_REGISTRY__"));
    }

    #[test]
    fn test_render_deployment_yaml_localhost_registry() {
        let rendered = render_deployment_yaml("k3d-fractal-registry.localhost:5000");
        assert!(rendered.contains("k3d-fractal-registry.localhost:5000"));
        assert!(!rendered.contains("__FRACTAL_REGISTRY__"));
    }
}

/// Pipe a YAML manifest into `kubectl apply -f -`.
///
/// # Errors
///
/// Returns an error if `kubectl` cannot be started, stdin cannot be
/// written to, or the apply command exits with a non-zero status.
fn apply_yaml(label: &str, yaml: &str) -> Result<()> {
    let mut child = std::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to start kubectl")?;

    child
        .stdin
        .take()
        .context("failed to open kubectl stdin")?
        .write_all(yaml.as_bytes())
        .context("failed to write manifest to kubectl")?;

    let status = child.wait().context("kubectl process failed")?;
    if !status.success() {
        bail!("kubectl apply failed for {label} manifest");
    }

    println!("{} Applied {label} manifest", "\u{2713}".green());
    Ok(())
}
