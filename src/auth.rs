//! `fractal auth login | logout | whoami`. Login either stores a Fractal
//! API token (`--token ftk_...`) or runs the Better Auth email/password
//! flow against identity.omni.dev (matches the omni-cli pattern at
//! `cli-stack/services/cli/src/cli/auth.rs`).
//!
//! The token is persisted to `~/.config/fractal/config.toml` (0600 on
//! Unix). Commands resolve their credential through
//! `crate::api::credential()`: `FRACTAL_API_TOKEN`, then the older
//! `FRACTAL_AUTH_TOKEN`, then the persisted token.
//!
//! A device-code grant against Gatekeeper would be cleaner but the
//! discovery doc at `https://identity.omni.dev/.well-known/oauth-authorization-server`
//! advertises only `response_types_supported = ["code"]`, so device
//! code isn't available today.

use std::io::{self, Write};

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::api::{self, Credential, CredentialSource};
use crate::config::Config;
use crate::transport::Transport;

const DEFAULT_AUTH_URL: &str = "https://identity.omni.dev";

/// Auth base URL. Honors `FRACTAL_AUTH_URL` so dev/staging instances
/// can point at a non-prod Gatekeeper.
fn auth_url() -> String {
    std::env::var("FRACTAL_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string())
}

/// Credential supplied out-of-band through `FRACTAL_API_TOKEN` or
/// `FRACTAL_AUTH_TOKEN` (CI and automation). When present, an
/// interactive `fractal auth login` is unnecessary.
fn env_credential() -> Option<Credential> {
    api::select_credential(
        std::env::var(api::TOKEN_ENV).ok().as_deref(),
        std::env::var(api::LEGACY_TOKEN_ENV).ok().as_deref(),
        None,
    )
}

/// Machine-readable identity for `fractal auth whoami --json`.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct WhoamiInfo {
    authenticated: bool,
    /// Where the active credential comes from: `"env"`
    /// (`FRACTAL_AUTH_TOKEN`) or `"config"` (persisted login).
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// Transport commands will use (`api` or `kube`)
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
    /// API base URL, when the transport is `api`
    #[serde(skip_serializing_if = "Option::is_none")]
    api_url: Option<String>,
    /// Projects the credential can see, when verified against the API
    #[serde(skip_serializing_if = "Option::is_none")]
    projects: Option<usize>,
}

#[derive(Deserialize)]
struct LoginResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    user: Option<LoginUser>,
}

#[derive(Deserialize)]
struct LoginUser {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

/// Prompt for email + password, exchange them for a bearer at
/// `/api/auth/sign-in/email`, persist the token.
///
/// When `FRACTAL_AUTH_TOKEN` is set, the interactive prompt is skipped
/// entirely: the env token already authenticates every command, so
/// there is nothing to do. This keeps `fractal auth login` safe to call
/// unattended from CI.
///
/// # Errors
///
/// Returns an error if stdin can't be read, the login request fails,
/// or the config can't be written.
pub async fn login(json: bool, token: Option<String>) -> Result<()> {
    if let Some(token) = token {
        return login_with_token(json, &token).await;
    }
    if let Some(cred) = env_credential() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&WhoamiInfo {
                    authenticated: true,
                    source: Some("env"),
                    ..WhoamiInfo::default()
                })?
            );
        } else {
            println!(
                "{} {} is set; it authenticates every command, so no login is needed",
                "Already authenticated.".bold(),
                cred.source.label().cyan(),
            );
        }
        return Ok(());
    }

    let base = auth_url();
    let email = prompt("Email: ")?;
    let password = rpassword::prompt_password("Password: ").context("failed to read password")?;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/auth/sign-in/email"))
        .json(&serde_json::json!({
            "email": email,
            "password": password,
        }))
        .send()
        .await
        .context("login request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("login failed ({status}): {body}");
    }

    let data: LoginResponse = resp
        .json()
        .await
        .context("login response was not valid JSON")?;

    let token = data
        .token
        .ok_or_else(|| anyhow::anyhow!("login response had no `token` field"))?;

    let user = data.user.unwrap_or_else(|| LoginUser {
        id: None,
        email: Some(email.clone()),
    });

    let mut cfg = Config::load()?;
    cfg.auth.access_token = Some(token);
    cfg.auth.user_id = user.id;
    cfg.auth.email = user.email.or(Some(email));
    cfg.save()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&WhoamiInfo {
                authenticated: true,
                source: Some("config"),
                email: cfg.auth.email,
                user_id: cfg.auth.user_id,
                ..WhoamiInfo::default()
            })?
        );
    } else if let Some(display) = &cfg.auth.email {
        println!("{} {}", "Logged in as".bold(), display.cyan());
    } else {
        println!("{}", "Logged in".bold());
    }
    Ok(())
}

/// Clear the persisted token. Does NOT call the auth server's revoke
/// endpoint - the token will expire naturally on the server side.
///
/// # Errors
///
/// Returns an error if the config can't be loaded or saved.
pub fn logout() -> Result<()> {
    let mut cfg = Config::load()?;
    let was_logged_in = cfg.auth.access_token.is_some();
    cfg.auth.access_token = None;
    cfg.auth.user_id = None;
    cfg.auth.email = None;
    cfg.save()?;
    if was_logged_in {
        println!("{}", "Logged out".bold());
    } else {
        println!("{}", "Not logged in".dimmed());
    }
    Ok(())
}

/// Read the token for `--token`: the value itself, or stdin for `-`
fn read_token_arg(arg: &str, stdin: impl io::BufRead) -> Result<String> {
    let token = if arg == "-" {
        let mut line = String::new();
        let mut stdin = stdin;
        stdin
            .read_line(&mut line)
            .context("failed to read the token from stdin")?;
        line
    } else {
        arg.to_string()
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("the token is empty");
    }
    Ok(token)
}

/// Count the projects a credential can see. Doubles as a check that the
/// API accepts it.
async fn verify(token: &str) -> Result<usize> {
    let client = api::ApiClient::new(&api::api_url(), token)?;
    Ok(api::resolve::list_projects(&client).await?.len())
}

/// Persist a Fractal API token after checking that the API accepts it
async fn login_with_token(json: bool, arg: &str) -> Result<()> {
    let token = read_token_arg(arg, io::stdin().lock())?;
    let projects = verify(&token).await?;

    let mut cfg = Config::load()?;
    cfg.auth.access_token = Some(token);
    cfg.auth.user_id = None;
    cfg.auth.email = None;
    cfg.save()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&WhoamiInfo {
                authenticated: true,
                source: Some("config"),
                api_url: Some(api::api_url()),
                projects: Some(projects),
                ..WhoamiInfo::default()
            })?
        );
        return Ok(());
    }
    println!(
        "{} API token saved ({} project{} visible)",
        "\u{2713}".green(),
        projects,
        if projects == 1 { "" } else { "s" }
    );
    if let Some(cred) = env_credential() {
        println!(
            "{} {} is set and takes precedence over the saved token",
            "Note:".yellow(),
            cred.source.label().cyan()
        );
    }
    if cfg.transport.as_deref() == Some("kube") {
        println!(
            "{} this machine is set to the kube transport (from `fractal cluster connect`); pass {} or set {} to use the API",
            "Note:".yellow(),
            "--transport api".cyan(),
            "FRACTAL_TRANSPORT=api".cyan()
        );
    }
    Ok(())
}

/// Print the active identity. Resolves `FRACTAL_API_TOKEN`, then
/// `FRACTAL_AUTH_TOKEN`, then the persisted login. On the API transport
/// the credential is checked against the API; on kube there is no
/// network call.
///
/// # Errors
///
/// Returns an error if the config can't be loaded, the API rejects the
/// credential, or JSON serialization fails.
pub async fn whoami(json: bool, transport: Transport) -> Result<()> {
    let cfg = Config::load()?;
    let Some(cred) = api::credential_from(&cfg) else {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&WhoamiInfo {
                    transport: Some(transport.to_string()),
                    ..WhoamiInfo::default()
                })?
            );
        } else {
            println!(
                "{} run {} or set {} to authenticate",
                "Not logged in.".bold(),
                "fractal auth login".cyan(),
                "FRACTAL_API_TOKEN".cyan(),
            );
        }
        return Ok(());
    };

    let (projects, api_url) = if transport == Transport::Api {
        (Some(verify(&cred.token).await?), Some(api::api_url()))
    } else {
        (None, None)
    };

    let info = WhoamiInfo {
        authenticated: true,
        source: Some(cred.source.json_value()),
        email: cfg.auth.email,
        user_id: cfg.auth.user_id,
        transport: Some(transport.to_string()),
        api_url,
        projects,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    let who = info.email.as_deref().map_or_else(
        || {
            if cred.token.starts_with("ftk_") {
                "with an API token".to_string()
            } else {
                "(no email on record)".to_string()
            }
        },
        |e| format!("as {}", e.cyan()),
    );
    let via = match cred.source {
        CredentialSource::Config => "saved login".to_string(),
        other => other.label().to_string(),
    };
    println!(
        "{} {} ({})",
        "Authenticated".bold(),
        who,
        format!("via {via}").dimmed()
    );
    println!("  {} {}", "Transport:".bold(), transport);
    if let (Some(url), Some(n)) = (info.api_url.as_deref(), info.projects) {
        println!("  {} {}", "API:".bold(), url.dimmed());
        println!("  {} {n}", "Projects:".bold());
    }
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    Ok(input.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::read_token_arg;

    #[test]
    fn token_arg_is_trimmed() {
        assert_eq!(read_token_arg("  ftk_abc \n", &b""[..]).unwrap(), "ftk_abc");
    }

    #[test]
    fn dash_reads_token_from_stdin() {
        assert_eq!(
            read_token_arg("-", &b"ftk_piped\n"[..]).unwrap(),
            "ftk_piped"
        );
    }

    #[test]
    fn empty_token_is_rejected() {
        assert!(read_token_arg("-", &b"\n"[..]).is_err());
        assert!(read_token_arg("  ", &b""[..]).is_err());
    }
}
