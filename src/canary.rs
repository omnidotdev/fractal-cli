//! `fractal canary start | promote | abort | status` -- drive
//! `FractalService.spec.canary` from the CLI without kubectl.
//!
//! All operations patch the FractalService CR via kube-rs. Operator
//! commit 1c0d64b acts on `spec.canary` by spinning up a second
//! Deployment + Service and weighting the HTTPRoute backendRefs.

use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::FractalService;

/// Start a canary rollout on `service` with `image` at `weight`%
/// traffic. Idempotent: subsequent calls update the existing canary.
///
/// # Errors
///
/// Returns an error if the service is missing or the patch fails.
pub async fn start_canary(service: &str, image: &str, weight: u8) -> Result<()> {
    if weight > 100 {
        anyhow::bail!("weight must be between 0 and 100");
    }

    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    api.get(service)
        .await
        .with_context(|| format!("service '{service}' not found"))?;

    let patch = serde_json::json!({
        "spec": {
            "canary": {
                "image": image,
                "weight": weight,
            }
        }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("failed to start canary on '{service}'"))?;

    println!(
        "{} canary started on {} ({}% traffic to {})",
        "\u{2713}".green(),
        service.cyan(),
        weight,
        image.dimmed(),
    );
    Ok(())
}

/// Promote the current canary: copy its image to `spec.source.image`
/// (image-source services only) and clear `spec.canary`. Functions as
/// "ship the new version, retire the old one".
///
/// # Errors
///
/// Returns an error if the service has no active canary, uses a git
/// source (no image to promote), or the patch fails.
pub async fn promote_canary(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);

    let svc = api
        .get(service)
        .await
        .with_context(|| format!("service '{service}' not found"))?;

    let canary = svc
        .spec
        .canary
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no active canary on '{service}'"))?;

    // For image-source services, promote moves the canary image into
    // spec.source. For git-source services, the canary's image came
    // from a sha-tagged build; promote means clearing the canary and
    // letting the operator's normal build path catch up on the next
    // push. We only handle the explicit image case here; bail with a
    // clear error otherwise.
    use fractal_operator::crd::service::ServiceSource;
    let promoted_image = canary.image.clone();
    let new_repo_tag = promoted_image.rsplit_once(':');
    let patch = match (&svc.spec.source, new_repo_tag) {
        (ServiceSource::Image { .. }, Some((repo, tag))) => serde_json::json!({
            "spec": {
                "canary": null,
                "source": { "image": { "repository": repo, "tag": tag } }
            }
        }),
        (ServiceSource::Git { .. }, _) => {
            anyhow::bail!(
                "service '{service}' uses a git source; promote the canary by \
                 pushing the canary commit to the source branch (the next \
                 webhook rebuild ships it), then run `fractal canary abort` \
                 to clear the canary state"
            );
        }
        (ServiceSource::Image { .. }, None) => {
            anyhow::bail!(
                "canary image '{promoted_image}' is missing a `:tag` segment; \
                 can't promote without an explicit tag"
            );
        }
    };

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("failed to promote canary on '{service}'"))?;

    println!(
        "{} canary promoted on {} (now serving {})",
        "\u{2713}".green(),
        service.cyan(),
        promoted_image.dimmed(),
    );
    Ok(())
}

/// Abort the active canary: clear `spec.canary`, sending 100% of
/// traffic back to the primary. The operator deletes the canary
/// Deployment + Service on next reconcile.
///
/// # Errors
///
/// Returns an error if the service is missing or the patch fails.
pub async fn abort_canary(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    api.get(service)
        .await
        .with_context(|| format!("service '{service}' not found"))?;

    let patch = serde_json::json!({
        "spec": { "canary": null }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("failed to abort canary on '{service}'"))?;

    println!(
        "{} canary aborted on {} (100% traffic back to primary)",
        "\u{2713}".green(),
        service.cyan(),
    );
    Ok(())
}

/// Print whether a canary is currently active on `service`, including
/// the canary image and current weight.
///
/// # Errors
///
/// Returns an error if the service is missing or the API call fails.
pub async fn canary_status(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    let svc = api
        .get(service)
        .await
        .with_context(|| format!("service '{service}' not found"))?;

    if let Some(canary) = svc.spec.canary.as_ref() {
        println!("{} canary active on {}", "\u{2713}".green(), service.cyan());
        println!("  image:  {}", canary.image.dimmed());
        println!("  weight: {}%", canary.weight);
    } else {
        println!(
            "{} no active canary on {}",
            "\u{2717}".dimmed(),
            service.cyan()
        );
    }
    Ok(())
}
