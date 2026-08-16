//! `fractal projects list | get` -- enumerate / inspect
//! `FractalProject` resources. Useful for CI scripts that need to
//! discover the namespace for a project name (e.g. to drive
//! `fractal deploy`).

use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, ListParams};

use fractal_operator::crd::project::FractalProject;

/// List all `FractalProject` resources cluster-wide. Projects live in
/// `fractal-system` but the lifecycle controller writes derived
/// namespaces (`fractal-<project>` or `fractal-<project>-<env>`) and
/// users typically want the namespace mapping for downstream CLI use.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable or the list call
/// fails.
pub async fn list_projects() -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");

    let projects = api
        .list(&ListParams::default())
        .await
        .context("failed to list FractalProjects")?;

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
    for proj in projects.items {
        let name = proj
            .metadata
            .name
            .as_deref()
            .unwrap_or("(unknown)")
            .to_string();
        let owner = proj.spec.owner.clone();
        let phase = proj.status.as_ref().map_or("Pending", |s| s.phase.as_str());
        let namespaces: Vec<String> = if proj.spec.environments.is_empty() {
            vec![format!("fractal-{name}")]
        } else {
            proj.spec
                .environments
                .iter()
                .map(|env| format!("fractal-{name}-{}", env.name))
                .collect()
        };
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
/// Returns an error if the project is missing or the apiserver is
/// unreachable.
pub async fn get_project(name: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");

    let proj = api
        .get(name)
        .await
        .with_context(|| format!("project '{name}' not found"))?;

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
