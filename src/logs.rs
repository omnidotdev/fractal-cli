use colored::Colorize;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use kube::Client;

/// Stream logs from pods matching a `FractalService` name.
///
/// Finds pods via the `app.kubernetes.io/name` label selector and
/// streams logs from the first matching pod. When `follow` is true,
/// the log stream will remain open for new output.
///
/// # Errors
///
/// Returns an error if the Kubernetes client cannot be created, the
/// pod list operation fails, or log retrieval fails.
pub async fn stream_logs(service: &str, follow: bool) -> anyhow::Result<()> {
    let client = Client::try_default().await?;

    // Find pods matching the service name via label selector
    let pod_api: Api<Pod> = Api::default_namespaced(client);
    let lp = ListParams::default().labels(&format!("app.kubernetes.io/name={service}"));

    let pods = pod_api.list(&lp).await?;

    if pods.items.is_empty() {
        println!(
            "{} No pods found for service '{service}'. Is it deployed?",
            "!".yellow()
        );
        return Ok(());
    }

    let pod_name = pods.items[0]
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Pod has no name"))?;

    println!(
        "{} Streaming logs from pod '{}'...",
        "\u{2192}".cyan(),
        pod_name.cyan()
    );

    let log_params = LogParams {
        follow,
        tail_lines: Some(100),
        ..Default::default()
    };

    let logs = pod_api.logs(pod_name, &log_params).await?;
    print!("{logs}");

    Ok(())
}
