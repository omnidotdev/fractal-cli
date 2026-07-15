use anyhow::{Result, bail};
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

pub async fn restore_service(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    let svc = api.get(service).await?;
    let phase = svc.status.as_ref().map_or("Unknown", |s| s.phase.as_str());

    if phase != "PendingDeletion" {
        bail!("Service '{service}' is not pending deletion (current phase: {phase})");
    }

    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                "fractal.omni.dev/restore": chrono::Utc::now().to_rfc3339()
            }
        }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    println!("Service '{service}' restored successfully");
    Ok(())
}
