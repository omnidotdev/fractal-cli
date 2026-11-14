//! `fractal deploy` - trigger a rebuild on a `FractalService` via the
//! authed Fractal API. Works the same on every transport: the API checks
//! project RBAC (`developer` role or higher), so CI runners need only a
//! token, never cluster access.

use anyhow::Result;
use colored::Colorize;
use reqwest::Method;
use serde::Serialize;

use crate::api;

#[derive(Serialize, Debug, PartialEq, Eq)]
struct RebuildRequest<'a> {
    namespace: &'a str,
    project: &'a str,
    service: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a str>,
}

/// Trigger a rebuild on `service`. `project` is the parent project
/// (used for RBAC); when omitted it is found by searching your projects
/// for the service. The namespace is the one the service lives in, read
/// from the API, unless `--namespace` pins it. `env` narrows the rebuild
/// to one environment; it travels in the request body, never as a
/// derived namespace.
///
/// # Errors
///
/// Returns an error if no credential is configured, the service can't be
/// located, the HTTP call fails, or the caller lacks `developer` or
/// higher on the project.
pub async fn deploy(
    namespace: Option<&str>,
    project: Option<&str>,
    service: &str,
    env: Option<&str>,
) -> Result<()> {
    let client = api::ApiClient::from_env()?;
    let namespace = namespace.map(str::trim).filter(|s| !s.is_empty());
    let env = env.map(str::trim).filter(|s| !s.is_empty());

    let (project, namespace) = match (project, namespace) {
        // Fully specified (the CI shape): no lookups
        (Some(p), Some(ns)) => (p.to_string(), ns.to_string()),
        (project, explicit) => {
            let at = api::resolve::locate_service(&client, project, service).await?;
            let ns = explicit.map_or_else(|| at.namespace(), str::to_string);
            (at.project, ns)
        }
    };

    let _: serde_json::Value = client
        .send(
            Method::POST,
            "/v1/services/rebuild",
            &RebuildRequest {
                namespace: &namespace,
                project: &project,
                service,
                env,
            },
        )
        .await?;

    println!(
        "{} rebuild triggered for {}/{}{}",
        "\u{2713}".green(),
        namespace.cyan(),
        service.cyan(),
        env.map_or_else(String::new, |e| format!(" (env: {e})")),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RebuildRequest;

    #[test]
    fn rebuild_body_matches_api_contract() {
        let body = RebuildRequest {
            namespace: "fractal-demo",
            project: "demo",
            service: "web",
            env: None,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "namespace": "fractal-demo", "project": "demo", "service": "web" })
        );
        let with_env = RebuildRequest {
            env: Some("staging"),
            ..body
        };
        // The env rides in the body; the namespace stays the service's own
        assert_eq!(
            serde_json::to_value(&with_env).unwrap(),
            serde_json::json!({ "namespace": "fractal-demo", "project": "demo", "service": "web", "env": "staging" })
        );
    }
}
