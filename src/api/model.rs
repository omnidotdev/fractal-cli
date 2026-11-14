//! GraphQL documents and response shapes for the Fractal API. The
//! selections mirror the ones the Fractal MCP server sends in
//! production, so the field set is known to resolve.

use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize `null` as the type's default (an empty list, usually)
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

const SERVICE_FIELDS: &str = r"
  metadata { name namespace creationTimestamp }
  spec {
    project
    serviceType
    displayName
    environment
    source {
      __typename
      ... on FractalServiceSourceGit { url branch }
      ... on FractalServiceSourceImage { repository tag }
    }
    build { mode dockerfilePath rootDir watchPaths }
    deploy {
      replicas
      schedule
      env {
        name
        value
        valueFrom {
          secretRef { name key }
          objectStorageRef { name output }
        }
      }
    }
  }
  status {
    phase
    phaseMessage
    readyReplicas
    desiredReplicas
    currentImage
    url
    lastError
  }
";

const PROJECT_FIELDS: &str = r"
  metadata { name namespace }
  spec {
    displayName
    slug
    owner
    organizationId
    environments { name }
    members { userId role }
  }
  status { phase namespace serviceCount hostname }
";

/// `fractalProjects`
pub fn list_projects_query() -> String {
    format!("query ListProjects {{ fractalProjects {{ {PROJECT_FIELDS} }} }}")
}

/// `fractalProject(name)`
pub fn get_project_query() -> String {
    format!(
        "query GetProject($name: String!) {{ fractalProject(name: $name) {{ {PROJECT_FIELDS} }} }}"
    )
}

/// `fractalServices(project)`
pub fn list_services_query() -> String {
    format!(
        "query ListServices($project: String!) {{ fractalServices(project: $project) {{ {SERVICE_FIELDS} }} }}"
    )
}

/// `fractalService(project, name)`
pub fn get_service_query() -> String {
    format!(
        "query GetService($project: String!, $name: String!) {{ fractalService(project: $project, name: $name) {{ {SERVICE_FIELDS} }} }}"
    )
}

/// `updateFractalService(input)`
pub fn update_service_mutation() -> String {
    format!(
        "mutation UpdateService($input: UpdateFractalServiceInput!) {{ updateFractalService(input: $input) {{ {SERVICE_FIELDS} }} }}"
    )
}

const TOKEN_FIELDS: &str =
    "id name subject scopes organizations issuedAt expiresAt revokedAt lastUsedAt usageCount";

/// `fractalApiTokens(organizationId)`
pub fn list_tokens_query() -> String {
    format!(
        "query ListApiTokens($organizationId: String!) {{ fractalApiTokens(organizationId: $organizationId) {{ {TOKEN_FIELDS} }} }}"
    )
}

/// `createFractalApiToken(input)`
pub fn create_token_mutation() -> String {
    format!(
        "mutation CreateApiToken($input: CreateFractalApiTokenInput!) {{ createFractalApiToken(input: $input) {{ token info {{ {TOKEN_FIELDS} }} }} }}"
    )
}

/// `revokeFractalApiToken(input)`
pub fn revoke_token_mutation() -> String {
    format!(
        "mutation RevokeApiToken($input: RevokeFractalApiTokenInput!) {{ revokeFractalApiToken(input: $input) {{ {TOKEN_FIELDS} }} }}"
    )
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub metadata: Meta,
    #[serde(default)]
    pub spec: ProjectSpec,
    #[serde(default)]
    pub status: Option<ProjectStatus>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSpec {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub owner: String,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub environments: Vec<Environment>,
    #[serde(default, deserialize_with = "null_default")]
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Environment {
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub user_id: String,
    pub role: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

impl Project {
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    pub fn phase(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .filter(|p| !p.is_empty())
    }

    /// The project's primary namespace: the one the control plane
    /// reports, else the `fractal-<name>` convention
    pub fn primary_namespace(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.namespace.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("fractal-{}", self.metadata.name))
    }

    /// Environment namespaces, derived the same way as the kube
    /// transport (`fractal-<name>` or `fractal-<name>-<env>`)
    pub fn namespaces(&self) -> Vec<String> {
        if self.spec.environments.is_empty() {
            vec![self.primary_namespace()]
        } else {
            self.spec
                .environments
                .iter()
                .map(|env| format!("fractal-{}-{}", self.metadata.name, env.name))
                .collect()
        }
    }

    /// True when `ident` names this project by name or slug
    pub fn matches(&self, ident: &str) -> bool {
        self.metadata.name == ident || self.spec.slug.as_deref() == Some(ident)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub metadata: Meta,
    #[serde(default)]
    pub spec: ServiceSpec,
    #[serde(default)]
    pub status: Option<ServiceStatus>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    #[serde(default, deserialize_with = "null_default")]
    pub service_type: String,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub build: Option<Build>,
    #[serde(default)]
    pub deploy: Option<Deploy>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    #[serde(default)]
    pub root_dir: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub watch_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deploy {
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub env: Vec<EnvVar>,
}

/// An env var as read from, and written back to, the API. Plaintext
/// secrets are never returned by reads, so a referenced value round-trips
/// as its `valueFrom` reference.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<EnvVarSource>,
    /// Write-only plaintext the API moves into a managed secret, storing
    /// only a reference in the service spec. Never returned by reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_storage_ref: Option<ObjectStorageRef>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SecretRef {
    pub name: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ObjectStorageRef {
    pub name: String,
    pub output: String,
}

impl EnvVar {
    /// Normalize a read value into a valid mutation input: a reference
    /// wins over a value, and a plain var always carries a value
    pub fn to_input(&self) -> Self {
        match &self.value_from {
            Some(src) if src.secret_ref.is_some() || src.object_storage_ref.is_some() => Self {
                name: self.name.clone(),
                value_from: Some(src.clone()),
                ..Self::default()
            },
            _ => Self {
                name: self.name.clone(),
                value: Some(self.value.clone().unwrap_or_default()),
                ..Self::default()
            },
        }
    }

    /// `(source, display value)` for `env list`. Referenced values are
    /// masked; only the reference is shown.
    pub fn display(&self) -> (&'static str, String) {
        let src = self.value_from.as_ref();
        if let Some(r) = src.and_then(|s| s.secret_ref.as_ref()) {
            return ("secret", format!("{}:{} (masked)", r.name, r.key));
        }
        if let Some(r) = src.and_then(|s| s.object_storage_ref.as_ref()) {
            return ("storage", format!("{}:{} (masked)", r.name, r.output));
        }
        self.value
            .as_ref()
            .map_or_else(|| ("empty", "-".to_string()), |v| ("literal", v.clone()))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub ready_replicas: Option<i32>,
    #[serde(default)]
    pub desired_replicas: Option<i32>,
    #[serde(default)]
    pub current_image: Option<String>,
}

impl Service {
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    pub fn namespace(&self) -> Option<&str> {
        self.metadata.namespace.as_deref().filter(|n| !n.is_empty())
    }

    pub fn env(&self) -> &[EnvVar] {
        self.spec.deploy.as_ref().map_or(&[], |d| d.env.as_slice())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub subject: String,
    #[serde(default, deserialize_with = "null_default")]
    pub scopes: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub issued_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub revoked_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedToken {
    pub token: String,
    pub info: TokenInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectsData {
    #[serde(deserialize_with = "null_default")]
    pub fractal_projects: Vec<Project>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectData {
    pub fractal_project: Option<Project>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicesData {
    #[serde(deserialize_with = "null_default")]
    pub fractal_services: Vec<Service>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceData {
    pub fractal_service: Option<Service>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokensData {
    #[serde(deserialize_with = "null_default")]
    pub fractal_api_tokens: Vec<TokenInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTokenData {
    pub create_fractal_api_token: CreatedToken,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeTokenData {
    pub revoke_fractal_api_token: TokenInfo,
}

#[cfg(test)]
pub mod fixtures {
    /// Canned `fractalServices` reply in the shape production returns
    pub const SERVICES_JSON: &str = r#"{"data":{"fractalServices":[
      {"metadata":{"name":"web","namespace":"fractal-demo","creationTimestamp":"2026-11-01T00:00:00Z"},
       "spec":{"project":"demo","serviceType":"web","displayName":null,"environment":null,
         "source":{"__typename":"FractalServiceSourceGit","url":"https://github.com/acme/app","branch":"master"},
         "build":{"mode":"auto","dockerfilePath":null,"rootDir":"apps/web","watchPaths":["apps/web","packages/ui"]},
         "deploy":{"replicas":2,"schedule":null,"env":[
           {"name":"PORT","value":"3000","valueFrom":null},
           {"name":"DB_PASSWORD","value":null,"valueFrom":{"secretRef":{"name":"web-env","key":"DB_PASSWORD"},"objectStorageRef":null}},
           {"name":"S3_KEY","value":null,"valueFrom":{"secretRef":null,"objectStorageRef":{"name":"media","output":"accessKeyId"}}}
         ]}},
       "status":{"phase":"Running","phaseMessage":null,"readyReplicas":2,"desiredReplicas":2,
         "currentImage":"reg/demo-web:sha-abc1234","url":"https://web.demo.test","lastError":null}},
      {"metadata":{"name":"nightly","namespace":"fractal-demo"},
       "spec":{"project":"demo","serviceType":"cronJob",
         "source":{"__typename":"FractalServiceSourceImage","repository":"reg/nightly","tag":"v1"},
         "build":{"mode":"none","rootDir":null,"watchPaths":null},
         "deploy":{"replicas":null,"schedule":"0 3 * * *","env":[]}},
       "status":null}
    ]}}"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::SERVICES_JSON;
    use super::*;
    use crate::api::parse_graphql;

    #[test]
    fn decodes_services() {
        let d: ServicesData = parse_graphql(200, SERVICES_JSON).unwrap();
        assert_eq!(d.fractal_services.len(), 2);
        let web = &d.fractal_services[0];
        assert_eq!(web.name(), "web");
        assert_eq!(web.namespace(), Some("fractal-demo"));
        assert_eq!(web.spec.service_type, "web");
        assert_eq!(
            web.spec.build.as_ref().unwrap().root_dir.as_deref(),
            Some("apps/web")
        );
        assert_eq!(web.env().len(), 3);
        let cron = &d.fractal_services[1];
        assert!(cron.status.is_none());
        assert!(cron.spec.build.as_ref().unwrap().watch_paths.is_empty());
        assert_eq!(
            cron.spec.deploy.as_ref().unwrap().schedule.as_deref(),
            Some("0 3 * * *")
        );
    }

    #[test]
    fn env_round_trips_references_without_plaintext() {
        let d: ServicesData = parse_graphql(200, SERVICES_JSON).unwrap();
        let inputs: Vec<EnvVar> = d.fractal_services[0]
            .env()
            .iter()
            .map(EnvVar::to_input)
            .collect();
        let v = serde_json::to_value(&inputs).unwrap();
        assert_eq!(v[0], serde_json::json!({ "name": "PORT", "value": "3000" }));
        assert_eq!(
            v[1],
            serde_json::json!({ "name": "DB_PASSWORD", "valueFrom": { "secretRef": { "name": "web-env", "key": "DB_PASSWORD" } } })
        );
        assert_eq!(
            v[2],
            serde_json::json!({ "name": "S3_KEY", "valueFrom": { "objectStorageRef": { "name": "media", "output": "accessKeyId" } } })
        );
    }

    #[test]
    fn env_display_masks_references() {
        let d: ServicesData = parse_graphql(200, SERVICES_JSON).unwrap();
        let env = d.fractal_services[0].env();
        assert_eq!(env[0].display(), ("literal", "3000".to_string()));
        assert_eq!(
            env[1].display(),
            ("secret", "web-env:DB_PASSWORD (masked)".to_string())
        );
        assert_eq!(
            env[2].display(),
            ("storage", "media:accessKeyId (masked)".to_string())
        );
    }

    #[test]
    fn decodes_projects_and_derives_namespaces() {
        let body = r#"{"data":{"fractalProjects":[
          {"metadata":{"name":"demo","namespace":"fractal-system"},
           "spec":{"displayName":"Demo","slug":"demo-app","owner":"user_1","organizationId":"org_1",
             "environments":[{"name":"production"},{"name":"staging"}],"members":[{"userId":"u2","role":"developer"}]},
           "status":{"phase":"Active","namespace":"fractal-demo","serviceCount":2,"hostname":null}},
          {"metadata":{"name":"p_x7k2","namespace":"fractal-system"},
           "spec":{"displayName":"Opaque","slug":null,"owner":"user_1","organizationId":null,"environments":[],"members":null},
           "status":{"phase":null,"namespace":"fractal-opaque-ns"}}
        ]}}"#;
        let d: ProjectsData = parse_graphql(200, body).unwrap();
        let demo = &d.fractal_projects[0];
        assert_eq!(
            demo.namespaces(),
            vec!["fractal-demo-production", "fractal-demo-staging"]
        );
        assert!(demo.matches("demo"));
        assert!(demo.matches("demo-app"));
        assert!(!demo.matches("other"));
        assert_eq!(demo.spec.members.len(), 1);
        let opaque = &d.fractal_projects[1];
        assert_eq!(opaque.namespaces(), vec!["fractal-opaque-ns"]);
        assert_eq!(opaque.phase(), None);
        assert!(opaque.spec.members.is_empty());
    }

    #[test]
    fn primary_namespace_falls_back_to_convention() {
        let p = Project {
            metadata: Meta {
                name: "demo".to_string(),
                namespace: None,
            },
            ..Project::default()
        };
        assert_eq!(p.primary_namespace(), "fractal-demo");
    }

    #[test]
    fn decodes_token_payloads() {
        let list: TokensData = parse_graphql(
            200,
            r#"{"data":{"fractalApiTokens":[{"id":"tok_1","name":"ci","subject":"user_1","scopes":["*"],
              "organizations":["org_1"],"issuedAt":"2026-11-01T00:00:00Z","expiresAt":null,"revokedAt":null,
              "lastUsedAt":"2026-11-10T00:00:00Z","usageCount":4}]}}"#,
        )
        .unwrap();
        assert_eq!(list.fractal_api_tokens[0].id, "tok_1");
        assert_eq!(list.fractal_api_tokens[0].scopes, vec!["*"]);
        let created: CreateTokenData = parse_graphql(
            200,
            r#"{"data":{"createFractalApiToken":{"token":"ftk_secret","info":{"id":"tok_2","name":"deploy",
              "subject":"user_1","scopes":["demo"],"organizations":["org_1"],"issuedAt":"2026-11-13T00:00:00Z",
              "expiresAt":"2026-12-13T00:00:00Z","revokedAt":null,"lastUsedAt":null,"usageCount":0}}}}"#,
        )
        .unwrap();
        assert_eq!(created.create_fractal_api_token.token, "ftk_secret");
        assert_eq!(
            created.create_fractal_api_token.info.expires_at.as_deref(),
            Some("2026-12-13T00:00:00Z")
        );
        let revoked: RevokeTokenData = parse_graphql(
            200,
            r#"{"data":{"revokeFractalApiToken":{"id":"tok_1","name":"ci","subject":"user_1","scopes":[],
              "organizations":[],"issuedAt":"2026-11-01T00:00:00Z","revokedAt":"2026-11-13T00:00:00Z","usageCount":4}}}"#,
        )
        .unwrap();
        assert!(revoked.revoke_fractal_api_token.revoked_at.is_some());
    }

    #[test]
    fn documents_reference_expected_operations() {
        assert!(list_services_query().contains("fractalServices(project: $project)"));
        assert!(list_services_query().contains("rootDir watchPaths"));
        assert!(get_service_query().contains("fractalService(project: $project, name: $name)"));
        assert!(update_service_mutation().contains("updateFractalService(input: $input)"));
        assert!(list_projects_query().contains("fractalProjects"));
        assert!(get_project_query().contains("fractalProject(name: $name)"));
        assert!(list_tokens_query().contains("fractalApiTokens(organizationId: $organizationId)"));
        assert!(create_token_mutation().contains("createFractalApiToken(input: $input)"));
        assert!(revoke_token_mutation().contains("revokeFractalApiToken(input: $input)"));
    }
}
