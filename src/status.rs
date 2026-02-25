use colored::Colorize;
use kube::api::{Api, ListParams};
use kube::Client;

use fractal_operator::crd::service::FractalService;

/// Display status of deployed `FractalService` resources.
///
/// When `project` is provided, filters results by the
/// `fractal.omni.dev/project` label. Otherwise lists all services
/// in the default namespace.
///
/// # Errors
///
/// Returns an error if the Kubernetes client cannot be created or
/// if the list operation fails.
pub async fn show_status(project: Option<&str>) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    let lp = project.map_or_else(ListParams::default, |proj| {
        ListParams::default().labels(&format!("fractal.omni.dev/project={proj}"))
    });

    let services = api.list(&lp).await?;

    if services.items.is_empty() {
        println!("No services found. Run 'fractal deploy' first.");
        return Ok(());
    }

    println!(
        "{:<25} {:<12} {:<15} {:<10}",
        "SERVICE".bold(),
        "TYPE".bold(),
        "STATUS".bold(),
        "REPLICAS".bold()
    );

    for svc in &services.items {
        let name = svc.metadata.name.as_deref().unwrap_or("unknown");
        let svc_type = format!("{:?}", svc.spec.service_type).to_lowercase();

        let (phase, ready, desired) = svc.status.as_ref().map_or_else(
            || ("Pending".to_string(), 0, 0),
            |s| (s.phase.clone(), s.ready_replicas, s.desired_replicas),
        );

        let phase_colored = match phase.as_str() {
            "Running" => phase.green(),
            "Deploying" | "Building" => phase.yellow(),
            "Failed" => phase.red(),
            _ => phase.normal(),
        };

        println!(
            "{name:<25} {svc_type:<12} {phase_colored:<15} {ready}/{desired}"
        );
    }

    Ok(())
}
