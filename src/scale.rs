use anyhow::{Context, Result, bail};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

use crate::api;
use crate::api::model::update_service_mutation;

/// `updateFractalService` input that sets the replica count
fn scale_input(project: &str, service: &str, replicas: i32) -> serde_json::Value {
    serde_json::json!({
        "input": { "project": project, "name": service, "deploy": { "replicas": replicas } }
    })
}

/// Scale a service over the hosted API
///
/// # Errors
///
/// Returns an error if the replica count is negative, the service can't
/// be found, or the update fails.
pub async fn scale_service_api(project: Option<&str>, service: &str, replicas: i32) -> Result<()> {
    if replicas < 0 {
        bail!("replicas must be >= 0, got {replicas}");
    }
    let client = api::ApiClient::from_env()?;
    let at = api::resolve::locate_service(&client, project, service).await?;
    let _: serde_json::Value = client
        .graphql(
            &update_service_mutation(),
            scale_input(&at.project, service, replicas),
        )
        .await?;
    println!(
        "{} Scaled '{}' to {} replica{}",
        "\u{2713}".green(),
        service.cyan(),
        replicas,
        if replicas == 1 { "" } else { "s" }
    );
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::scale_input;

    #[test]
    fn scale_input_sets_only_replicas() {
        assert_eq!(
            scale_input("demo", "web", 3),
            serde_json::json!({ "input": { "project": "demo", "name": "web", "deploy": { "replicas": 3 } } })
        );
    }
}
