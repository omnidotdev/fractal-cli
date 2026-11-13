//! `fractal auth login | logout | whoami` — Better Auth email/password
//! flow against identity.omni.dev (matches the omni-cli pattern at
//! `cli-stack/services/cli/src/cli/auth.rs`).
//!
//! Token is persisted to `~/.config/fractal/config.toml` (0600 on
//! Unix). Other commands that need to call fractal-api pick it up via
//! `crate::cluster::auth_token()`, which falls through to
//! `FRACTAL_AUTH_TOKEN` env when the config has no token. The env
//! override stays for CI scenarios.
//!
//! A device-code grant against Gatekeeper would be cleaner but the
//! discovery doc at `https://identity.omni.dev/.well-known/oauth-authorization-server`
//! advertises only `response_types_supported = ["code"]`, so device
//! code isn't available today.

use std::io::{self, Write};

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::config::Config;

const DEFAULT_AUTH_URL: &str = "https://identity.omni.dev";

/// Auth base URL. Honors `FRACTAL_AUTH_URL` so dev/staging instances
/// can point at a non-prod Gatekeeper.
fn auth_url() -> String {
    std::env::var("FRACTAL_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string())
}

/// Bearer token supplied out-of-band via `FRACTAL_AUTH_TOKEN` (CI and
/// automation). Empty is treated as unset. This is the same override
/// the API-calling commands resolve first (see `cluster::api_session`),
/// so when it is present an interactive `fractal auth login` is
/// unnecessary.
fn env_token() -> Option<String> {
    std::env::var("FRACTAL_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// Machine-readable identity for `fractal auth whoami --json`.
#[derive(Serialize)]
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
pub async fn login(json: bool) -> Result<()> {
    if env_token().is_some() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&WhoamiInfo {
                    authenticated: true,
                    source: Some("env"),
                    email: None,
                    user_id: None,
                })?
            );
        } else {
            println!(
                "{} {} is set; it authenticates every command, so no login is needed",
                "Already authenticated.".bold(),
                "FRACTAL_AUTH_TOKEN".cyan(),
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

/// Print the currently logged-in identity. Resolves `FRACTAL_AUTH_TOKEN`
/// first (the CI/automation override), then the persisted login config.
/// No network call either way.
///
/// # Errors
///
/// Returns an error if the config can't be loaded or JSON serialization
/// fails.
pub fn whoami(json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let info = if env_token().is_some() {
        WhoamiInfo {
            authenticated: true,
            source: Some("env"),
            email: cfg.auth.email,
            user_id: cfg.auth.user_id,
        }
    } else if cfg.auth.access_token.is_some() {
        WhoamiInfo {
            authenticated: true,
            source: Some("config"),
            email: cfg.auth.email,
            user_id: cfg.auth.user_id,
        }
    } else {
        WhoamiInfo {
            authenticated: false,
            source: None,
            email: None,
            user_id: None,
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    match (info.authenticated, info.source, info.email.as_deref()) {
        (true, Some("env"), Some(email)) => {
            println!(
                "{} {} ({})",
                "Authenticated as".bold(),
                email.cyan(),
                "via FRACTAL_AUTH_TOKEN".dimmed(),
            );
        }
        (true, Some("env"), None) => {
            println!(
                "{} {}",
                "Authenticated".bold(),
                "via FRACTAL_AUTH_TOKEN".dimmed(),
            );
        }
        (true, _, Some(email)) => {
            println!("{} {}", "Logged in as".bold(), email.cyan());
        }
        (true, _, None) => {
            println!("{}", "Logged in (no email on record)".bold());
        }
        _ => {
            println!(
                "{} run {} to authenticate",
                "Not logged in.".bold(),
                "fractal auth login".cyan(),
            );
        }
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
