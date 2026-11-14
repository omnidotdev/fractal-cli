//! `fractal services` -- enumerate `FractalService`s in a namespace.
//! Convenience for quickly checking what's deployed without paging
//! through the dashboard or `kubectl get fsvc`. Pass `--json` for a
//! machine-readable listing (CI/agents); the JSON rows also surface the
//! monorepo build subdir (`rootDir`), webhook `watchPaths`, and cron
//! `schedule` that the human table omits.

use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, ListParams};
use serde::Serialize;

use fractal_operator::crd::service::{FractalService, ServiceType};

use crate::api;

/// A single service as rendered for `--json`. Field names are
/// camelCase to match the platform's CRD wire convention. Monorepo and
/// cron fields are omitted when empty so a plain web service stays
/// terse.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceRow {
    name: String,
    service_type: String,
    phase: String,
    ready_replicas: i32,
    desired_replicas: i32,
    image: String,
    /// Monorepo build subdirectory (`spec.build.rootDir`), when set
    #[serde(skip_serializing_if = "Option::is_none")]
    root_dir: Option<String>,
    /// Webhook rebuild path filters (`spec.build.watchPaths`)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    watch_paths: Vec<String>,
    /// Cron schedule (`spec.deploy.schedule`), set for `cronJob` services
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule: Option<String>,
}

/// Human/JSON label for a service type, matching the CRD's camelCase
/// serialization so the two never diverge.
const fn service_type_label(service_type: &ServiceType) -> &'static str {
    match service_type {
        ServiceType::Web => "web",
        ServiceType::Worker => "worker",
        ServiceType::CronJob => "cronJob",
        ServiceType::StaticSite => "staticSite",
        ServiceType::Database => "database",
    }
}

/// Resolve the image to show: prefer the running image from status,
/// else the pinned source image, else a placeholder.
fn resolve_image(svc: &FractalService) -> String {
    svc.status
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
        })
}

/// Build the machine-readable row for a service.
fn service_row(svc: &FractalService) -> ServiceRow {
    let name = svc
        .metadata
        .name
        .as_deref()
        .unwrap_or("(unknown)")
        .to_string();
    let phase = svc
        .status
        .as_ref()
        .map_or("Pending", |s| s.phase.as_str())
        .to_string();
    let ready = svc.status.as_ref().map_or(0, |s| s.ready_replicas);
    let desired = svc.status.as_ref().map_or(0, |s| s.desired_replicas);
    ServiceRow {
        name,
        service_type: service_type_label(&svc.spec.service_type).to_string(),
        phase,
        ready_replicas: ready,
        desired_replicas: desired,
        image: resolve_image(svc),
        root_dir: svc.spec.build.root_dir.clone(),
        watch_paths: svc.spec.build.watch_paths.clone(),
        schedule: svc.spec.deploy.schedule.clone(),
    }
}

/// List all `FractalService`s in `namespace` (defaulting to the
/// kubeconfig's current namespace) with their type, phase, ready
/// replicas, and current image. With `json`, emits a JSON array of
/// rows (including monorepo/cron fields) instead of the human table.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable, the list call
/// fails, or JSON serialization fails.
pub async fn list_services(namespace: Option<String>, json: bool) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = match namespace.as_deref() {
        Some(ns) => Api::namespaced(client, ns),
        None => Api::default_namespaced(client),
    };

    let services = api
        .list(&ListParams::default())
        .await
        .context("failed to list FractalServices")?;

    if json {
        let rows: Vec<ServiceRow> = services.items.iter().map(service_row).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if services.items.is_empty() {
        println!("{}", "(no services)".dimmed());
        return Ok(());
    }

    println!(
        "{:<32}  {:<10}  {:<10}  {:<8}  {}",
        "NAME".bold(),
        "TYPE".bold(),
        "PHASE".bold(),
        "READY".bold(),
        "IMAGE".bold(),
    );
    for svc in &services.items {
        let row = service_row(svc);
        let phase_colored = color_phase(&row.phase);
        let ready_label = format!("{}/{}", row.ready_replicas, row.desired_replicas);
        println!(
            "{:<32}  {:<10}  {:<10}  {:<8}  {}",
            row.name.cyan(),
            row.service_type.dimmed(),
            phase_colored,
            ready_label.dimmed(),
            row.image.dimmed(),
        );
    }

    Ok(())
}

fn color_phase(phase: &str) -> String {
    match phase {
        "Running" => phase.green().to_string(),
        "Crashed" | "Failed" => phase.red().to_string(),
        "PendingDeletion" => phase.yellow().to_string(),
        _ => phase.dimmed().to_string(),
    }
}

/// Build the machine-readable row for a service read over the API. The
/// shape is identical to the kube transport's row.
fn api_service_row(svc: &api::model::Service) -> ServiceRow {
    let status = svc.status.as_ref();
    let image = status
        .and_then(|s| s.current_image.clone())
        .filter(|i| !i.is_empty())
        .or_else(|| {
            let src = svc.spec.source.as_ref()?;
            Some(format!(
                "{}:{}",
                src.repository.as_ref()?,
                src.tag.as_ref()?
            ))
        })
        .unwrap_or_else(|| "(no image)".to_string());
    let build = svc.spec.build.as_ref();
    ServiceRow {
        name: svc.name().to_string(),
        service_type: svc.spec.service_type.clone(),
        phase: status
            .and_then(|s| s.phase.clone())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "Pending".to_string()),
        ready_replicas: status.and_then(|s| s.ready_replicas).unwrap_or(0),
        desired_replicas: status.and_then(|s| s.desired_replicas).unwrap_or(0),
        image,
        root_dir: build
            .and_then(|b| b.root_dir.clone())
            .filter(|d| !d.is_empty()),
        watch_paths: build.map(|b| b.watch_paths.clone()).unwrap_or_default(),
        schedule: svc
            .spec
            .deploy
            .as_ref()
            .and_then(|d| d.schedule.clone())
            .filter(|s| !s.is_empty()),
    }
}

/// Collect `(project, row)` pairs over the API: one project when
/// `project` is set, else every visible project. `namespace` narrows the
/// rows to that namespace.
async fn collect_api_rows(
    client: &api::ApiClient,
    project: Option<&str>,
    namespace: Option<&str>,
) -> Result<Vec<(String, ServiceRow)>> {
    let projects = match project {
        Some(ident) => vec![api::resolve::find_project(client, ident).await?],
        None => api::resolve::list_projects(client).await?,
    };
    let mut rows = Vec::new();
    for proj in projects {
        for svc in api::resolve::list_services(client, proj.name()).await? {
            if namespace.is_some_and(|ns| svc.namespace() != Some(ns)) {
                continue;
            }
            rows.push((proj.name().to_string(), api_service_row(&svc)));
        }
    }
    Ok(rows)
}

/// List services over the hosted API. `--json` emits the same row shape
/// as the kube transport; the human table adds a PROJECT column.
///
/// # Errors
///
/// Returns an error if the API is unreachable or rejects the request.
pub async fn list_services_api(
    project: Option<&str>,
    namespace: Option<&str>,
    json: bool,
) -> Result<()> {
    let client = api::ApiClient::from_env()?;
    let rows = collect_api_rows(&client, project, namespace).await?;

    if json {
        let rows: Vec<&ServiceRow> = rows.iter().map(|(_, r)| r).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("{}", "(no services)".dimmed());
        return Ok(());
    }

    println!(
        "{:<24}  {:<32}  {:<10}  {:<10}  {:<8}  {}",
        "PROJECT".bold(),
        "NAME".bold(),
        "TYPE".bold(),
        "PHASE".bold(),
        "READY".bold(),
        "IMAGE".bold(),
    );
    for (proj, row) in &rows {
        let ready_label = format!("{}/{}", row.ready_replicas, row.desired_replicas);
        println!(
            "{:<24}  {:<32}  {:<10}  {:<10}  {:<8}  {}",
            proj.dimmed(),
            row.name.cyan(),
            row.service_type.dimmed(),
            color_phase(&row.phase),
            ready_label.dimmed(),
            row.image.dimmed(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ServiceRow, ServiceType, api_service_row, service_type_label};
    use crate::api::model::{ServicesData, fixtures::SERVICES_JSON};
    use crate::api::parse_graphql;

    #[test]
    fn api_rows_match_kube_row_shape() {
        let d: ServicesData = parse_graphql(200, SERVICES_JSON).unwrap();
        let web = serde_json::to_value(api_service_row(&d.fractal_services[0])).unwrap();
        assert_eq!(
            web,
            serde_json::json!({
                "name": "web",
                "serviceType": "web",
                "phase": "Running",
                "readyReplicas": 2,
                "desiredReplicas": 2,
                "image": "reg/demo-web:sha-abc1234",
                "rootDir": "apps/web",
                "watchPaths": ["apps/web", "packages/ui"],
            })
        );
        let cron = serde_json::to_value(api_service_row(&d.fractal_services[1])).unwrap();
        assert_eq!(
            cron,
            serde_json::json!({
                "name": "nightly",
                "serviceType": "cronJob",
                "phase": "Pending",
                "readyReplicas": 0,
                "desiredReplicas": 0,
                "image": "reg/nightly:v1",
                "schedule": "0 3 * * *",
            })
        );
    }

    #[test]
    fn service_type_label_matches_crd_camelcase() {
        assert_eq!(service_type_label(&ServiceType::Web), "web");
        assert_eq!(service_type_label(&ServiceType::Worker), "worker");
        assert_eq!(service_type_label(&ServiceType::CronJob), "cronJob");
        assert_eq!(service_type_label(&ServiceType::StaticSite), "staticSite");
        assert_eq!(service_type_label(&ServiceType::Database), "database");
    }

    #[test]
    fn row_serializes_monorepo_and_cron_fields() {
        let row = ServiceRow {
            name: "reports".to_string(),
            service_type: "cronJob".to_string(),
            phase: "Running".to_string(),
            ready_replicas: 1,
            desired_replicas: 1,
            image: "reg/app:abc1234".to_string(),
            root_dir: Some("apps/web".to_string()),
            watch_paths: vec!["apps/web".to_string(), "packages/ui".to_string()],
            schedule: Some("0 * * * *".to_string()),
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["name"], "reports");
        assert_eq!(v["serviceType"], "cronJob");
        assert_eq!(v["readyReplicas"], 1);
        assert_eq!(v["desiredReplicas"], 1);
        assert_eq!(v["rootDir"], "apps/web");
        assert_eq!(v["watchPaths"][1], "packages/ui");
        assert_eq!(v["schedule"], "0 * * * *");
    }

    #[test]
    fn row_omits_empty_optional_fields() {
        let row = ServiceRow {
            name: "api".to_string(),
            service_type: "web".to_string(),
            phase: "Running".to_string(),
            ready_replicas: 2,
            desired_replicas: 2,
            image: "reg/api:def5678".to_string(),
            root_dir: None,
            watch_paths: Vec::new(),
            schedule: None,
        };
        let obj = serde_json::to_value(&row).unwrap();
        let map = obj.as_object().unwrap();
        assert!(!map.contains_key("rootDir"));
        assert!(!map.contains_key("watchPaths"));
        assert!(!map.contains_key("schedule"));
    }
}
