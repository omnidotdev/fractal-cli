use anyhow::{bail, Context, Result};
use colored::Colorize;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;

use fractal_operator::crd::service::{EnvVar, EnvVarSource, FractalService, SecretRef};

/// Parse a `KEY=VALUE` pair
///
/// # Errors
///
/// Returns an error if the input doesn't contain `=` or the key is empty.
pub fn parse_key_value(pair: &str) -> Result<(String, String)> {
    let (key, value) = pair
        .split_once('=')
        .context("expected KEY=VALUE format")?;

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
        let (source, value) = if let Some(v) = &env.value {
            ("literal", v.clone())
        } else if let Some(vf) = &env.value_from {
            if let Some(sr) = &vf.secret_ref {
                ("secret", format!("{}:{} (masked)", sr.name, sr.key))
            } else {
                ("unknown", "—".to_string())
            }
        } else {
            ("empty", "—".to_string())
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
pub async fn set_secret_env(
    service: &str,
    name: &str,
    secret: &str,
    key: &str,
) -> Result<()> {
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
async fn patch_env(
    api: &Api<FractalService>,
    service: &str,
    env: &[EnvVar],
) -> Result<()> {
    let patch = serde_json::json!({
        "spec": {
            "deploy": {
                "env": env
            }
        }
    });

    api.patch(
        service,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .context("failed to patch env vars")?;

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
}
