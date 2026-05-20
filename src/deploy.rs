//! `fractal deploy` - trigger a rebuild on a FractalService via the
//! authed fractal-api endpoint. Designed for CI use; the existing
//! `fractal restart` does the same job via kubectl, but assumes the
//! caller has cluster RBAC. `deploy` goes through fractal-api's
//! project RBAC instead (Gatekeeper JWT + `developer` role required).

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Serialize;

use crate::cluster::api_session;

#[derive(Serialize)]
struct RebuildRequest<'a> {
    namespace: &'a str,
    project: &'a str,
    service: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a str>,
}

/// Trigger a rebuild on `namespace/service` (a FractalService).
/// `project` is the parent FractalProject name (used for RBAC) and
/// `env` optionally narrows the rebuild to a single environment.
///
/// # Errors
///
/// Returns an error if env vars are missing, the HTTP call fails, the
/// server returns a non-2xx status, or the caller doesn't have
/// `developer` or higher on the project.
pub async fn deploy(
    namespace: &str,
    project: &str,
    service: &str,
    env: Option<String>,
) -> Result<()> {
    let (base, token, client) = api_session()?;
    let url = format!(
        "{}/v1/services/rebuild",
        base.trim_end_matches('/'),
    );
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&RebuildRequest {
            namespace,
            project,
            service,
            env: env.as_deref(),
        })
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "rebuild endpoint returned {status}: {}",
            if body.is_empty() { "<empty body>" } else { &body }
        );
    }

    println!(
        "{} rebuild triggered for {}/{}{}",
        "\u{2713}".green(),
        namespace.cyan(),
        service.cyan(),
        env.as_deref()
            .map_or_else(String::new, |e| format!(" (env: {e})")),
    );
    Ok(())
}
