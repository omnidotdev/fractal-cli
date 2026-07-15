use anyhow::{Context, Result, bail};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

/// Scale a service to the specified number of replicas.
///
/// # Errors
///
/// Returns an error if the replica count is negative, the service
/// doesn't exist, or the patch fails.
pub async fn scale_service(service: &str, replicas: i32) -> Result<()> {
    if replicas < 0 {
        bail!("replicas must be >= 0, got {replicas}");
    }

    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    // Verify the service exists
    api.get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let patch = serde_json::json!({
        "spec": {
            "deploy": {
                "replicas": replicas
            }
        }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("failed to patch replicas")?;

    println!(
        "{} Scaled '{}' to {} replica{}",
        "\u{2713}".green(),
        service.cyan(),
        replicas,
        if replicas == 1 { "" } else { "s" }
    );

    Ok(())
}
