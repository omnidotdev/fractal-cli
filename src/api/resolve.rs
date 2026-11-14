//! Look up projects and services over the API. Hosted commands address a
//! service by name; the project comes from `--project` /
//! `FRACTAL_PROJECT` when given, otherwise from a search across every
//! project the caller can see.

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::ApiClient;
use super::model::{
    Project, ProjectData, ProjectsData, Service, ServiceData, ServicesData, get_project_query,
    get_service_query, list_projects_query, list_services_query,
};

/// The project hint from `--project`, else `FRACTAL_PROJECT`
pub fn project_hint(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| std::env::var("FRACTAL_PROJECT").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Every project the caller can see
///
/// # Errors
///
/// Returns an error when the API call fails.
pub async fn list_projects(client: &ApiClient) -> Result<Vec<Project>> {
    let data: ProjectsData = client.graphql(&list_projects_query(), json!({})).await?;
    Ok(data.fractal_projects)
}

/// Find a project by name, falling back to a slug match
///
/// # Errors
///
/// Returns an error when no visible project matches.
pub async fn find_project(client: &ApiClient, ident: &str) -> Result<Project> {
    let data: ProjectData = client
        .graphql(&get_project_query(), json!({ "name": ident }))
        .await?;
    if let Some(p) = data.fractal_project {
        return Ok(p);
    }
    list_projects(client)
        .await?
        .into_iter()
        .find(|p| p.matches(ident))
        .with_context(|| format!("project '{ident}' not found (run `fractal projects list`)"))
}

/// Every service in one project
///
/// # Errors
///
/// Returns an error when the API call fails.
pub async fn list_services(client: &ApiClient, project: &str) -> Result<Vec<Service>> {
    let data: ServicesData = client
        .graphql(&list_services_query(), json!({ "project": project }))
        .await?;
    Ok(data.fractal_services)
}

/// A service plus the project it belongs to
#[derive(Debug)]
pub struct Located {
    pub project: String,
    pub service: Service,
}

impl Located {
    /// The namespace the service runs in
    pub fn namespace(&self) -> String {
        self.service
            .namespace()
            .map_or_else(|| format!("fractal-{}", self.project), str::to_string)
    }
}

/// Choose the single match for `name` across projects, or explain why
/// the choice is ambiguous or empty
///
/// # Errors
///
/// Returns an error for zero or multiple matches.
pub fn pick_match(name: &str, mut matches: Vec<Located>) -> Result<Located> {
    match matches.len() {
        0 => bail!("service '{name}' not found in any project you can access"),
        1 => Ok(matches.remove(0)),
        _ => {
            let projects: Vec<&str> = matches.iter().map(|m| m.project.as_str()).collect();
            bail!(
                "service '{name}' exists in more than one project ({}); pass --project",
                projects.join(", ")
            )
        }
    }
}

/// Locate `name`, in `project` when given, else by searching every
/// visible project
///
/// # Errors
///
/// Returns an error when the service can't be found or is ambiguous.
pub async fn locate_service(
    client: &ApiClient,
    project: Option<&str>,
    name: &str,
) -> Result<Located> {
    if let Some(ident) = project {
        let proj = find_project(client, ident).await?;
        let pname = proj.name().to_string();
        let data: ServiceData = client
            .graphql(
                &get_service_query(),
                json!({ "project": pname, "name": name }),
            )
            .await?;
        let service = data
            .fractal_service
            .with_context(|| format!("service '{name}' not found in project '{pname}'"))?;
        return Ok(Located {
            project: pname,
            service,
        });
    }

    let mut matches = Vec::new();
    for proj in list_projects(client).await? {
        let pname = proj.name().to_string();
        for service in list_services(client, &pname).await? {
            if service.name() == name {
                matches.push(Located {
                    project: pname.clone(),
                    service,
                });
            }
        }
    }
    pick_match(name, matches)
}

#[cfg(test)]
mod tests {
    use super::{Located, pick_match};
    use crate::api::model::{Meta, Service};

    fn located(project: &str, ns: Option<&str>) -> Located {
        Located {
            project: project.to_string(),
            service: Service {
                metadata: Meta {
                    name: "api".to_string(),
                    namespace: ns.map(str::to_string),
                },
                ..Service::default()
            },
        }
    }

    #[test]
    fn single_match_wins() {
        let m = pick_match("api", vec![located("demo", Some("fractal-demo-prod"))]).unwrap();
        assert_eq!(m.project, "demo");
        assert_eq!(m.namespace(), "fractal-demo-prod");
    }

    #[test]
    fn missing_namespace_uses_convention() {
        assert_eq!(located("demo", None).namespace(), "fractal-demo");
    }

    #[test]
    fn no_match_is_not_found() {
        let err = pick_match("api", vec![]).unwrap_err().to_string();
        assert_eq!(err, "service 'api' not found in any project you can access");
    }

    #[test]
    fn ambiguous_match_asks_for_project() {
        let err = pick_match("api", vec![located("a", None), located("b", None)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("(a, b)"));
        assert!(err.contains("--project"));
    }
}
