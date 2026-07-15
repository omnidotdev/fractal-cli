//! `fractal services list` -- enumerate `FractalService`s in a
//! namespace. Convenience for quickly checking what's deployed
//! without paging through the dashboard or `kubectl get fsvc`.

use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, ListParams};

use fractal_operator::crd::service::FractalService;

/// List all `FractalService`s in `namespace` (defaulting to the
/// kubeconfig's current namespace) with their phase, ready replicas,
/// and current image.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable or the list call
/// fails.
pub async fn list_services(namespace: Option<String>) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = match namespace.as_deref() {
        Some(ns) => Api::namespaced(client, ns),
        None => Api::default_namespaced(client),
    };

    let services = api
        .list(&ListParams::default())
        .await
        .context("failed to list FractalServices")?;

    if services.items.is_empty() {
        println!("{}", "(no services)".dimmed());
        return Ok(());
    }

    println!(
        "{:<32}  {:<10}  {:<8}  {}",
        "NAME".bold(),
        "PHASE".bold(),
        "READY".bold(),
        "IMAGE".bold(),
    );
    for svc in services.items {
        let name = svc
            .metadata
            .name
            .as_deref()
            .unwrap_or("(unknown)")
            .to_string();
        let phase = svc
            .status
            .as_ref()
            .map(|s| s.phase.as_str())
            .unwrap_or("Pending");
        let ready = svc.status.as_ref().map_or(0, |s| s.ready_replicas);
        let desired = svc.status.as_ref().map_or(0, |s| s.desired_replicas);
        let image: String = svc
            .status
            .as_ref()
            .and_then(|s| s.current_image.clone())
            .unwrap_or_else(|| {
                if let fractal_operator::crd::service::ServiceSource::Image { repository, tag } =
                    &svc.spec.source
                {
                    format!("{repository}:{tag}")
                } else {
                    "(no image)".to_string()
                }
            });
        let phase_colored = match phase {
            "Running" => phase.green().to_string(),
            "Crashed" | "Failed" => phase.red().to_string(),
            "PendingDeletion" => phase.yellow().to_string(),
            _ => phase.dimmed().to_string(),
        };
        let ready_label = format!("{ready}/{desired}");
        println!(
            "{:<32}  {:<10}  {:<8}  {}",
            name.cyan(),
            phase_colored,
            ready_label.dimmed(),
            image.dimmed(),
        );
    }

    Ok(())
}
