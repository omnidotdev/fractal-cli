use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::project::{FractalProject, ProjectMember, ProjectRole};

fn parse_role(s: &str) -> Result<ProjectRole> {
    match s.to_ascii_lowercase().as_str() {
        "owner" => anyhow::bail!(
            "owner cannot be assigned through this command; transfer ownership is a separate operation"
        ),
        "admin" => Ok(ProjectRole::Admin),
        "developer" | "dev" => Ok(ProjectRole::Developer),
        "viewer" => Ok(ProjectRole::Viewer),
        other => anyhow::bail!("unknown role '{other}' (expected admin/developer/viewer)"),
    }
}

fn format_role(role: ProjectRole) -> &'static str {
    match role {
        ProjectRole::Owner => "owner",
        ProjectRole::Admin => "admin",
        ProjectRole::Developer => "developer",
        ProjectRole::Viewer => "viewer",
    }
}

/// List members of a project.
///
/// # Errors
///
/// Returns an error if the project is not found in `fractal-system`.
pub async fn list_members(project: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");
    let proj = api
        .get(project)
        .await
        .context(format!("project '{project}' not found"))?;

    println!("{} {}", "Owner:".bold(), proj.spec.owner.cyan());
    println!();

    if proj.spec.members.is_empty() {
        println!("{}", "(no additional members)".dimmed());
        return Ok(());
    }

    println!("{:<40}  {}", "USER".bold(), "ROLE".bold());
    for m in &proj.spec.members {
        println!("{:<40}  {}", m.user_id.cyan(), format_role(m.role));
    }
    Ok(())
}

/// Add or update a member's role on a project.
///
/// # Errors
///
/// Returns an error if the project is not found, the role is invalid, or
/// the patch fails.
pub async fn add_member(project: &str, user_id: &str, role: &str) -> Result<()> {
    let parsed_role = parse_role(role)?;
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");
    let proj = api
        .get(project)
        .await
        .context(format!("project '{project}' not found"))?;

    if user_id == proj.spec.owner {
        anyhow::bail!(
            "'{user_id}' is the project owner; ownership transfer is a separate operation"
        );
    }

    let mut next: Vec<ProjectMember> = proj
        .spec
        .members
        .into_iter()
        .filter(|m| m.user_id != user_id)
        .collect();
    next.push(ProjectMember {
        user_id: user_id.to_string(),
        role: parsed_role,
    });

    let patch = serde_json::json!({ "spec": { "members": next } });
    api.patch(project, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("failed to patch project")?;

    println!(
        "{} {} as {} on {}",
        "\u{2713}".green(),
        user_id.cyan(),
        format_role(parsed_role),
        project.cyan(),
    );
    Ok(())
}

/// Remove a member from a project. Owner cannot be removed this way.
///
/// # Errors
///
/// Returns an error if the project is not found or the user is the owner.
pub async fn remove_member(project: &str, user_id: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalProject> = Api::namespaced(client, "fractal-system");
    let proj = api
        .get(project)
        .await
        .context(format!("project '{project}' not found"))?;

    if user_id == proj.spec.owner {
        anyhow::bail!("cannot remove the project owner '{user_id}' through this command");
    }

    let next: Vec<ProjectMember> = proj
        .spec
        .members
        .into_iter()
        .filter(|m| m.user_id != user_id)
        .collect();

    let patch = serde_json::json!({ "spec": { "members": next } });
    api.patch(project, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("failed to patch project")?;

    println!(
        "{} removed {} from {}",
        "\u{2713}".green(),
        user_id.cyan(),
        project.cyan(),
    );
    Ok(())
}
