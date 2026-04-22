use anyhow::Result;
use kube::api::{Api, DeleteParams};
use kube::Client;

use fractal_operator::crd::service::FractalService;

pub async fn destroy_service(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    let svc = api.get(service).await?;
    let phase = svc.status.as_ref().map_or("Unknown", |s| s.phase.as_str());

    if phase == "PendingDeletion" {
        let deletion_at = svc
            .status
            .as_ref()
            .and_then(|s| s.deletion_scheduled_at.as_deref())
            .unwrap_or("unknown");
        println!("Service '{service}' is already pending deletion (scheduled: {deletion_at})");
        println!("To restore: fractal restore {service}");
        return Ok(());
    }

    api.delete(service, &DeleteParams::default()).await?;
    println!("Service '{service}' marked for deletion");
    println!("A grace period applies before permanent deletion.");
    println!("To restore: fractal restore {service}");
    Ok(())
}
