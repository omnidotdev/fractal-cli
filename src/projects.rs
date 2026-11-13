//! `fractal projects list | get` -- enumerate / inspect
//! `FractalProject` resources. Useful for CI scripts that need to
//! discover the namespace for a project name (e.g. to drive
//! `fractal deploy`). Pass `--json` for machine-readable output.

use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, ListParams};
use serde::Serialize;

use fractal_operator::crd::project::FractalProject;

/// A project row for `--json`. Carries the derived namespace mapping a
/// CI script needs to drive `fractal deploy`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRow {
    name: String,
    owner: String,
    phase: String,
    namespaces: Vec<String>,
}

/// One environment of a project for `--json`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentInfo {
    name: String,
    namespace: String,
}

/// A single project's detail for `--json`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDetail {
    name: String,
    owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    namespaces: Vec<String>,
    environments: Vec<EnvironmentInfo>,
    members: usize,
}

/// Derive the environment namespaces for a project (`fractal-<name>` for
/// a single-env project, `fractal-<name>-<env>` per declared env).
fn namespaces_for(proj: &FractalProject, name: &str) -> Vec<String> {
    if proj.spec.environments.is_empty() {
        vec![format!("fractal-{name}")]
    } else {
        proj.spec
            .environments
            .iter()
            .map(|env| format!("fractal-{name}-{}", env.name))
            .collect()
    }
}

/// List all `FractalProject` resources cluster-wide. Projects live in
/// `fractal-system` but the lifecycle controller writes derived
/// namespaces (`fractal-<project>` or `fractal-<project>-<env>`) and
/// users typically want the namespace mapping for downstream CLI use.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable, the list call
/// fails, or JSON serialization fails.
pub async fn list_projects(json: bool) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");

    let projects = api
        .list(&ListParams::default())
        .await
        .context("failed to list FractalProjects")?;

    if json {
        let rows: Vec<ProjectRow> = projects
            .items
            .iter()
            .map(|proj| {
                let name = proj.metadata.name.as_deref().unwrap_or("(unknown)").to_string();
                let namespaces = namespaces_for(proj, &name);
                ProjectRow {
                    name,
                    owner: proj.spec.owner.clone(),
                    phase: proj
                        .status
                        .as_ref()
                        .map_or("Pending", |s| s.phase.as_str())
                        .to_string(),
                    namespaces,
                }
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if projects.items.is_empty() {
        println!("{}", "(no projects)".dimmed());
        return Ok(());
    }

    println!(
        "{:<24}  {:<24}  {:<14}  {}",
        "NAME".bold(),
        "OWNER".bold(),
        "PHASE".bold(),
        "NAMESPACES".bold(),
    );
    for proj in &projects.items {
        let name = proj.metadata.name.as_deref().unwrap_or("(unknown)").to_string();
        let owner = proj.spec.owner.clone();
        let phase = proj.status.as_ref().map_or("Pending", |s| s.phase.as_str());
        let namespaces = namespaces_for(proj, &name);
        let phase_colored = match phase {
            "Active" | "Running" => phase.green().to_string(),
            "PendingDeletion" => phase.yellow().to_string(),
            "Failed" => phase.red().to_string(),
            _ => phase.dimmed().to_string(),
        };
        println!(
            "{:<24}  {:<24}  {:<14}  {}",
            name.cyan(),
            owner.dimmed(),
            phase_colored,
            namespaces.join(", ").dimmed(),
        );
    }

    Ok(())
}

/// Print a single project's spec + status. Use this to discover the
/// project's environment namespaces, members, and current phase from
/// a script.
///
/// # Errors
///
/// Returns an error if the project is missing, the apiserver is
/// unreachable, or JSON serialization fails.
pub async fn get_project(name: &str, json: bool) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");

    let proj = api
        .get(name)
        .await
        .with_context(|| format!("project '{name}' not found"))?;

    if json {
        let environments: Vec<EnvironmentInfo> = proj
            .spec
            .environments
            .iter()
            .map(|env| EnvironmentInfo {
                name: env.name.clone(),
                namespace: format!("fractal-{name}-{}", env.name),
            })
            .collect();
        let detail = ProjectDetail {
            name: name.to_string(),
            owner: proj.spec.owner.clone(),
            phase: proj.status.as_ref().map(|s| s.phase.clone()),
            namespaces: namespaces_for(&proj, name),
            environments,
            members: proj.spec.members.len(),
        };
        println!("{}", serde_json::to_string_pretty(&detail)?);
        return Ok(());
    }

    println!("{} {}", "Project:".bold(), name.cyan());
    println!("  {} {}", "Owner:".bold(), proj.spec.owner);
    if let Some(phase) = proj.status.as_ref().map(|s| s.phase.as_str()) {
        println!("  {} {phase}", "Phase:".bold());
    }
    if proj.spec.environments.is_empty() {
        println!(
            "  {} {}",
            "Namespace:".bold(),
            format!("fractal-{name}").dimmed()
        );
    } else {
        println!("  {}", "Environments:".bold());
        for env in &proj.spec.environments {
            println!(
                "    - {}  (namespace: fractal-{name}-{})",
                env.name.cyan(),
                env.name.dimmed()
            );
        }
    }
    if !proj.spec.members.is_empty() {
        println!("  {} {}", "Members:".bold(), proj.spec.members.len());
    }

    Ok(())
}
