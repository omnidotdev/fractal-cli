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
use serde::Deserialize;

use crate::config::Config;

const DEFAULT_AUTH_URL: &str = "https://identity.omni.dev";

/// Auth base URL. Honors `FRACTAL_AUTH_URL` so dev/staging instances
/// can point at a non-prod Gatekeeper.
fn auth_url() -> String {
    std::env::var("FRACTAL_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string())
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
/// # Errors
///
/// Returns an error if stdin can't be read, the login request fails,
/// or the config can't be written.
pub async fn login() -> Result<()> {
    let base = auth_url();
    let email = prompt("Email: ")?;
    let password = rpassword::prompt_password("Password: ")
        .context("failed to read password")?;

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

    let user = data.user.unwrap_or(LoginUser {
        id: None,
        email: Some(email.clone()),
    });

    let mut cfg = Config::load()?;
    cfg.auth.access_token = Some(token);
    cfg.auth.user_id = user.id;
    cfg.auth.email = user.email.clone().or(Some(email.clone()));
    cfg.save()?;

    println!(
        "{} {}",
        "Logged in as".bold(),
        user.email.unwrap_or(email).cyan()
    );
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

/// Print the currently logged-in identity (from the stored config,
/// no network call).
///
/// # Errors
///
/// Returns an error if the config can't be loaded.
pub fn whoami() -> Result<()> {
    let cfg = Config::load()?;
    match (cfg.auth.email.as_deref(), cfg.auth.access_token.is_some()) {
        (Some(email), true) => {
            println!("{} {}", "Logged in as".bold(), email.cyan());
        }
        (_, true) => {
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
