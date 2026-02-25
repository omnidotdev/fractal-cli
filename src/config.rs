use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A Fractal project configuration (`fractal.toml`)
#[derive(Debug, Serialize, Deserialize)]
pub struct FractalConfig {
    pub project: ProjectConfig,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

/// Project-level settings
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A service within the project
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    #[serde(default = "default_service_type")]
    pub service_type: String,
    pub source: SourceConfig,
    #[serde(default)]
    pub build: Option<BuildConfig>,
    #[serde(default)]
    pub deploy: DeploymentConfig,
    #[serde(default)]
    pub expose: Option<ExposeConfig>,
}

fn default_service_type() -> String {
    "web".to_string()
}

/// Source configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceConfig {
    pub git: Option<GitSource>,
    pub image: Option<ImageSource>,
}

/// Git repository source
#[derive(Debug, Serialize, Deserialize)]
pub struct GitSource {
    pub url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

/// Container image source
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageSource {
    pub repository: String,
    #[serde(default = "default_tag")]
    pub tag: String,
}

fn default_tag() -> String {
    "latest".to_string()
}

/// Build configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct BuildConfig {
    pub mode: String,
    pub dockerfile_path: Option<String>,
}

/// Autoscaling configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct AutoscaleConfig {
    pub min_replicas: i32,
    pub max_replicas: i32,
    pub target_cpu_percent: Option<i32>,
}

/// Deployment settings
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeploymentConfig {
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    #[serde(default)]
    pub env: HashMap<String, EnvValue>,
    #[serde(default)]
    pub resources: Option<ResourceConfig>,
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    #[serde(default)]
    pub volumes: Option<Vec<VolumeConfig>>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub autoscale: Option<AutoscaleConfig>,
}

/// Health check probe configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub path: String,
    pub port: Option<i32>,
    pub initial_delay_seconds: Option<i32>,
    pub period_seconds: Option<i32>,
    pub failure_threshold: Option<i32>,
}

/// Persistent volume configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub mount_path: String,
    pub size: String,
    pub storage_class: Option<String>,
}

const fn default_replicas() -> i32 {
    1
}

/// Environment variable value -- either a literal string or a secret reference
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum EnvValue {
    /// Literal string value
    Literal(String),
    /// Reference to a Kubernetes secret key
    SecretRef {
        secret: String,
        key: String,
    },
}

/// CPU and memory resource configuration
#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceConfig {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

/// Expose settings
#[derive(Debug, Serialize, Deserialize)]
pub struct ExposeConfig {
    pub port: i32,
    pub domain: Option<String>,
    #[serde(default)]
    pub tls: bool,
}

/// Generate a starter `fractal.toml` for a new project
#[must_use]
pub fn generate_template(name: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
owner = "team@example.com"
description = "A Fractal project"

[[services]]
name = "{name}-api"
service_type = "web"

[services.source.image]
repository = "nginx"
tag = "latest"

# [services.build]
# mode = "dockerfile"
# dockerfile_path = "Dockerfile"

[services.deploy]
replicas = 1
# env = {{ PORT = "8080", DB_PASSWORD = {{ secret = "db-creds", key = "password" }} }}

# [services.deploy.resources]
# cpu = "500m"
# memory = "256Mi"

# [services.deploy.health_check]
# path = "/healthz"
# port = 8080
# initial_delay_seconds = 5
# period_seconds = 10

# [[services.deploy.volumes]]
# mount_path = "/data"
# size = "1Gi"
# storage_class = "standard"

# schedule = "0 * * * *"  # cron schedule (for cronJob service_type)

# [services.deploy.autoscale]
# min_replicas = 2
# max_replicas = 10
# target_cpu_percent = 80

[services.expose]
port = 8080
# domain = "example.com"
# tls = true
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_template() {
        let toml_str = generate_template("my-app");
        let config: FractalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.project.name, "my-app");
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].name, "my-app-api");
    }

    #[test]
    fn test_roundtrip() {
        let config = FractalConfig {
            project: ProjectConfig {
                name: "demo".to_string(),
                owner: "user@example.com".to_string(),
                description: None,
            },
            services: vec![],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: FractalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.project.name, "demo");
    }

    #[test]
    fn test_parse_with_resources() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "api"
service_type = "web"

[services.source.image]
repository = "nginx"

[services.deploy]
replicas = 2

[services.deploy.resources]
cpu = "500m"
memory = "256Mi"

[services.expose]
port = 8080
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let resources = config.services[0].deploy.resources.as_ref().unwrap();
        assert_eq!(resources.cpu.as_deref(), Some("500m"));
        assert_eq!(resources.memory.as_deref(), Some("256Mi"));
    }

    #[test]
    fn test_parse_with_secret_env() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "api"
service_type = "web"

[services.source.image]
repository = "nginx"

[services.deploy]
replicas = 1

[services.deploy.env]
PORT = "8080"
DB_PASSWORD = { secret = "db-creds", key = "password" }

[services.expose]
port = 8080
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let env = &config.services[0].deploy.env;
        assert_eq!(env.len(), 2);
        match &env["PORT"] {
            EnvValue::Literal(v) => assert_eq!(v, "8080"),
            EnvValue::SecretRef { .. } => panic!("expected literal"),
        }
        match &env["DB_PASSWORD"] {
            EnvValue::SecretRef { secret, key } => {
                assert_eq!(secret, "db-creds");
                assert_eq!(key, "password");
            }
            EnvValue::Literal(_) => panic!("expected secret ref"),
        }
    }

    #[test]
    fn test_parse_with_build_config() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "api"
service_type = "web"

[services.source.git]
url = "https://github.com/user/app"
branch = "main"

[services.build]
mode = "dockerfile"
dockerfile_path = "Dockerfile"

[services.deploy]
replicas = 1

[services.expose]
port = 3000
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let build = config.services[0].build.as_ref().unwrap();
        assert_eq!(build.mode, "dockerfile");
        assert_eq!(build.dockerfile_path.as_deref(), Some("Dockerfile"));
    }

    #[test]
    fn test_parse_with_health_check() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "api"
service_type = "web"

[services.source.image]
repository = "nginx"

[services.deploy]
replicas = 1

[services.deploy.health_check]
path = "/healthz"
port = 8080
initial_delay_seconds = 10
period_seconds = 15
failure_threshold = 5

[services.expose]
port = 8080
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let hc = config.services[0].deploy.health_check.as_ref().unwrap();
        assert_eq!(hc.path, "/healthz");
        assert_eq!(hc.port, Some(8080));
        assert_eq!(hc.initial_delay_seconds, Some(10));
        assert_eq!(hc.period_seconds, Some(15));
        assert_eq!(hc.failure_threshold, Some(5));
    }

    #[test]
    fn test_parse_with_volumes() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "db"
service_type = "web"

[services.source.image]
repository = "postgres"
tag = "16"

[services.deploy]
replicas = 1

[[services.deploy.volumes]]
mount_path = "/var/lib/postgresql/data"
size = "10Gi"
storage_class = "fast-ssd"

[[services.deploy.volumes]]
mount_path = "/backups"
size = "5Gi"
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let volumes = config.services[0].deploy.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].mount_path, "/var/lib/postgresql/data");
        assert_eq!(volumes[0].size, "10Gi");
        assert_eq!(volumes[0].storage_class.as_deref(), Some("fast-ssd"));
        assert_eq!(volumes[1].mount_path, "/backups");
        assert!(volumes[1].storage_class.is_none());
    }

    #[test]
    fn test_parse_with_autoscale() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "api"
service_type = "web"

[services.source.image]
repository = "nginx"

[services.deploy]
replicas = 2

[services.deploy.autoscale]
min_replicas = 2
max_replicas = 10
target_cpu_percent = 70

[services.expose]
port = 8080
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        let autoscale = config.services[0].deploy.autoscale.as_ref().unwrap();
        assert_eq!(autoscale.min_replicas, 2);
        assert_eq!(autoscale.max_replicas, 10);
        assert_eq!(autoscale.target_cpu_percent, Some(70));
    }

    #[test]
    fn test_parse_with_schedule() {
        let toml_str = r#"
[project]
name = "test"
owner = "user@example.com"

[[services]]
name = "cleanup"
service_type = "cron"

[services.source.image]
repository = "busybox"

[services.deploy]
replicas = 1
schedule = "*/5 * * * *"
"#;
        let config: FractalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.services[0].deploy.schedule.as_deref(),
            Some("*/5 * * * *")
        );
    }
}
