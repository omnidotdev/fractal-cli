use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client, CustomResourceExt};

use fractal_operator::crd::{FractalProject, FractalService};

/// Apply Fractal CRDs to the connected cluster via server-side apply.
///
/// Installs (or updates) the `FractalProject` and `FractalService`
/// custom resource definitions without requiring `kubectl`.
///
/// # Errors
///
/// Returns an error if the Kubernetes API is unreachable or a CRD
/// patch fails.
pub async fn apply_crds(client: &Client) -> Result<()> {
    println!("{} Applying Fractal CRDs", "\u{2022}".cyan());

    let crd_api: Api<CustomResourceDefinition> = Api::all(client.clone());
    let params = PatchParams::apply("fractal-bootstrap");

    // Apply `FractalProject` CRD
    let project_crd = FractalProject::crd();
    let name = project_crd
        .metadata
        .name
        .as_deref()
        .context("FractalProject CRD missing metadata.name")?;

    crd_api
        .patch(name, &params, &Patch::Apply(&project_crd))
        .await
        .context("failed to apply FractalProject CRD")?;

    println!(
        "{} Applied CRD '{}'",
        "\u{2713}".green(),
        name.cyan()
    );

    // Apply `FractalService` CRD
    let service_crd = FractalService::crd();
    let name = service_crd
        .metadata
        .name
        .as_deref()
        .context("FractalService CRD missing metadata.name")?;

    crd_api
        .patch(name, &params, &Patch::Apply(&service_crd))
        .await
        .context("failed to apply FractalService CRD")?;

    println!(
        "{} Applied CRD '{}'",
        "\u{2713}".green(),
        name.cyan()
    );

    println!(
        "\n{} All Fractal CRDs applied",
        "\u{2713}".green()
    );

    Ok(())
}
