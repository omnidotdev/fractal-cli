use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

/// Trigger a rollback for a service.
///
/// Patches the `fractal.omni.dev/rollback-to` annotation on the
/// `FractalService` CR. The operator watches for this annotation and
/// performs the rollback automatically.
///
/// # Errors
///
/// Returns an error if the service doesn't exist or the annotation
/// patch fails.
pub async fn rollback_service(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    // Verify the service exists
    api.get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                "fractal.omni.dev/rollback-to": "previous"
            }
        }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context(format!("failed to trigger rollback for '{service}'"))?;

    println!(
        "{} Rollback triggered for '{}'",
        "\u{2713}".green(),
        service.cyan()
    );

    Ok(())
}
