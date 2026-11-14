//! `fractal login | logout | whoami` (also under `fractal auth`).
//!
//! `fractal login` runs the standard browser login against Gatekeeper
//! (see [`crate::oauth`]) and saves the access token, refresh token, and
//! expiry to the config file (0600 on Unix): `~/.config/fractal/config.toml`
//! on Linux, `~/Library/Application Support/fractal/config.toml` on macOS. Every API
//! command then refreshes the access token silently before it expires.
//! `fractal login --token ftk_...` saves a Fractal API token instead.
//!
//! The authorization request always carries `prompt=consent`, so the
//! browser shows an "Authorize Fractal CLI" page naming the signed-in
//! account rather than silently reusing the session;
//! `fractal login --switch-account` sends `prompt=login consent` to force
//! a fresh sign-in that still ends on the consent page. After a browser login the CLI selects the workspace to act
//! in (see [`crate::workspace`]).
//!
//! Commands resolve their credential through `crate::api::credential()`:
//! `FRACTAL_API_TOKEN`, then the older `FRACTAL_AUTH_TOKEN`, then the
//! saved login.

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::api::{self, Credential, CredentialSource};
use crate::config::{Config, TokenKind};
use crate::oauth::{self, Pkce, Prompt, TokenSet, Workspace};
use crate::transport::Transport;
use crate::workspace::{self, LoginChoice};

/// How long `fractal login` waits for the browser
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Credential supplied out-of-band through `FRACTAL_API_TOKEN` or
/// `FRACTAL_AUTH_TOKEN` (CI and automation). When present, an
/// interactive `fractal login` is unnecessary.
fn env_credential() -> Option<Credential> {
    api::select_credential(
        std::env::var(api::TOKEN_ENV).ok().as_deref(),
        std::env::var(api::LEGACY_TOKEN_ENV).ok().as_deref(),
        None,
    )
}

/// Machine-readable identity for `fractal whoami --json`
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct WhoamiInfo {
    authenticated: bool,
    /// Where the active credential comes from: `"env"`
    /// (`FRACTAL_API_TOKEN`) or `"config"` (saved login).
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    /// `"oauth"` for a browser login, `"api_token"` for a Fractal API
    /// token
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// Workspace commands act in (`--workspace`, `FRACTAL_WORKSPACE`,
    /// the saved workspace, else your only workspace)
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<Workspace>,
    /// Where `workspace` came from
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_source: Option<workspace::Source>,
    /// Every workspace the browser login belongs to
    #[serde(skip_serializing_if = "Option::is_none")]
    workspaces: Option<Vec<Workspace>>,
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

const fn kind_label(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Oauth => "oauth",
        TokenKind::ApiToken => "api_token",
    }
}

/// The credential kind for `whoami`: the saved kind, or an API token
/// recognized by its prefix
fn credential_kind(cred: &Credential, cfg: &Config) -> Option<&'static str> {
    if cred.source == CredentialSource::Config {
        if let Some(kind) = cfg.auth.kind {
            return Some(kind_label(kind));
        }
    }
    cred.token.starts_with("ftk_").then_some("api_token")
}

/// Log in. With `--token`, save a Fractal API token; otherwise run the
/// browser login and select the workspace to act in.
/// `switch_account` forces a fresh sign-in instead of the consent page
/// for the account already signed in to the browser.
///
/// When `FRACTAL_API_TOKEN` (or `FRACTAL_AUTH_TOKEN`) is set, the login
/// is skipped: the env token already authenticates every command. This
/// keeps `fractal login` safe to call unattended from CI.
///
/// # Errors
///
/// Returns an error if the login fails, times out, or the config can't
/// be written.
pub async fn login(
    json: bool,
    token: Option<String>,
    no_browser: bool,
    switch_account: bool,
) -> Result<()> {
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

    let prompt = if switch_account {
        Prompt::Login
    } else {
        Prompt::Consent
    };
    let (tokens, id_claims, leftover_lines) = browser_login(no_browser, prompt).await?;
    let claims = id_claims
        .or_else(|| oauth::decode_jwt_claims(&tokens.access_token))
        .unwrap_or(Value::Null);

    let mut cfg = Config::load()?;
    cfg.clear_auth();
    cfg.auth.access_token = Some(tokens.access_token);
    cfg.auth.kind = Some(TokenKind::Oauth);
    cfg.auth.refresh_token = tokens.refresh_token;
    cfg.auth.expires_at = tokens.expires_at;
    cfg.auth.user_id = oauth::claim_str(&claims, "sub");
    cfg.auth.email = oauth::claim_str(&claims, "email");
    cfg.auth.name = oauth::claim_str(&claims, "name");
    cfg.save()?;

    let memberships = select_login_workspace(&mut cfg, &claims, json, leftover_lines).await;
    let several = memberships.len() > 1;
    cfg.save()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&WhoamiInfo {
                authenticated: true,
                source: Some("config"),
                kind: Some(kind_label(TokenKind::Oauth)),
                email: cfg.auth.email.clone(),
                name: cfg.auth.name.clone(),
                user_id: cfg.auth.user_id.clone(),
                workspace_source: cfg.workspace.as_ref().map(|_| workspace::Source::Saved),
                workspace: cfg.workspace.clone(),
                workspaces: Some(workspace::sorted(&memberships)),
                ..WhoamiInfo::default()
            })?
        );
        return Ok(());
    }
    match &cfg.auth.email {
        Some(email) => println!("{} Logged in as {}", "\u{2713}".green(), email.cyan()),
        None => println!("{} Logged in", "\u{2713}".green()),
    }
    match &cfg.workspace {
        Some(w) => println!("  {} {}", "Workspace:".bold(), w.to_string().cyan()),
        None if several => workspace::print_pick_hint(),
        None => {}
    }
    if cfg.auth.refresh_token.is_none() {
        println!(
            "{} the identity provider issued no refresh token, so you will need to run {} again when this session expires",
            "Note:".yellow(),
            "fractal login".cyan()
        );
    }
    print_precedence_notes(&cfg);
    Ok(())
}

/// Pick the workspace at the end of a browser login (see
/// [`workspace::plan_login`]) and store it in `cfg`. Returns the login's
/// workspaces. Userinfo carries workspace names and slugs; the ID
/// token's claim is the fallback when it can't be reached.
async fn select_login_workspace(
    cfg: &mut Config,
    claims: &Value,
    json: bool,
    leftover_lines: Option<UnboundedReceiver<String>>,
) -> Vec<Workspace> {
    let access = cfg.auth.access_token.clone().unwrap_or_default();
    let memberships = match workspace::fetch_memberships(&access).await {
        Ok(all) if !all.is_empty() => all,
        _ => oauth::workspaces(claims),
    };
    let interactive = !json && workspace::interactive();
    cfg.workspace = match workspace::plan_login(&memberships, cfg.workspace.as_ref(), interactive) {
        LoginChoice::Nothing => None,
        LoginChoice::Only(w) => Some(w),
        LoginChoice::Pick { options, default } => {
            // Reuse the paste login's stdin reader so it can't swallow the answer
            let mut lines = leftover_lines.unwrap_or_else(oauth::spawn_line_reader);
            match workspace::prompt_pick(&options, default, &mut lines).await {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("{} {e}", "Note:".yellow());
                    None
                }
            }
        }
        LoginChoice::Hint { keep } => keep,
    };
    memberships
}

/// Wait for the next pasted line, or forever when not reading stdin
async fn next_line(lines: &mut Option<UnboundedReceiver<String>>) -> Option<String> {
    match lines {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Tokens, the ID token's claims, and the stdin reader a paste login
/// started (still reading, so later prompts must share it)
type LoginResult = (TokenSet, Option<Value>, Option<UnboundedReceiver<String>>);

/// Run the browser login and return the tokens plus the ID token's
/// claims
async fn browser_login(no_browser: bool, prompt: Prompt) -> Result<LoginResult> {
    let http = oauth::http_client()?;
    let discovery = oauth::discover(&http, &oauth::auth_url()).await?;
    let client_id = oauth::client_id();
    let pkce = Pkce::generate();
    let state = oauth::new_state();
    let listener = oauth::bind_loopback().await?;
    let redirect_uri = oauth::redirect_uri_for(listener.local_addr()?.port());
    let url = oauth::authorize_url(
        &discovery.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        prompt,
    )?;

    let mut paste = !oauth::should_open_browser(no_browser, oauth::needs_display_server(), |k| {
        std::env::var(k).ok()
    });
    if !paste {
        eprintln!("Opening your browser to sign in...");
        if oauth::open_browser(url.as_str()) {
            eprintln!(
                "{}\n\n  {url}\n",
                "If it didn't open, visit this URL:".dimmed()
            );
        } else {
            eprintln!("{}", "Couldn't open a browser.".yellow());
            paste = true;
        }
    }
    if paste {
        eprintln!("Open this URL in a browser to sign in:\n\n  {url}\n");
        eprintln!(
            "After you approve, the browser lands on a 127.0.0.1 page that may fail to load. \
             Copy the full URL from its address bar and paste it here:"
        );
    }

    let issuer = discovery.issuer.as_deref();
    let mut lines = paste.then(oauth::spawn_line_reader);
    let wait = async {
        let redirect = oauth::await_redirect(&listener, &state, issuer);
        tokio::pin!(redirect);
        loop {
            tokio::select! {
                res = &mut redirect => {
                    let (code, stream) = res?;
                    return Ok::<_, anyhow::Error>((code, Some(stream)));
                }
                line = next_line(&mut lines) => {
                    let Some(line) = line else {
                        // stdin closed; keep waiting on the listener
                        lines = None;
                        continue;
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match oauth::parse_pasted(&line) {
                        Err(e) => eprintln!("{} {e}", "Error:".red()),
                        Ok(p) => match oauth::validate_callback(&p, &state, issuer) {
                            Ok(code) => return Ok((code, None)),
                            // Keep waiting: the paste may be from an older attempt
                            Err(e @ oauth::CallbackError::StateMismatch) => eprintln!(
                                "{} {e}; paste the URL from the sign-in this command started",
                                "Error:".red()
                            ),
                            Err(e) => return Err(e.into()),
                        },
                    }
                }
            }
        }
    };
    let (code, browser) = tokio::time::timeout(LOGIN_TIMEOUT, wait)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after 5 minutes waiting for the browser sign-in; run `fractal login` to try again"
            )
        })??;

    let exchanged = oauth::exchange_code(
        &http,
        &discovery.token_endpoint,
        &client_id,
        &code,
        &pkce.verifier,
        &redirect_uri,
    )
    .await;
    if let Some(stream) = browser {
        oauth::finish_browser(stream, exchanged.is_ok()).await;
    }
    let resp = exchanged.map_err(|e| anyhow::anyhow!("sign-in failed: {e}"))?;
    let id_claims = resp.id_token.as_deref().and_then(oauth::decode_jwt_claims);
    Ok((
        TokenSet::from_response(resp, oauth::unix_now(), None),
        id_claims,
        lines,
    ))
}

/// Clear the saved login. A browser login's refresh token is revoked
/// first when the identity provider advertises a revocation endpoint.
///
/// # Errors
///
/// Returns an error if the config can't be loaded or saved.
pub async fn logout() -> Result<()> {
    let mut cfg = Config::load()?;
    let was_logged_in = cfg.auth.access_token.is_some();
    let revoke_failed = match cfg.auth.refresh_token.clone() {
        Some(refresh) if cfg.auth.is_oauth() => revoke(&refresh).await.is_err(),
        _ => false,
    };
    cfg.clear_auth();
    cfg.save()?;
    if was_logged_in {
        println!("{}", "Logged out".bold());
        if revoke_failed {
            println!(
                "{} couldn't revoke the session with the identity provider; the saved credentials were removed from this machine",
                "Note:".yellow()
            );
        }
    } else {
        println!("{}", "Not logged in".dimmed());
    }
    if let Some(cred) = env_credential() {
        println!(
            "{} {} is still set and keeps authenticating commands",
            "Note:".yellow(),
            cred.source.label().cyan()
        );
    }
    Ok(())
}

/// Revoke a refresh token at the advertised revocation endpoint. A
/// provider without one is not an error.
async fn revoke(refresh: &str) -> Result<()> {
    let http = oauth::http_client()?;
    let discovery = oauth::discover(&http, &oauth::auth_url()).await?;
    match discovery.revocation_endpoint.as_deref() {
        Some(endpoint) => {
            oauth::revoke_refresh_token(&http, endpoint, &oauth::client_id(), refresh).await
        }
        None => Ok(()),
    }
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

/// Count the projects a client can see. Doubles as a check that the API
/// accepts its credential.
async fn count_projects(client: &api::ApiClient) -> Result<usize> {
    Ok(api::resolve::list_projects(client).await?.len())
}

/// Notes about what overrides a saved login
fn print_precedence_notes(cfg: &Config) {
    if let Some(cred) = env_credential() {
        println!(
            "{} {} is set and takes precedence over the saved login",
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
}

/// Persist a Fractal API token after checking that the API accepts it
async fn login_with_token(json: bool, arg: &str) -> Result<()> {
    let token = read_token_arg(arg, io::stdin().lock())?;
    let client = api::ApiClient::new(&api::api_url(), &token)?;
    let projects = count_projects(&client).await?;

    let mut cfg = Config::load()?;
    cfg.clear_auth();
    cfg.auth.access_token = Some(token);
    cfg.auth.kind = Some(TokenKind::ApiToken);
    cfg.save()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&WhoamiInfo {
                authenticated: true,
                source: Some("config"),
                kind: Some(kind_label(TokenKind::ApiToken)),
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
    print_precedence_notes(&cfg);
    Ok(())
}

/// Identity details for a browser login, read from userinfo (which
/// carries workspace names) and falling back to what the login saved
/// plus the access token's claims when userinfo can't be reached
struct OauthIdentity {
    email: Option<String>,
    name: Option<String>,
    user_id: Option<String>,
    workspaces: Vec<Workspace>,
}

async fn oauth_identity(client: &api::ApiClient, cfg: &Config) -> Result<OauthIdentity> {
    let token = client.bearer_token().await?;
    let fetched = async {
        let http = oauth::http_client()?;
        let discovery = oauth::discover(&http, &oauth::auth_url()).await?;
        let endpoint = discovery
            .userinfo_endpoint
            .context("no userinfo endpoint advertised")?;
        oauth::userinfo(&http, &endpoint, &token).await
    }
    .await
    .ok();
    let token_claims = oauth::decode_jwt_claims(&token).unwrap_or(Value::Null);
    let rich = fetched
        .as_ref()
        .map(oauth::workspaces)
        .filter(|w| !w.is_empty());
    let pick = |key: &str, saved: &Option<String>| {
        fetched
            .as_ref()
            .and_then(|c| oauth::claim_str(c, key))
            .or_else(|| saved.clone())
    };
    Ok(OauthIdentity {
        email: pick("email", &cfg.auth.email),
        name: pick("name", &cfg.auth.name),
        user_id: pick("sub", &cfg.auth.user_id).or_else(|| oauth::claim_str(&token_claims, "sub")),
        workspaces: rich.unwrap_or_else(|| oauth::workspaces(&token_claims)),
    })
}

/// Print the active identity. Resolves `FRACTAL_API_TOKEN`, then
/// `FRACTAL_AUTH_TOKEN`, then the saved login. On the API transport
/// the credential is checked against the API. A browser login also
/// shows the account's email, name, and workspace.
///
/// # Errors
///
/// Returns an error if the config can't be loaded, the API rejects the
/// credential, the session can't be refreshed, or JSON serialization
/// fails.
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
                "fractal login".cyan(),
                "FRACTAL_API_TOKEN".cyan(),
            );
        }
        return Ok(());
    };
    let kind = credential_kind(&cred, &cfg);
    let is_oauth = kind == Some("oauth");
    let needs_client = transport == Transport::Api || is_oauth;
    let client = if needs_client {
        Some(api::ApiClient::from_env()?)
    } else {
        None
    };

    let (projects, api_url) = match (&client, transport) {
        (Some(c), Transport::Api) => (Some(count_projects(c).await?), Some(api::api_url())),
        _ => (None, None),
    };

    let mut info = WhoamiInfo {
        authenticated: true,
        source: Some(cred.source.json_value()),
        kind,
        email: cfg.auth.email.clone(),
        name: cfg.auth.name.clone(),
        user_id: cfg.auth.user_id.clone(),
        transport: Some(transport.to_string()),
        api_url,
        projects,
        ..WhoamiInfo::default()
    };
    if let (true, Some(c)) = (is_oauth, &client) {
        let identity = oauth_identity(c, &cfg).await?;
        if let Some((w, source)) = active_workspace(&cfg, &identity.workspaces) {
            info.workspace = Some(w);
            info.workspace_source = Some(source);
        }
        info.email = identity.email;
        info.name = identity.name;
        info.user_id = identity.user_id;
        info.workspaces = Some(workspace::sorted(&identity.workspaces));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    print_whoami(&info, &cred);
    Ok(())
}

/// The workspace a browser login acts in, with its name and slug from
/// `all`. A saved workspace is shown as saved when `all` couldn't be
/// fetched.
fn active_workspace(cfg: &Config, all: &[Workspace]) -> Option<(Workspace, workspace::Source)> {
    let ids: Vec<String> = all.iter().map(|w| w.id.clone()).collect();
    let req = workspace::requested(cfg, true, &ids)?;
    if let Some(w) = workspace::find(&req.value, all) {
        return Some((w.clone(), req.source));
    }
    if !all.is_empty() {
        return None;
    }
    let fallback = match req.source {
        workspace::Source::Saved => cfg.workspace.clone()?,
        _ => Workspace {
            id: req.value,
            name: None,
            slug: None,
        },
    };
    Some((fallback, req.source))
}

fn print_whoami(info: &WhoamiInfo, cred: &Credential) {
    let who = match (&info.email, &info.name) {
        (Some(email), Some(name)) => format!("as {} ({name})", email.cyan()),
        (Some(email), None) => format!("as {}", email.cyan()),
        (None, Some(name)) => format!("as {}", name.cyan()),
        (None, None) if info.kind == Some("api_token") => "with an API token".to_string(),
        (None, None) => "(no email on record)".to_string(),
    };
    let via = match (cred.source, info.kind) {
        (CredentialSource::Config, Some("oauth")) => "browser login".to_string(),
        (CredentialSource::Config, Some("api_token")) => "saved API token".to_string(),
        (CredentialSource::Config, _) => "saved login".to_string(),
        (other, _) => other.label().to_string(),
    };
    println!(
        "{} {} ({})",
        "Authenticated".bold(),
        who,
        format!("via {via}").dimmed()
    );
    if let Some(ws) = &info.workspace {
        let from = info
            .workspace_source
            .filter(|s| matches!(s, workspace::Source::Flag | workspace::Source::Env))
            .map(|s| format!(" (from {})", s.label()).dimmed().to_string())
            .unwrap_or_default();
        println!(
            "  {} {}{from}",
            "Workspace:".bold(),
            ws.to_string().cyan().bold()
        );
    } else if info.workspaces.as_ref().is_some_and(|w| w.len() > 1) {
        workspace::print_pick_hint();
    }
    println!(
        "  {} {}",
        "Transport:".bold(),
        info.transport.as_deref().unwrap_or("")
    );
    if let (Some(url), Some(n)) = (info.api_url.as_deref(), info.projects) {
        println!("  {} {}", "API:".bold(), url.dimmed());
        println!("  {} {n}", "Projects:".bold());
    }
}

#[cfg(test)]
mod tests {
    use super::{credential_kind, read_token_arg};
    use crate::api::{Credential, CredentialSource};
    use crate::config::{Config, TokenKind};

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

    #[test]
    fn credential_kind_follows_the_source() {
        let mut cfg = Config::default();
        cfg.auth.kind = Some(TokenKind::Oauth);
        let saved = Credential {
            token: "eyJ.x.y".to_string(),
            source: CredentialSource::Config,
        };
        assert_eq!(credential_kind(&saved, &cfg), Some("oauth"));
        // The env token wins, so the saved kind doesn't describe it
        let env = Credential {
            token: "ftk_ci".to_string(),
            source: CredentialSource::ApiTokenEnv,
        };
        assert_eq!(credential_kind(&env, &cfg), Some("api_token"));
        // A pre-browser-login config has no kind
        let legacy = Config::default();
        assert_eq!(credential_kind(&saved, &legacy), None);
    }
}
