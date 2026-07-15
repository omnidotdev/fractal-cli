use std::process::Command;

use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::Api;

use fractal_operator::crd::service::{FractalService, ServiceType};

/// Connect to a Database service's Postgres via `psql`.
///
/// Looks up the CNPG-managed app secret `{service}-app`, extracts the
/// connection parameters, and either prints the connection string
/// (`--print-only`) or execs `psql` with the credentials in env.
///
/// # Errors
///
/// Returns an error if the service isn't a Database, if the secret
/// can't be read, or if `psql` is not on PATH (when not `--print-only`).
pub async fn db_connect(service: &str, print_only: bool) -> Result<()> {
    let client = Client::try_default().await?;

    let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());
    let svc = svc_api.get(service).await.context(format!(
        "service '{service}' not found in current namespace"
    ))?;

    if !matches!(svc.spec.service_type, ServiceType::Database) {
        anyhow::bail!("'{service}' is not a Database service");
    }

    let secret_api: Api<Secret> = Api::default_namespaced(client);
    let secret_name = format!("{service}-app");
    let secret = secret_api
        .get(&secret_name)
        .await
        .context(format!("CNPG app secret '{secret_name}' not found"))?;

    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("secret '{secret_name}' has no data"))?;

    let user = decode_field(data, "username")?;
    let password = decode_field(data, "password")?;
    let host = decode_field(data, "host").unwrap_or_else(|_| format!("{service}-rw"));
    let port = decode_field(data, "port").unwrap_or_else(|_| "5432".to_string());
    let dbname = decode_field(data, "dbname").unwrap_or_else(|_| user.clone());

    if print_only {
        // Don't echo the password; print a connection string suitable for
        // pasting into a tool that prompts for the password separately.
        println!("host={host} port={port} user={user} dbname={dbname}");
        println!(
            "{} (password redacted; available in secret {secret_name})",
            "info:".dimmed()
        );
        return Ok(());
    }

    println!(
        "{} {}@{}:{}/{}",
        "\u{2192}".green(),
        user.cyan(),
        host.cyan(),
        port,
        dbname.cyan(),
    );

    // Exec psql, passing the password via PGPASSWORD env. psql then uses
    // libpq the same way every Postgres client does.
    let status = Command::new("psql")
        .env("PGPASSWORD", password)
        .arg("-h")
        .arg(&host)
        .arg("-p")
        .arg(&port)
        .arg("-U")
        .arg(&user)
        .arg("-d")
        .arg(&dbname)
        .status()
        .context("failed to exec psql (is it on PATH?)")?;

    if !status.success() {
        anyhow::bail!("psql exited with status {status}");
    }
    Ok(())
}

fn decode_field(
    data: &std::collections::BTreeMap<String, k8s_openapi::ByteString>,
    key: &str,
) -> Result<String> {
    let raw = data
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("field '{key}' missing from secret"))?;
    // Secret data is already decoded by the kube client into ByteString,
    // but we still need to interpret the bytes as UTF-8.
    let s =
        String::from_utf8(raw.0.clone()).context(format!("field '{key}' is not valid UTF-8"))?;
    Ok(s)
}
