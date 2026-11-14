use anyhow::{Context, Result, bail};
use colored::Colorize;
use kube::Client;
use kube::api::{Api, Patch, PatchParams};

use fractal_operator::crd::service::{EnvVar, EnvVarSource, FractalService, SecretRef};

use crate::api;
use crate::api::model;
use crate::api::resolve::Located;

/// Parse a `KEY=VALUE` pair
///
/// # Errors
///
/// Returns an error if the input doesn't contain `=` or the key is empty.
pub fn parse_key_value(pair: &str) -> Result<(String, String)> {
    let (key, value) = pair.split_once('=').context("expected KEY=VALUE format")?;

    if key.is_empty() {
        bail!("key cannot be empty");
    }

    Ok((key.to_string(), value.to_string()))
}

/// Insert or replace an env var in a list, returning the new list
#[must_use]
pub fn upsert_env(envs: &[EnvVar], new: EnvVar) -> Vec<EnvVar> {
    let mut result: Vec<EnvVar> = envs
        .iter()
        .filter(|e| e.name != new.name)
        .cloned()
        .collect();
    result.push(new);
    result
}

/// Remove an env var by name, returning the new list
#[must_use]
pub fn remove_env(envs: &[EnvVar], name: &str) -> Vec<EnvVar> {
    envs.iter().filter(|e| e.name != name).cloned().collect()
}

/// List env vars for a service
pub async fn list_env(service: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    let svc = api
        .get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let envs = &svc.spec.deploy.env;

    if envs.is_empty() {
        println!("No environment variables set for '{}'", service.cyan());
        return Ok(());
    }

    println!(
        "{:<20} {:<12} {}",
        "NAME".bold(),
        "SOURCE".bold(),
        "VALUE".bold()
    );

    for env in envs {
        let (source, value) = match (&env.value, &env.value_from) {
            (Some(v), _) => ("literal", v.clone()),
            (None, Some(vf)) => vf.secret_ref.as_ref().map_or_else(
                || ("unknown", "—".to_string()),
                |sr| ("secret", format!("{}:{} (masked)", sr.name, sr.key)),
            ),
            (None, None) => ("empty", "—".to_string()),
        };

        println!("{:<20} {:<12} {}", env.name, source, value);
    }

    Ok(())
}

/// Set a literal env var on a service
pub async fn set_env(service: &str, pair: &str) -> Result<()> {
    let (key, value) = parse_key_value(pair)?;
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    let svc = api
        .get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let new_env = EnvVar {
        name: key.clone(),
        value: Some(value),
        value_from: None,
    };

    let updated = upsert_env(&svc.spec.deploy.env, new_env);
    patch_env(&api, service, &updated).await?;

    println!(
        "{} Set '{}' on '{}'",
        "\u{2713}".green(),
        key.cyan(),
        service.cyan()
    );

    Ok(())
}

/// Set a secret-ref env var on a service
pub async fn set_secret_env(service: &str, name: &str, secret: &str, key: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    let svc = api
        .get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let new_env = EnvVar {
        name: name.to_string(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_ref: Some(SecretRef {
                name: secret.to_string(),
                key: key.to_string(),
            }),
            ..Default::default()
        }),
    };

    let updated = upsert_env(&svc.spec.deploy.env, new_env);
    patch_env(&api, service, &updated).await?;

    println!(
        "{} Set '{}' (secret ref) on '{}'",
        "\u{2713}".green(),
        name.cyan(),
        service.cyan()
    );

    Ok(())
}

/// Unset an env var on a service
pub async fn unset_env(service: &str, name: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalService> = Api::default_namespaced(client);
    let svc = api
        .get(service)
        .await
        .context(format!("service '{service}' not found"))?;

    let updated = remove_env(&svc.spec.deploy.env, name);
    patch_env(&api, service, &updated).await?;

    println!(
        "{} Unset '{}' on '{}'",
        "\u{2713}".green(),
        name.cyan(),
        service.cyan()
    );

    Ok(())
}

/// Patch the env list on a `FractalService`
async fn patch_env(api: &Api<FractalService>, service: &str, env: &[EnvVar]) -> Result<()> {
    let patch = serde_json::json!({
        "spec": {
            "deploy": {
                "env": env
            }
        }
    });

    api.patch(service, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("failed to patch env vars")?;

    Ok(())
}

/// Merge `new` into the env read from the API, replacing a same-named
/// entry. Existing entries are normalized into valid mutation inputs, so
/// referenced secrets round-trip as references, never plaintext.
fn upsert_api_env(existing: &[model::EnvVar], new: model::EnvVar) -> Vec<model::EnvVar> {
    let mut out: Vec<model::EnvVar> = existing
        .iter()
        .filter(|e| e.name != new.name)
        .map(model::EnvVar::to_input)
        .collect();
    out.push(new);
    out
}

/// Remove `name` from the env read from the API. `None` when it isn't set.
fn remove_api_env(existing: &[model::EnvVar], name: &str) -> Option<Vec<model::EnvVar>> {
    if !existing.iter().any(|e| e.name == name) {
        return None;
    }
    Some(
        existing
            .iter()
            .filter(|e| e.name != name)
            .map(model::EnvVar::to_input)
            .collect(),
    )
}

/// `updateFractalService` input that replaces the service's env list
fn env_update_input(project: &str, service: &str, env: &[model::EnvVar]) -> serde_json::Value {
    serde_json::json!({
        "input": { "project": project, "name": service, "deploy": { "env": env } }
    })
}

async fn locate(project: Option<&str>, service: &str) -> Result<(api::ApiClient, Located)> {
    let client = api::ApiClient::from_env()?;
    let located = api::resolve::locate_service(&client, project, service).await?;
    Ok((client, located))
}

async fn write_api_env(client: &api::ApiClient, at: &Located, env: &[model::EnvVar]) -> Result<()> {
    let _: serde_json::Value = client
        .graphql(
            &model::update_service_mutation(),
            env_update_input(&at.project, at.service.name(), env),
        )
        .await?;
    Ok(())
}

/// List env vars over the hosted API. Referenced secrets are masked.
///
/// # Errors
///
/// Returns an error if the service can't be found or the API call fails.
pub async fn list_env_api(project: Option<&str>, service: &str) -> Result<()> {
    let (_, at) = locate(project, service).await?;
    let envs = at.service.env();
    if envs.is_empty() {
        println!("No environment variables set for '{}'", service.cyan());
        return Ok(());
    }
    println!(
        "{:<20} {:<12} {}",
        "NAME".bold(),
        "SOURCE".bold(),
        "VALUE".bold()
    );
    for env in envs {
        let (source, value) = env.display();
        println!("{:<20} {:<12} {}", env.name, source, value);
    }
    Ok(())
}

/// Set an env var over the hosted API. With `secret`, the value is sent
/// write-only and stored as a managed secret reference.
///
/// # Errors
///
/// Returns an error if the pair is malformed, the service can't be
/// found, or the update fails.
pub async fn set_env_api(
    project: Option<&str>,
    service: &str,
    pair: &str,
    secret: bool,
) -> Result<()> {
    let (key, value) = parse_key_value(pair)?;
    let (client, at) = locate(project, service).await?;
    let new = if secret {
        model::EnvVar {
            name: key.clone(),
            secret: Some(value),
            ..model::EnvVar::default()
        }
    } else {
        model::EnvVar {
            name: key.clone(),
            value: Some(value),
            ..model::EnvVar::default()
        }
    };
    let updated = upsert_api_env(at.service.env(), new);
    write_api_env(&client, &at, &updated).await?;
    println!(
        "{} Set '{}'{} on '{}'",
        "\u{2713}".green(),
        key.cyan(),
        if secret { " (secret)" } else { "" },
        service.cyan()
    );
    Ok(())
}

/// Reference an existing secret from an env var over the hosted API
///
/// # Errors
///
/// Returns an error if the service can't be found or the update fails.
pub async fn set_secret_env_api(
    project: Option<&str>,
    service: &str,
    name: &str,
    secret: &str,
    key: &str,
) -> Result<()> {
    let (client, at) = locate(project, service).await?;
    let new = model::EnvVar {
        name: name.to_string(),
        value_from: Some(model::EnvVarSource {
            secret_ref: Some(model::SecretRef {
                name: secret.to_string(),
                key: key.to_string(),
            }),
            object_storage_ref: None,
        }),
        ..model::EnvVar::default()
    };
    let updated = upsert_api_env(at.service.env(), new);
    write_api_env(&client, &at, &updated).await?;
    println!(
        "{} Set '{}' (secret ref) on '{}'",
        "\u{2713}".green(),
        name.cyan(),
        service.cyan()
    );
    Ok(())
}

/// Remove an env var over the hosted API
///
/// # Errors
///
/// Returns an error if the service can't be found or the update fails.
pub async fn unset_env_api(project: Option<&str>, service: &str, name: &str) -> Result<()> {
    let (client, at) = locate(project, service).await?;
    let Some(updated) = remove_api_env(at.service.env(), name) else {
        println!("'{}' is not set on '{}'", name.cyan(), service.cyan());
        return Ok(());
    };
    write_api_env(&client, &at, &updated).await?;
    println!(
        "{} Unset '{}' on '{}'",
        "\u{2713}".green(),
        name.cyan(),
        service.cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_value() {
        let (k, v) = parse_key_value("PORT=8080").unwrap();
        assert_eq!(k, "PORT");
        assert_eq!(v, "8080");

        let (k, v) = parse_key_value("URL=https://example.com/path?q=1").unwrap();
        assert_eq!(k, "URL");
        assert_eq!(v, "https://example.com/path?q=1");

        assert!(parse_key_value("NOEQUALS").is_err());
        assert!(parse_key_value("=value").is_err());
    }

    #[test]
    fn test_upsert_env_replaces_existing() {
        let envs = vec![
            EnvVar {
                name: "PORT".to_string(),
                value: Some("8080".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "HOST".to_string(),
                value: Some("localhost".to_string()),
                value_from: None,
            },
        ];

        let new = EnvVar {
            name: "PORT".to_string(),
            value: Some("3000".to_string()),
            value_from: None,
        };

        let result = upsert_env(&envs, new);
        assert_eq!(result.len(), 2);
        let port = result.iter().find(|e| e.name == "PORT").unwrap();
        assert_eq!(port.value.as_deref(), Some("3000"));
    }

    #[test]
    fn test_upsert_env_adds_new() {
        let envs = vec![EnvVar {
            name: "PORT".to_string(),
            value: Some("8080".to_string()),
            value_from: None,
        }];

        let new = EnvVar {
            name: "HOST".to_string(),
            value: Some("0.0.0.0".to_string()),
            value_from: None,
        };

        let result = upsert_env(&envs, new);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_remove_env() {
        let envs = vec![
            EnvVar {
                name: "PORT".to_string(),
                value: Some("8080".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "HOST".to_string(),
                value: Some("localhost".to_string()),
                value_from: None,
            },
        ];

        let result = remove_env(&envs, "PORT");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "HOST");

        // Removing non-existent key is a no-op
        let result = remove_env(&envs, "NONEXISTENT");
        assert_eq!(result.len(), 2);
    }

    fn api_env() -> Vec<model::EnvVar> {
        let d: model::ServicesData =
            crate::api::parse_graphql(200, model::fixtures::SERVICES_JSON).unwrap();
        d.fractal_services[0].env().to_vec()
    }

    #[test]
    fn api_upsert_replaces_and_keeps_references() {
        let new = model::EnvVar {
            name: "PORT".to_string(),
            value: Some("8080".to_string()),
            ..model::EnvVar::default()
        };
        let out = upsert_api_env(&api_env(), new);
        let input = env_update_input("demo", "web", &out);
        assert_eq!(input["input"]["project"], "demo");
        assert_eq!(input["input"]["name"], "web");
        let env = input["input"]["deploy"]["env"].as_array().unwrap();
        assert_eq!(env.len(), 3);
        assert_eq!(
            env[0],
            serde_json::json!({ "name": "DB_PASSWORD", "valueFrom": { "secretRef": { "name": "web-env", "key": "DB_PASSWORD" } } })
        );
        assert_eq!(
            env[2],
            serde_json::json!({ "name": "PORT", "value": "8080" })
        );
    }

    #[test]
    fn api_upsert_sends_secret_write_only() {
        let new = model::EnvVar {
            name: "API_KEY".to_string(),
            secret: Some("s3cr3t".to_string()),
            ..model::EnvVar::default()
        };
        let out = upsert_api_env(&api_env(), new);
        let last = serde_json::to_value(out.last().unwrap()).unwrap();
        assert_eq!(
            last,
            serde_json::json!({ "name": "API_KEY", "secret": "s3cr3t" })
        );
    }

    #[test]
    fn api_remove_drops_only_named_var() {
        let out = remove_api_env(&api_env(), "PORT").unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.name != "PORT"));
        assert!(remove_api_env(&api_env(), "MISSING").is_none());
    }
}
