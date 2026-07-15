use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::apps::v1::Deployment;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

/// Trigger a rolling restart for a service.
///
/// Patches the underlying Deployment's pod template annotation to
/// force Kubernetes to roll all pods.
///
/// # Errors
///
/// Returns an error if the service doesn't exist or the deployment
/// patch fails.
pub async fn restart_service(service: &str) -> Result<()> {
    let client = Client::try_default().await?;

    // Verify the FractalService exists
    let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());
    svc_api
        .get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    // Patch the underlying Deployment's pod template annotation
    let deploy_api: Api<Deployment> = Api::default_namespaced(client);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    let patch = serde_json::json!({
        "spec": {
            "template": {
                "metadata": {
                    "annotations": {
                        "kubectl.kubernetes.io/restartedAt": timestamp
                    }
                }
            }
        }
    });

    deploy_api
        .patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context(format!("failed to restart deployment '{service}'"))?;

    println!("{} Restarting '{}'", "\u{2713}".green(), service.cyan());

    Ok(())
}
