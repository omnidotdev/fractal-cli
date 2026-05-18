use std::path::Path;

use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::Api;

use fractal_operator::crd::service::FractalService;

/// Sync a service's environment variables to a local `.env` file.
///
/// Resolves the FractalService's env spec, follows any `secretRef`
/// indirections to the backing K8s Secrets, and writes a flat
/// `KEY=value` file at `output_path`. Existing values in that file are
/// preserved if `--overwrite` is not set.
///
/// # Errors
///
/// Returns an error if the service or any referenced secret cannot be
/// read, or if the output file cannot be written.
pub async fn sync_secrets(service: &str, output_path: &Path, overwrite: bool) -> Result<()> {
    let client = Client::try_default().await?;

    let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());
    let svc = svc_api
        .get(service)
        .await
        .context(format!("service '{service}' not found in current namespace"))?;

    let secret_api: Api<Secret> = Api::default_namespaced(client);

    let mut entries: Vec<(String, String)> = Vec::new();
    let mut redacted: Vec<String> = Vec::new();

    for env_var in &svc.spec.deploy.env {
        let key = &env_var.name;
        if let Some(value) = &env_var.value {
            entries.push((key.clone(), value.clone()));
            continue;
        }
        let Some(sref) = env_var.value_from.as_ref().and_then(|vf| vf.secret_ref.as_ref())
        else {
            continue;
        };
        let secret = match secret_api.get(&sref.name).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "{} secret '{}' not readable: {e}",
                    "warning:".yellow(),
                    sref.name
                );
                redacted.push(key.clone());
                continue;
            }
        };
        let value = secret
            .data
            .as_ref()
            .and_then(|d| d.get(&sref.key))
            .and_then(|raw| String::from_utf8(raw.0.clone()).ok());
        match value {
            Some(v) => entries.push((key.clone(), v)),
            None => {
                eprintln!(
                    "{} secret '{}' has no key '{}'",
                    "warning:".yellow(),
                    sref.name,
                    sref.key,
                );
                redacted.push(key.clone());
            }
        }
    }

    // Merge with existing file unless --overwrite.
    let existing = if overwrite {
        Vec::new()
    } else {
        read_existing(output_path)?
    };

    // Existing keys win unless the service has a fresher value (which
    // happens when overwrite is set or the key was missing locally).
    let mut by_key: std::collections::BTreeMap<String, String> = entries.into_iter().collect();
    if !overwrite {
        for (k, v) in existing {
            by_key.entry(k).or_insert(v);
        }
    }

    write_env_file(output_path, &by_key)?;

    println!(
        "{} {} env vars to {}",
        "\u{2713}".green(),
        by_key.len().to_string().cyan(),
        output_path.display()
    );
    if !redacted.is_empty() {
        println!(
            "{} could not resolve: {}",
            "warning:".yellow(),
            redacted.join(", ").dimmed()
        );
    }
    Ok(())
}

fn read_existing(path: &Path) -> Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading existing env file {}", path.display()))?;
    let out = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(out)
}

fn write_env_file(path: &Path, vars: &std::collections::BTreeMap<String, String>) -> Result<()> {
    let mut body = String::new();
    for (k, v) in vars {
        // Quote values containing whitespace or special chars so they
        // survive being sourced by a shell. Single-quote is the safer
        // choice — no escape processing inside.
        let needs_quote = v
            .chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '$');
        if needs_quote {
            let escaped = v.replace('\'', "'\\''");
            body.push_str(&format!("{k}='{escaped}'\n"));
        } else {
            body.push_str(&format!("{k}={v}\n"));
        }
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::NamedTempFile;

    #[test]
    fn writes_simple_keys() {
        let f = NamedTempFile::new().unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        vars.insert("BAZ".to_string(), "qux".to_string());
        write_env_file(f.path(), &vars).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("BAZ=qux\n"));
        assert!(body.contains("FOO=bar\n"));
    }

    #[test]
    fn quotes_values_with_spaces() {
        let f = NamedTempFile::new().unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("MSG".to_string(), "hello world".to_string());
        write_env_file(f.path(), &vars).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("MSG='hello world'"));
    }

    #[test]
    fn quotes_values_with_dollar() {
        let f = NamedTempFile::new().unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("X".to_string(), "a$b".to_string());
        write_env_file(f.path(), &vars).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("X='a$b'"));
    }

    #[test]
    fn reads_existing_file() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "FOO=bar\n# comment\nBAZ=qux\n").unwrap();
        let existing = read_existing(f.path()).unwrap();
        assert_eq!(existing.len(), 2);
    }

    #[test]
    fn handles_missing_file() {
        let p = std::path::PathBuf::from("/nonexistent/path/12345");
        let existing = read_existing(&p).unwrap();
        assert!(existing.is_empty());
    }
}
