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

/// Mint a per-cluster install token from the Fractal API.
///
/// # Errors
///
/// Returns an error if the API URL or auth token env vars are missing,
/// the HTTP call fails, or the server returns a non-2xx status.
async fn mint_install_token(
    cluster_name: Option<&str>,
) -> Result<MintTokenResponse> {
    let base = api_url().context(
        "FRACTAL_API_URL not set (point at https://api.fractal.omni.dev or your dev API)",
    )?;
    let token = auth_token().context(
        "FRACTAL_AUTH_TOKEN not set (paste a Gatekeeper bearer from the dashboard)",
    )?;

    let url = format!("{}/v1/clusters/install-tokens", base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build HTTP client")?;
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
