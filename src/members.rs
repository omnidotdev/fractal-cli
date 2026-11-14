use anyhow::{Context, Result};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::project::{FractalProject, ProjectMember, ProjectRole};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::api;

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

const fn format_role(role: ProjectRole) -> &'static str {
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

/// `GET /v1/projects/members` reply
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MembersReply {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    members: Vec<api::model::Member>,
    #[serde(default)]
    viewer_role: Option<String>,
}

/// Body for adding or removing a member. `namespace` + `name` address
/// the project resource, not a service namespace.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberBody<'a> {
    namespace: &'a str,
    name: &'a str,
    user_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'a str>,
}

/// Resolve the project and the namespace its resource lives in
async fn project_ref(client: &api::ApiClient, project: &str) -> Result<(String, String)> {
    let proj = api::resolve::find_project(client, project).await?;
    let ns = proj
        .metadata
        .namespace
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "fractal-system".to_string());
    Ok((proj.metadata.name, ns))
}

/// List members over the hosted API
///
/// # Errors
///
/// Returns an error if the project can't be found or you aren't a member.
pub async fn list_members_api(project: &str) -> Result<()> {
    let client = api::ApiClient::from_env()?;
    let (name, ns) = project_ref(&client, project).await?;
    let reply: MembersReply = client
        .get("/v1/projects/members", &[("namespace", ns), ("name", name)])
        .await?;
    print_members(&reply);
    Ok(())
}

fn print_members(reply: &MembersReply) {
    if let Some(owner) = reply.owner.as_deref() {
        println!("{} {}", "Owner:".bold(), owner.cyan());
    }
    if let Some(role) = reply.viewer_role.as_deref() {
        println!("{} {}", "Your role:".bold(), role);
    }
    println!();
    if reply.members.is_empty() {
        println!("{}", "(no additional members)".dimmed());
        return;
    }
    println!("{:<40}  {}", "USER".bold(), "ROLE".bold());
    for m in &reply.members {
        println!("{:<40}  {}", m.user_id.cyan(), m.role);
    }
}

/// Add or update a member over the hosted API
///
/// # Errors
///
/// Returns an error if the role is invalid, the project can't be found,
/// or you can't manage its members.
pub async fn add_member_api(project: &str, user_id: &str, role: &str) -> Result<()> {
    let role = format_role(parse_role(role)?);
    let client = api::ApiClient::from_env()?;
    let (name, ns) = project_ref(&client, project).await?;
    let _: serde_json::Value = client
        .send(
            Method::POST,
            "/v1/projects/members",
            &MemberBody {
                namespace: &ns,
                name: &name,
                user_id,
                role: Some(role),
            },
        )
        .await?;
    println!(
        "{} {} as {} on {}",
        "\u{2713}".green(),
        user_id.cyan(),
        role,
        name.cyan(),
    );
    Ok(())
}

/// Remove a member over the hosted API
///
/// # Errors
///
/// Returns an error if the project can't be found or you can't manage
/// its members.
pub async fn remove_member_api(project: &str, user_id: &str) -> Result<()> {
    let client = api::ApiClient::from_env()?;
    let (name, ns) = project_ref(&client, project).await?;
    let _: serde_json::Value = client
        .send(
            Method::DELETE,
            "/v1/projects/members",
            &MemberBody {
                namespace: &ns,
                name: &name,
                user_id,
                role: None,
            },
        )
        .await?;
    println!(
        "{} removed {} from {}",
        "\u{2713}".green(),
        user_id.cyan(),
        name.cyan(),
    );
    Ok(())
}

#[cfg(test)]
mod api_tests {
    use super::{MemberBody, MembersReply};

    #[test]
    fn decodes_members_reply() {
        let r: MembersReply = crate::api::parse_rest(
            200,
            r#"{"owner":"user_1","members":[{"userId":"u2","role":"developer"}],"viewerRole":"owner"}"#,
        )
        .unwrap();
        assert_eq!(r.owner.as_deref(), Some("user_1"));
        assert_eq!(r.members[0].user_id, "u2");
        assert_eq!(r.viewer_role.as_deref(), Some("owner"));
    }

    #[test]
    fn member_body_matches_api_contract() {
        let add = MemberBody {
            namespace: "fractal-system",
            name: "demo",
            user_id: "u2",
            role: Some("viewer"),
        };
        assert_eq!(
            serde_json::to_value(&add).unwrap(),
            serde_json::json!({ "namespace": "fractal-system", "name": "demo", "userId": "u2", "role": "viewer" })
        );
        let remove = MemberBody { role: None, ..add };
        assert!(serde_json::to_value(&remove).unwrap().get("role").is_none());
    }
}
