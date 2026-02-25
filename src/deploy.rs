use std::collections::HashMap;

use anyhow::{bail, Context};
use colored::Colorize;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;

use fractal_operator::crd::project::{FractalProject, FractalProjectSpec};
use fractal_operator::crd::service::{
    AutoscaleConfig as CrdAutoscaleConfig, BuildConfig, BuildMode, DeployConfig, EnvVar,
    EnvVarSource, ExposeConfig as CrdExposeConfig, FractalService, FractalServiceSpec,
    HealthCheckConfig as CrdHealthCheckConfig, ResourceRequirements, SecretRef, ServiceSource,
    ServiceType, VolumeConfig as CrdVolumeConfig,
};

use crate::config::{EnvValue, FractalConfig, ServiceConfig};

/// Parse `--image` overrides from the CLI.
///
/// Each override must be in the format `SERVICE=IMAGE:TAG`. If no tag
/// is provided, `latest` is used.
///
/// # Errors
///
/// Returns an error if any override is malformed.
pub fn parse_image_overrides(overrides: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();

    for entry in overrides {
        let (service, image) = entry
            .split_once('=')
            .context(format!("invalid --image format: '{entry}', expected SERVICE=IMAGE:TAG"))?;

        if service.is_empty() {
            bail!("service name cannot be empty in --image override: '{entry}'");
        }
        if image.is_empty() {
            bail!("image cannot be empty in --image override: '{entry}'");
        }

        // Default to :latest if no tag provided
        let image_with_tag = if image.contains(':') {
            image.to_string()
        } else {
            format!("{image}:latest")
        };

        map.insert(service.to_string(), image_with_tag);
    }

    Ok(map)
}

/// Deploy a `FractalConfig` to the cluster.
///
/// When `dry_run` is true, prints what would be applied without making
/// any API calls.
///
/// # Errors
///
/// Returns an error if the Kubernetes client cannot be created or if
/// any CRD apply operation fails.
pub async fn deploy(
    config: &FractalConfig,
    dry_run: bool,
    overrides: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if dry_run {
        print_dry_run(config);
        return Ok(());
    }

    let client = Client::try_default().await?;

    // Apply the FractalProject
    let project_spec = FractalProjectSpec {
        display_name: config.project.name.clone(),
        description: config.project.description.clone(),
        owner: config.project.owner.clone(),
    };

    let project = FractalProject::new(&config.project.name, project_spec);
    let project_api: Api<FractalProject> = Api::default_namespaced(client.clone());

    project_api
        .patch(
            &config.project.name,
            &PatchParams::apply("fractal-cli"),
            &Patch::Apply(project),
        )
        .await?;

    println!(
        "{} Project '{}' applied",
        "\u{2713}".green(),
        config.project.name.cyan()
    );

    // Apply each FractalService
    for svc_config in &config.services {
        let svc_spec = to_service_spec(&config.project.name, svc_config, overrides);
        let svc = FractalService::new(&svc_config.name, svc_spec);
        let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());

        svc_api
            .patch(
                &svc_config.name,
                &PatchParams::apply("fractal-cli"),
                &Patch::Apply(svc),
            )
            .await?;

        println!(
            "{} Service '{}' applied",
            "\u{2713}".green(),
            svc_config.name.cyan()
        );
    }

    println!(
        "\n{} Deployed {} service(s) to project '{}'",
        "\u{2713}".green(),
        config.services.len(),
        config.project.name
    );

    Ok(())
}

fn convert_env(config: &ServiceConfig) -> Vec<EnvVar> {
    config
        .deploy
        .env
        .iter()
        .map(|(k, v)| match v {
            EnvValue::Literal(val) => EnvVar {
                name: k.clone(),
                value: Some(val.clone()),
                value_from: None,
            },
            EnvValue::SecretRef { secret, key } => EnvVar {
                name: k.clone(),
                value: None,
                value_from: Some(EnvVarSource {
                    secret_ref: Some(SecretRef {
                        name: secret.clone(),
                        key: key.clone(),
                    }),
                }),
            },
        })
        .collect()
}

fn convert_service_type(s: &str) -> ServiceType {
    match s {
        "worker" => ServiceType::Worker,
        "cron" | "cronJob" => ServiceType::CronJob,
        "static" | "staticSite" => ServiceType::StaticSite,
        _ => ServiceType::Web,
    }
}

fn convert_deploy(config: &ServiceConfig, env: Vec<EnvVar>) -> DeployConfig {
    let resources = config.deploy.resources.as_ref().map(|r| ResourceRequirements {
        cpu: r.cpu.clone(),
        memory: r.memory.clone(),
    });

    let health_check = config.deploy.health_check.as_ref().map(|hc| CrdHealthCheckConfig {
        path: hc.path.clone(),
        port: hc.port,
        initial_delay_seconds: hc.initial_delay_seconds,
        period_seconds: hc.period_seconds,
        failure_threshold: hc.failure_threshold,
    });

    let volumes = config.deploy.volumes.as_ref().map(|vols| {
        vols.iter()
            .map(|v| CrdVolumeConfig {
                mount_path: v.mount_path.clone(),
                size: v.size.clone(),
                storage_class: v.storage_class.clone(),
            })
            .collect()
    });

    let autoscale = config.deploy.autoscale.as_ref().map(|a| CrdAutoscaleConfig {
        min_replicas: a.min_replicas,
        max_replicas: a.max_replicas,
        target_cpu_percent: a.target_cpu_percent,
    });

    DeployConfig {
        replicas: Some(config.deploy.replicas),
        env,
        resources,
        health_check,
        volumes,
        schedule: config.deploy.schedule.clone(),
        autoscale,
    }
}

/// Convert a `ServiceConfig` to a `FractalServiceSpec`.
///
/// When the service name appears in `overrides`, the source is replaced
/// with the overridden image and the build mode is set to `None`.
fn to_service_spec(
    project: &str,
    config: &ServiceConfig,
    overrides: &HashMap<String, String>,
) -> FractalServiceSpec {
    let env = convert_env(config);
    let service_type = convert_service_type(&config.service_type);
    let expose = config.expose.as_ref().map(|e| CrdExposeConfig {
        port: e.port,
        domain: e.domain.clone(),
        tls: e.tls,
    });
    let deploy = convert_deploy(config, env);

    // Check for image override from CI/CD
    if let Some(image_ref) = overrides.get(&config.name) {
        let (repository, tag) = image_ref
            .rsplit_once(':')
            .map_or((image_ref.as_str(), "latest"), |(r, t)| (r, t));

        return FractalServiceSpec {
            project: project.to_string(),
            source: ServiceSource::Image {
                repository: repository.to_string(),
                tag: tag.to_string(),
            },
            build: BuildConfig {
                mode: BuildMode::None,
                dockerfile_path: None,
            },
            deploy,
            expose,
            service_type,
        };
    }

    let source = match (&config.source.git, &config.source.image) {
        (Some(git), _) => ServiceSource::Git {
            url: git.url.clone(),
            branch: git.branch.clone(),
        },
        (None, Some(image)) => ServiceSource::Image {
            repository: image.repository.clone(),
            tag: image.tag.clone(),
        },
        (None, None) => ServiceSource::Image {
            repository: format!("{project}/{}", config.name),
            tag: "latest".to_string(),
        },
    };

    let build = config.build.as_ref().map_or(
        BuildConfig {
            mode: BuildMode::None,
            dockerfile_path: None,
        },
        |b| {
            let mode = match b.mode.as_str() {
                "dockerfile" => BuildMode::Dockerfile,
                "none" => BuildMode::None,
                _ => BuildMode::Auto,
            };
            BuildConfig {
                mode,
                dockerfile_path: b.dockerfile_path.clone(),
            }
        },
    );

    FractalServiceSpec {
        project: project.to_string(),
        source,
        build,
        deploy,
        expose,
        service_type,
    }
}

/// Print what would be deployed without making API calls.
fn print_dry_run(config: &FractalConfig) {
    println!("{}", "Dry run - would apply:".yellow());
    println!("  Project: {}", config.project.name.cyan());
    for svc in &config.services {
        println!("  Service: {} ({})", svc.name.cyan(), svc.service_type);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_image_overrides_valid() {
        let overrides = vec![
            "api=registry.example.com/api:v1.2.3".to_string(),
            "worker=myimage:latest".to_string(),
        ];
        let result = parse_image_overrides(&overrides).unwrap();
        assert_eq!(
            result.get("api").unwrap(),
            "registry.example.com/api:v1.2.3"
        );
        assert_eq!(result.get("worker").unwrap(), "myimage:latest");
    }

    #[test]
    fn test_parse_image_overrides_no_tag() {
        let overrides = vec!["api=registry.example.com/api".to_string()];
        let result = parse_image_overrides(&overrides).unwrap();
        assert_eq!(
            result.get("api").unwrap(),
            "registry.example.com/api:latest"
        );
    }

    #[test]
    fn test_parse_image_overrides_invalid() {
        let overrides = vec!["no-equals-sign".to_string()];
        assert!(parse_image_overrides(&overrides).is_err());

        let overrides = vec!["=image:tag".to_string()];
        assert!(parse_image_overrides(&overrides).is_err());

        let overrides = vec!["service=".to_string()];
        assert!(parse_image_overrides(&overrides).is_err());
    }
}
