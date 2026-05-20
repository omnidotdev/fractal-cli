use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::core::v1::Node;
use kube::api::ListParams;
use kube::config::{Kubeconfig, KubeConfigOptions};
use kube::{Api, Client, Config};

/// Fallback install bundle URL printed when the per-cluster mint
/// endpoint isn't reachable (e.g. the user hasn't set `FRACTAL_API_URL`
/// or `FRACTAL_AUTH_TOKEN`). Lets the CLI still produce a runnable
/// install command for users on the OSS path.
const DEFAULT_BUNDLE_URL: &str =
    "https://fractal.omni.dev/install/operator.yaml";

/// Public base URL of the Fractal API. Used to mint per-cluster install
/// tokens against `POST /v1/clusters/install-tokens`. Required - the CLI
/// has no embedded default because production and dev hit different
/// hosts.
fn api_url() -> Option<String> {
    std::env::var("FRACTAL_API_URL").ok()
}

/// Gatekeeper-issued bearer token used to authenticate the mint call.
/// Lookup order:
///   1. `FRACTAL_AUTH_TOKEN` env var (CI/automation override).
///   2. `~/.config/fractal/config.toml` populated by
///      `fractal auth login`.
fn auth_token() -> Option<String> {
    if let Ok(env) = std::env::var("FRACTAL_AUTH_TOKEN") {
        if !env.is_empty() {
            return Some(env);
        }
    }
    crate::config::Config::load()
        .ok()
        .and_then(|c| c.auth.access_token)
}

/// Shape of `POST /v1/clusters/install-tokens` request body. Currently
/// just the optional human-readable cluster label.
#[derive(serde::Serialize)]
struct MintTokenRequest<'a> {
    #[serde(rename = "clusterName", skip_serializing_if = "Option::is_none")]
    cluster_name: Option<&'a str>,
}

/// Shape of the successful mint response. Matches
/// `src/routes/clusters.ts` on the server side.
#[derive(serde::Deserialize)]
struct MintTokenResponse {
    #[serde(rename = "installUrl")]
    install_url: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
    instructions: String,
}

/// Shape of one row in the list endpoint response. Mirrors the
/// `presentToken` function in `fractal-api/src/routes/clusters.ts`.
#[derive(serde::Deserialize)]
struct ListedInstallToken {
    id: String,
    #[serde(rename = "clusterName")]
    cluster_name: Option<String>,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
}

#[derive(serde::Deserialize)]
struct ListedInstallTokensResponse {
    tokens: Vec<ListedInstallToken>,
}

/// Shared helper: build a reqwest client + resolve API URL + token.
fn api_session() -> Result<(String, String, reqwest::Client)> {
    let base = api_url().context(
        "FRACTAL_API_URL not set (point at https://api.fractal.omni.dev or your dev API)",
    )?;
    let token = auth_token().context(
        "Not authenticated. Run `fractal auth login` (or set FRACTAL_AUTH_TOKEN).",
    )?;
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build HTTP client")?;
    Ok((base, token, client))
}

/// Mint a per-cluster install token from the Fractal API.
///
/// # Errors
///
/// Returns an error if the API URL or auth token env vars are missing,
/// the HTTP call fails, or the server returns a non-2xx status.
async fn mint_install_token(
    cluster_name: Option<&str>,
) -> Result<MintTokenResponse> {
    let (base, token, client) = api_session()?;
    let url = format!("{}/v1/clusters/install-tokens", base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&MintTokenRequest { cluster_name })
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "mint endpoint returned {status}: {}",
            if body.is_empty() { "<empty body>" } else { &body }
        );
    }

    resp.json::<MintTokenResponse>()
        .await
        .context("install token response could not be parsed")
}

/// List the caller's install tokens. Prints a table with status,
/// cluster label, and expiry. Output matches the dashboard's Clusters
/// page shape.
///
/// # Errors
///
/// Returns an error if env vars are missing, the HTTP call fails, or
/// the server returns a non-2xx status.
pub async fn list_tokens() -> Result<()> {
    let (base, token, client) = api_session()?;
    let url = format!("{}/v1/clusters/install-tokens", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "list endpoint returned {status}: {}",
            if body.is_empty() { "<empty body>" } else { &body }
        );
    }

    let parsed: ListedInstallTokensResponse = resp
        .json()
        .await
        .context("list response could not be parsed")?;

    if parsed.tokens.is_empty() {
        println!("{}", "(no install tokens issued)".dimmed());
        return Ok(());
    }

    println!(
        "{:<24}  {:<10}  {:<24}  {}",
        "CLUSTER".bold(),
        "STATUS".bold(),
        "CREATED".bold(),
        "EXPIRES".bold(),
    );
    for tok in parsed.tokens {
        let cluster = tok.cluster_name.as_deref().unwrap_or("(unnamed)");
        let status = match tok.status.as_str() {
            "active" => tok.status.green().to_string(),
            "used" => tok.status.dimmed().to_string(),
            _ => tok.status.yellow().to_string(),
        };
        println!(
            "{:<24}  {:<10}  {:<24}  {}\n  {}: {}",
            cluster.cyan(),
            status,
            tok.created_at.dimmed(),
            tok.expires_at.dimmed(),
            "id".dimmed(),
            tok.id.dimmed(),
        );
    }
    Ok(())
}

/// Revoke a previously-minted install token by id. fractal-api scopes
/// the delete to the caller's tokens; a 404 means the id wasn't yours
/// or never existed.
///
/// # Errors
///
/// Returns an error if env vars are missing, the HTTP call fails, or
/// the server returns a non-2xx status.
pub async fn revoke_token(id: &str) -> Result<()> {
    let (base, token, client) = api_session()?;
    let url = format!(
        "{}/v1/clusters/install-tokens/{}",
        base.trim_end_matches('/'),
        urlencode_path_segment(id),
    );
    let resp = client
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("DELETE {url} failed"))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        anyhow::bail!("token {id} not found (it may belong to another user, or never existed)");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "revoke endpoint returned {status}: {}",
            if body.is_empty() { "<empty body>" } else { &body }
        );
    }
    println!("{} token {} revoked", "\u{2713}".green(), id.cyan());
    Ok(())
}

/// Minimal URL-path-segment escape. The install-token id is a UUID
/// per the DB column default, but defending against arbitrary input is
/// cheap and avoids surprises if the id format ever changes.
fn urlencode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Resolve the kubeconfig path to use.
///
/// Precedence:
///   1. Explicit `--kubeconfig` flag value
///   2. `KUBECONFIG` env var
///   3. `~/.kube/config`
///
/// The env-reading side effects live in the public wrapper; the inner
/// `resolve_kubeconfig_path_inner` is pure so tests don't need to mutate
/// process env (which is `unsafe` under edition 2024).
fn resolve_kubeconfig_path(explicit: Option<&Path>) -> Result<PathBuf> {
    let kubeconfig_env = std::env::var("KUBECONFIG").ok();
    let home_env = std::env::var("HOME").ok();
    resolve_kubeconfig_path_inner(explicit, kubeconfig_env.as_deref(), home_env.as_deref())
}

fn resolve_kubeconfig_path_inner(
    explicit: Option<&Path>,
    kubeconfig_env: Option<&str>,
    home_env: Option<&str>,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(env) = kubeconfig_env {
        if let Some(first) = env.split(':').next() {
            if !first.is_empty() {
                return Ok(PathBuf::from(first));
            }
        }
    }
    let home = home_env.context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".kube/config"))
}

/// Mint an install token and print its URL + instructions. Returns
/// `true` if mint succeeded (caller should skip the static fallback);
/// `false` if mint failed (caller should print the static fallback).
/// Errors are logged inline as warnings - this is a soft-fail because
/// the OSS bundle is still a usable install path.
async fn try_mint_and_print(cluster_name: Option<&str>) -> bool {
    match mint_install_token(cluster_name).await {
        Ok(minted) => {
            println!(
                "{} {}):",
                "Install URL (single-use, expires".bold(),
                minted.expires_at.bold()
            );
            println!("  {}", minted.install_url.cyan());
            println!();
            println!("{}", "To install, run:".bold());
            for line in minted.instructions.lines() {
                println!("  {}", line.dimmed());
            }
            println!();
            println!(
                "{} once the operator pod is Ready, run {} to verify.",
                "Then:".bold(),
                "fractal cluster status".cyan()
            );
            true
        }
        Err(err) => {
            println!(
                "{} could not mint install token: {err}",
                "Warning:".yellow().bold(),
            );
            println!(
                "{} falling back to static OSS bundle URL",
                "Note:".yellow().bold(),
            );
            println!();
            false
        }
    }
}

/// Validate cluster connectivity, then print install instructions.
///
/// When `FRACTAL_API_URL` + `FRACTAL_AUTH_TOKEN` are set, mints a
/// single-use install token via the API and prints the signed URL +
/// curl-into-kubectl command. Otherwise falls back to the static OSS
/// bundle URL.
///
/// # Errors
///
/// Returns an error if the kubeconfig cannot be loaded or the apiserver
/// is unreachable. Cluster introspection failures (node list, version)
/// are surfaced as warnings rather than hard errors so the user still
/// gets the install URL. Mint failures are also surfaced as warnings;
/// the fallback bundle URL still works for OSS installs.
pub async fn connect(
    kubeconfig: Option<PathBuf>,
    context: Option<String>,
    cluster_name: Option<String>,
) -> Result<()> {
    let path = resolve_kubeconfig_path(kubeconfig.as_deref())?;
    if !path.exists() {
        anyhow::bail!(
            "kubeconfig not found at {} (pass --kubeconfig or set KUBECONFIG)",
            path.display()
        );
    }

    println!(
        "{} {}",
        "Loading kubeconfig:".bold(),
        path.display().to_string().dimmed()
    );

    let kc = Kubeconfig::read_from(&path)
        .with_context(|| format!("failed to parse kubeconfig at {}", path.display()))?;

    let opts = KubeConfigOptions {
        context: context.clone(),
        cluster: None,
        user: None,
    };
    let config = Config::from_custom_kubeconfig(kc.clone(), &opts)
        .await
        .context("failed to build client config from kubeconfig")?;

    let server = config.cluster_url.to_string();
    let client = Client::try_from(config).context("failed to build kube client")?;

    // Sanity-ping the apiserver. List nodes — cheap, present on every
    // cluster, and gives us a node count to show the user.
    let nodes: Api<Node> = Api::all(client.clone());
    let node_list = nodes.list(&ListParams::default().limit(20)).await;

    let active_ctx = context
        .clone()
        .or_else(|| kc.current_context.clone())
        .unwrap_or_else(|| "<none>".to_string());

    println!("{} {}", "Context:".bold(), active_ctx.cyan());
    println!("{} {}", "Apiserver:".bold(), server.cyan());

    match node_list {
        Ok(list) => {
            let count = list.items.len();
            let truncated = if count == 20 { "+" } else { "" };
            println!(
                "{} {} nodes reachable",
                "Connectivity:".bold(),
                format!("{count}{truncated}").green(),
            );
        }
        Err(err) => {
            println!(
                "{} {} ({})",
                "Connectivity:".bold(),
                "could not list nodes".yellow(),
                err
            );
        }
    }

    println!();

    if api_url().is_some()
        && auth_token().is_some()
        && try_mint_and_print(cluster_name.as_deref()).await
    {
        return Ok(());
    }

    println!(
        "{} {}",
        "Install bundle (static, unsigned):".bold(),
        DEFAULT_BUNDLE_URL.cyan()
    );

    println!();
    println!("{}", "To install, run:".bold());
    println!(
        "  {} {}",
        "kubectl apply -f".dimmed(),
        DEFAULT_BUNDLE_URL.cyan()
    );
    println!();
    println!(
        "{} once the operator pod is Ready, run {} to verify.",
        "Then:".bold(),
        "fractal cluster status".cyan()
    );

    Ok(())
}

/// Inspect cluster install state.
///
/// Reports whether the fractal-operator Deployment is present and Ready
/// in the `fractal-system` namespace.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable. Missing operator is
/// reported as informational output, not an error.
pub async fn status() -> Result<()> {
    use k8s_openapi::api::apps::v1::Deployment;

    let client = Client::try_default()
        .await
        .context("failed to build kube client (is kubeconfig configured?)")?;

    let deploys: Api<Deployment> = Api::namespaced(client, "fractal-system");

    if let Some(dep) = deploys.get_opt("fractal-operator").await? {
        let status = dep.status.unwrap_or_default();
        let ready = status.ready_replicas.unwrap_or(0);
        let desired = status.replicas.unwrap_or(0);
        if ready > 0 && ready == desired {
            println!(
                "{} operator {} ({}/{} replicas ready)",
                "\u{2713}".green(),
                "Ready".green(),
                ready,
                desired,
            );
        } else {
            println!(
                "{} operator {} ({}/{} replicas ready)",
                "\u{26A0}".yellow(),
                "NotReady".yellow(),
                ready,
                desired,
            );
        }
    } else {
        println!(
            "{} no fractal-operator Deployment in {} namespace",
            "\u{2717}".red(),
            "fractal-system".bold(),
        );
        println!("  run {} to install", "fractal cluster connect".cyan());
    }

    Ok(())
}

/// Disconnect the local context from the Fractal dashboard.
///
/// Currently a placeholder — the live cluster registration / heartbeat
/// service that this command revokes against isn't deployed yet. Once
/// the install bundle generator + cluster registration endpoint land,
/// this will revoke the cluster's outbound credentials (Vortex/Aether
/// mTLS) so the dashboard treats the cluster as disconnected.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable.
#[allow(clippy::unused_async)] // signature locked in for when the SaaS endpoint lands
pub async fn disconnect() -> Result<()> {
    println!(
        "{} Cluster registration / revocation endpoint not yet deployed.",
        "Note:".yellow().bold(),
    );
    println!(
        "  Workloads on this cluster will keep running. To stop the operator,"
    );
    println!(
        "  delete the Deployment in {}: {}",
        "fractal-system".bold(),
        "kubectl -n fractal-system delete deploy fractal-operator".cyan(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_kubeconfig_path_inner;
    use std::path::PathBuf;

    #[test]
    fn explicit_path_wins() {
        let explicit = PathBuf::from("/tmp/custom-kubeconfig");
        let resolved =
            resolve_kubeconfig_path_inner(Some(&explicit), Some("/tmp/from-env"), Some("/home/u"))
                .unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn env_var_used_when_no_explicit_path() {
        let resolved =
            resolve_kubeconfig_path_inner(None, Some("/tmp/from-env"), Some("/home/u")).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/from-env"));
    }

    #[test]
    fn first_kubeconfig_entry_wins_when_colon_separated() {
        let resolved =
            resolve_kubeconfig_path_inner(None, Some("/tmp/primary:/tmp/secondary"), Some("/h"))
                .unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/primary"));
    }

    #[test]
    fn empty_kubeconfig_entry_falls_through_to_home() {
        let resolved = resolve_kubeconfig_path_inner(None, Some(":"), Some("/home/u")).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u/.kube/config"));
    }

    #[test]
    fn home_used_when_no_env_set() {
        let resolved = resolve_kubeconfig_path_inner(None, None, Some("/home/u")).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u/.kube/config"));
    }

    #[test]
    fn missing_home_is_an_error() {
        let err = resolve_kubeconfig_path_inner(None, None, None).unwrap_err();
        assert!(err.to_string().contains("HOME"));
    }
}
