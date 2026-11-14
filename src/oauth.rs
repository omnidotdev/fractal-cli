//! Browser login against Gatekeeper, the Omni identity provider.
//!
//! `fractal login` runs the standard browser login flow for a
//! command-line tool: OAuth 2.0 Authorization Code with PKCE (RFC 7636)
//! and a loopback redirect (RFC 8252). The CLI listens on
//! `http://127.0.0.1:<free port>/callback`, opens the system browser at
//! the authorization endpoint, and exchanges the returned code for an
//! access token, a refresh token, and an ID token. `fractal-cli` is a
//! public client, so no secret is involved: the PKCE verifier proves the
//! exchange comes from the process that started the login.
//!
//! Endpoints are read from the provider's discovery document, never
//! hardcoded. Gatekeeper ignores the port on a loopback redirect, so the
//! listener binds whatever port the OS hands out.
//!
//! Nothing in this module prints or logs a token.

use std::fmt;
use std::io::BufRead;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Production identity provider
pub const DEFAULT_AUTH_URL: &str = "https://identity.omni.dev";

/// Overrides the identity provider base URL (dev and staging)
pub const AUTH_URL_ENV: &str = "FRACTAL_AUTH_URL";

/// The `fractal-cli` client registered in Gatekeeper. A client id is
/// public, not a secret.
const BUILTIN_CLIENT_ID: &str = "c3f941d43b27b3bab4b6fc9bee4e6954";

/// Overrides the OAuth client id (dev against a local Gatekeeper)
pub const CLIENT_ID_ENV: &str = "FRACTAL_OAUTH_CLIENT_ID";

/// Scopes requested at login. `offline_access` yields the refresh token
/// and `organization` the workspace memberships claim.
pub const SCOPES: &str = "openid profile email offline_access organization";

/// Gatekeeper's namespaced claim listing the caller's workspaces. Access
/// and ID tokens carry `{ id }` entries; userinfo adds name and slug.
pub const ORGANIZATIONS_CLAIM: &str = "https://manifold.omni.dev/@omni/claims/organizations";

/// Shown when a saved browser login can no longer be refreshed
pub const SESSION_EXPIRED: &str = "Session expired, run `fractal login`";

/// Shown when the identity provider can't be reached to refresh
pub const REFRESH_UNREACHABLE: &str = "could not reach the identity provider to refresh your session; check your connection and try again";

/// Refresh an access token this many seconds before it expires
pub const REFRESH_SKEW_SECS: i64 = 60;

/// Path the loopback listener answers on
const CALLBACK_PATH: &str = "/callback";

/// Largest request head the loopback listener reads
const MAX_REQUEST_HEAD: usize = 16 * 1024;

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Identity provider base URL: `FRACTAL_AUTH_URL`, else production.
/// Trailing slashes are trimmed.
pub fn auth_url() -> String {
    non_empty_env(AUTH_URL_ENV)
        .unwrap_or_else(|| DEFAULT_AUTH_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// OAuth client id: `FRACTAL_OAUTH_CLIENT_ID`, else the built-in one
pub fn client_id() -> String {
    non_empty_env(CLIENT_ID_ENV).unwrap_or_else(|| BUILTIN_CLIENT_ID.to_string())
}

/// Current time in Unix seconds
pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Whether an access token expiring at `expires_at` should be refreshed
/// before use. An unknown expiry is left alone; a 401 still triggers a
/// refresh.
pub fn needs_refresh(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|exp| exp - now <= REFRESH_SKEW_SECS)
}

/// HTTP client for identity provider calls
///
/// # Errors
///
/// Returns an error when the client can't be constructed.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("fractal-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

/// The parts of the provider's discovery document the CLI uses
#[derive(Clone, Debug, Deserialize)]
pub struct Discovery {
    #[serde(default)]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
}

/// Fetch the discovery document, trying the OIDC location
/// first and the OAuth authorization server location second
///
/// # Errors
///
/// Returns an error when neither document can be fetched and decoded.
pub async fn discover(http: &reqwest::Client, base: &str) -> Result<Discovery> {
    let base = base.trim_end_matches('/');
    let mut last = String::new();
    for path in [
        "/.well-known/openid-configuration",
        "/.well-known/oauth-authorization-server",
    ] {
        match http.get(format!("{base}{path}")).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<Discovery>().await {
                Ok(doc) => return Ok(doc),
                Err(e) => last = format!("unexpected discovery document ({e})"),
            },
            Ok(resp) => last = format!("discovery returned {}", resp.status().as_u16()),
            Err(e) => last = e.to_string(),
        }
    }
    anyhow::bail!("could not read the identity provider configuration at {base}: {last}")
}

/// PKCE verifier and its S256 challenge
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh 256-bit verifier and its challenge
    pub fn generate() -> Self {
        let verifier = random_token();
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// 32 random bytes, base64url without padding (43 characters)
fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// S256 code challenge: base64url(sha256(verifier)) without padding
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random `state` that binds the redirect to this login attempt
pub fn new_state() -> String {
    random_token()
}

/// Redirect URI for a loopback listener on `port`
pub fn redirect_uri_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

/// The OIDC `prompt` sent on the authorization request
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// Always show the "Authorize Fractal CLI" page naming the signed-in
    /// account, instead of silently reusing the browser session. The
    /// standard flow for a command-line login, so the person sees which
    /// account the CLI is about to act as
    Consent,
    /// Force a fresh sign-in, then still show the consent page
    /// (`fractal login --switch-account`). `login` alone would let a
    /// trusted client receive a code without the consent page
    Login,
}

impl Prompt {
    /// The `prompt` parameter value (OIDC Core 3.1.2.1)
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Consent => "consent",
            Self::Login => "login consent",
        }
    }
}

/// Build the authorization URL the browser opens
///
/// # Errors
///
/// Returns an error when the authorization endpoint isn't a valid URL.
pub fn authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    prompt: Prompt,
) -> Result<Url> {
    Url::parse_with_params(
        endpoint,
        [
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPES),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("prompt", prompt.as_str()),
        ],
    )
    .context("the identity provider advertised an invalid authorization endpoint")
}

/// Query parameters on the redirect back from the provider
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub iss: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

impl CallbackParams {
    fn from_url(url: &Url) -> Self {
        let mut params = Self::default();
        for (key, value) in url.query_pairs() {
            let value = Some(value.into_owned());
            match key.as_ref() {
                "code" => params.code = value,
                "state" => params.state = value,
                "iss" => params.iss = value,
                "error" => params.error = value,
                "error_description" => params.error_description = value,
                _ => {}
            }
        }
        params
    }
}

/// Parse the redirect URL a user pasted from the browser's address bar.
/// A bare query string (`code=...&state=...`) is accepted too.
///
/// # Errors
///
/// Returns an error when the input is neither a URL nor a query string
/// carrying a `code` or an `error`.
pub fn parse_pasted(input: &str) -> Result<CallbackParams> {
    let trimmed = input.trim();
    let url = Url::parse(trimmed).or_else(|_| {
        let query = trimmed.trim_start_matches('?');
        Url::parse(&format!("http://127.0.0.1{CALLBACK_PATH}?{query}"))
    });
    let params = url.map(|u| CallbackParams::from_url(&u)).ok();
    match params {
        Some(p) if p.code.is_some() || p.error.is_some() => Ok(p),
        _ => anyhow::bail!(
            "that doesn't look like the sign-in redirect; paste the full URL from the browser's address bar"
        ),
    }
}

/// Parse a request target (`/callback?code=...`) received by the
/// loopback listener. `None` for any other path.
pub fn parse_request_target(target: &str) -> Option<CallbackParams> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    (url.path() == CALLBACK_PATH).then(|| CallbackParams::from_url(&url))
}

/// Why a redirect was not accepted
#[derive(Debug, PartialEq, Eq)]
pub enum CallbackError {
    /// The `state` doesn't match this login attempt, so the redirect
    /// may not come from it
    StateMismatch,
    /// The redirect names a different issuer (RFC 9207)
    IssuerMismatch,
    /// The provider reported an error, e.g. the user denied access
    Denied(String),
    /// No authorization code in the redirect
    MissingCode,
}

impl fmt::Display for CallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateMismatch => f.write_str(
                "the sign-in response doesn't match this login attempt (state mismatch)",
            ),
            Self::IssuerMismatch => {
                f.write_str("the sign-in response came from an unexpected identity provider")
            }
            Self::Denied(reason) => write!(f, "sign-in was not completed: {reason}"),
            Self::MissingCode => f.write_str("the sign-in response had no authorization code"),
        }
    }
}

impl std::error::Error for CallbackError {}

/// Validate a redirect against this login attempt and return its
/// authorization code. `state` is checked first, so a forged or stale
/// redirect is rejected before anything else in it is trusted.
///
/// # Errors
///
/// Returns a [`CallbackError`] when the redirect can't be accepted.
pub fn validate_callback(
    params: &CallbackParams,
    expected_state: &str,
    issuer: Option<&str>,
) -> std::result::Result<String, CallbackError> {
    if params.state.as_deref() != Some(expected_state) {
        return Err(CallbackError::StateMismatch);
    }
    if let Some(error) = &params.error {
        let reason = params
            .error_description
            .as_deref()
            .filter(|d| !d.is_empty())
            .map_or_else(|| error.clone(), |d| format!("{error} ({d})"));
        return Err(CallbackError::Denied(reason));
    }
    if let (Some(got), Some(want)) = (params.iss.as_deref(), issuer) {
        if got.trim_end_matches('/') != want.trim_end_matches('/') {
            return Err(CallbackError::IssuerMismatch);
        }
    }
    params
        .code
        .clone()
        .filter(|c| !c.is_empty())
        .ok_or(CallbackError::MissingCode)
}

/// Bind the loopback listener on a free port
///
/// # Errors
///
/// Returns an error when no loopback port can be bound.
pub async fn bind_loopback() -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not start the local sign-in listener on 127.0.0.1")
}

/// Read a request head and return its target when it is a `GET`
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_HEAD {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next()?.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    }
}

/// Page shown in the browser once the CLI has its tokens
const SUCCESS_PAGE: &str =
    "You're signed in to Fractal. You can close this tab and return to your terminal.";

/// Page shown when the sign-in didn't complete
const FAILURE_PAGE: &str = "Sign-in didn't complete. Return to your terminal for details.";

fn page(message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Fractal CLI</title>\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<style>body{{font-family:system-ui,sans-serif;display:flex;min-height:100vh;margin:0;\
align-items:center;justify-content:center;padding:0 16px;background:#fafafa;color:#111}}\
@media (prefers-color-scheme:dark){{body{{background:#111;color:#eee}}}}\
p{{font-size:1.1rem;max-width:32rem;text-align:center}}</style></head>\
<body><p>{message}</p></body></html>"
    )
}

/// Write a small HTML response and close the connection. Best effort:
/// the browser tab is cosmetic, the terminal carries the result.
async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = page(message);
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Tell the waiting browser tab how the login ended
pub async fn finish_browser(mut stream: TcpStream, ok: bool) {
    if ok {
        respond(&mut stream, "200 OK", SUCCESS_PAGE).await;
    } else {
        respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
    }
}

/// Wait on the loopback listener for the provider's redirect. Requests
/// to other paths get a 404 and a redirect with someone else's `state`
/// gets a 400; both keep the listener waiting. Returns the code plus
/// the open connection so the caller can answer the tab once the
/// exchange finishes.
///
/// # Errors
///
/// Returns an error when accepting fails or the provider reports an
/// error for this login attempt.
pub async fn await_redirect(
    listener: &TcpListener,
    expected_state: &str,
    issuer: Option<&str>,
) -> Result<(String, TcpStream)> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("the local sign-in listener failed")?;
        let target =
            tokio::time::timeout(Duration::from_secs(10), read_request_target(&mut stream)).await;
        let Ok(Some(target)) = target else {
            continue;
        };
        let Some(params) = parse_request_target(&target) else {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        };
        match validate_callback(&params, expected_state, issuer) {
            Ok(code) => return Ok((code, stream)),
            Err(CallbackError::StateMismatch) => {
                respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
            }
            Err(e) => {
                respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
                return Err(e.into());
            }
        }
    }
}

/// Read stdin lines on a detached thread so a pending read never holds
/// up process exit
pub fn spawn_line_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Whether to open a browser. Pure so the rules are testable: never
/// with `--no-browser` or over SSH, and on Linux and the BSDs only when
/// a display server is available.
pub fn should_open_browser(
    no_browser: bool,
    needs_display_server: bool,
    env: impl Fn(&str) -> Option<String>,
) -> bool {
    let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
    if no_browser || set("SSH_CONNECTION") || set("SSH_CLIENT") || set("SSH_TTY") {
        return false;
    }
    !needs_display_server || set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// Whether this platform needs X11 or Wayland to show a browser
pub const fn needs_display_server() -> bool {
    !cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Open `url` in the system browser. Returns false when the opener is
/// missing or fails.
pub fn open_browser(url: &str) -> bool {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // rundll32 takes the URL as one argument, so `&` in the query
        // isn't parsed by a shell
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Successful token endpoint response. Deliberately not `Debug`, so a
/// token can never end up in a log line.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Why a token request failed
#[derive(Debug)]
pub enum TokenError {
    /// The provider couldn't be reached
    Unreachable(String),
    /// The provider refused the grant (e.g. `invalid_grant`)
    Rejected {
        status: u16,
        error: Option<String>,
        description: Option<String>,
    },
    /// The response couldn't be decoded
    Malformed,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "could not reach the identity provider ({e})"),
            Self::Rejected {
                status,
                error,
                description,
            } => {
                write!(f, "the identity provider refused the request ({status}")?;
                if let Some(e) = error {
                    write!(f, ": {e}")?;
                }
                if let Some(d) = description.as_deref().filter(|d| !d.is_empty()) {
                    write!(f, ", {d}")?;
                }
                f.write_str(")")
            }
            Self::Malformed => f.write_str("unexpected response from the identity provider"),
        }
    }
}

impl std::error::Error for TokenError {}

#[derive(Deserialize)]
struct TokenErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

async fn post_token(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> std::result::Result<TokenResponse, TokenError> {
    let resp = http
        .post(endpoint)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|e| TokenError::Unreachable(e.without_url().to_string()))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if (200..300).contains(&status) {
        return serde_json::from_str(&body).map_err(|_| TokenError::Malformed);
    }
    let parsed: Option<TokenErrorBody> = serde_json::from_str(&body).ok();
    Err(TokenError::Rejected {
        status,
        error: parsed.as_ref().and_then(|p| p.error.clone()),
        description: parsed.and_then(|p| p.error_description),
    })
}

/// Exchange an authorization code for tokens (public client, PKCE)
///
/// # Errors
///
/// Returns a [`TokenError`] when the exchange fails.
pub async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> std::result::Result<TokenResponse, TokenError> {
    post_token(
        http,
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ],
    )
    .await
}

/// Trade a refresh token for a new access token
///
/// # Errors
///
/// Returns a [`TokenError`] when the refresh fails.
pub async fn refresh_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> std::result::Result<TokenResponse, TokenError> {
    post_token(
        http,
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
    .await
}

/// Revoke a refresh token (RFC 7009)
///
/// # Errors
///
/// Returns an error when the provider can't be reached or refuses.
pub async fn revoke_refresh_token(
    http: &reqwest::Client,
    revocation_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<()> {
    let resp = http
        .post(revocation_endpoint)
        .form(&[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("revocation returned {}", status.as_u16());
    }
    Ok(())
}

/// Fetch the caller's claims from the userinfo endpoint
///
/// # Errors
///
/// Returns an error when the request fails or the body isn't JSON.
pub async fn userinfo(http: &reqwest::Client, endpoint: &str, access_token: &str) -> Result<Value> {
    let resp = http
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("userinfo returned {}", status.as_u16());
    }
    resp.json().await.context("unexpected userinfo response")
}

/// Tokens to persist after a login or refresh
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

impl TokenSet {
    /// Build from a token response. A provider that doesn't rotate the
    /// refresh token keeps the previous one; expiry comes from
    /// `expires_in`, else the access token's `exp` claim.
    pub fn from_response(resp: TokenResponse, now: i64, previous_refresh: Option<String>) -> Self {
        let expires_at = resp.expires_in.map(|s| now + s).or_else(|| {
            decode_jwt_claims(&resp.access_token).and_then(|c| c.get("exp").and_then(Value::as_i64))
        });
        Self {
            refresh_token: resp
                .refresh_token
                .filter(|t| !t.is_empty())
                .or(previous_refresh),
            access_token: resp.access_token,
            expires_at,
        }
    }
}

/// Decode a JWT's payload without verifying it. Used only for display
/// and for picking a default workspace; the API verifies every token
/// it receives, and the ID token arrives straight from the token
/// endpoint over TLS (OIDC Core 3.1.3.7).
pub fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A string claim, when present and non-empty
pub fn claim_str(claims: &Value, key: &str) -> Option<String> {
    claims
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A workspace the caller belongs to. Also the shape `fractal workspace
/// use` saves to the config file
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

impl fmt::Display for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.name, &self.slug) {
            (Some(name), Some(slug)) => write!(f, "{name} ({slug})"),
            (Some(name), None) => f.write_str(name),
            (None, Some(slug)) => f.write_str(slug),
            (None, None) => f.write_str(&self.id),
        }
    }
}

/// Workspaces from the organizations claim. Off-shape entries are
/// skipped.
pub fn workspaces(claims: &Value) -> Vec<Workspace> {
    claims
        .get(ORGANIZATIONS_CLAIM)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = claim_str(entry, "id")?;
                    Some(Workspace {
                        id,
                        name: claim_str(entry, "name"),
                        slug: claim_str(entry, "slug"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_have_the_rfc_7636_shape() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        let unreserved = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        assert!(unreserved(&pkce.verifier));
        assert!(unreserved(&pkce.challenge));
        assert_eq!(pkce.challenge, challenge_for(&pkce.verifier));
        assert_ne!(Pkce::generate().verifier, pkce.verifier);
    }

    #[test]
    fn s256_challenge_matches_the_rfc_7636_test_vector() {
        // RFC 7636 appendix B
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn state_is_random_per_login() {
        assert_ne!(new_state(), new_state());
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_scopes() {
        let url = authorize_url(
            "https://id.example.test/oauth2/authorize",
            "cid",
            "http://127.0.0.1:5555/callback",
            "st",
            "ch",
            Prompt::Consent,
        )
        .unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/callback");
        assert_eq!(q["scope"], SCOPES);
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["prompt"], "consent");
    }

    #[test]
    fn authorize_url_forces_a_fresh_sign_in_to_switch_accounts() {
        let url = authorize_url(
            "https://id.example.test/oauth2/authorize",
            "cid",
            "http://127.0.0.1:5555/callback",
            "st",
            "ch",
            Prompt::Login,
        )
        .unwrap();
        let prompts: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "prompt")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(prompts, ["login consent"]);
        // Space-separated per OIDC, URL-encoded on the wire
        assert!(
            url.as_str().contains("prompt=login+consent")
                || url.as_str().contains("prompt=login%20consent"),
            "{url}"
        );
    }

    #[test]
    fn redirect_uri_is_loopback_ip_not_localhost() {
        assert_eq!(redirect_uri_for(8123), "http://127.0.0.1:8123/callback");
    }

    fn params(code: Option<&str>, state: Option<&str>) -> CallbackParams {
        CallbackParams {
            code: code.map(str::to_string),
            state: state.map(str::to_string),
            ..CallbackParams::default()
        }
    }

    #[test]
    fn matching_state_yields_the_code() {
        assert_eq!(
            validate_callback(&params(Some("c1"), Some("s1")), "s1", None),
            Ok("c1".to_string())
        );
    }

    #[test]
    fn state_mismatch_is_rejected() {
        assert_eq!(
            validate_callback(&params(Some("c1"), Some("other")), "s1", None),
            Err(CallbackError::StateMismatch)
        );
        assert_eq!(
            validate_callback(&params(Some("c1"), None), "s1", None),
            Err(CallbackError::StateMismatch)
        );
    }

    #[test]
    fn state_is_checked_before_a_provider_error() {
        let p = CallbackParams {
            error: Some("access_denied".to_string()),
            state: Some("forged".to_string()),
            ..CallbackParams::default()
        };
        assert_eq!(
            validate_callback(&p, "s1", None),
            Err(CallbackError::StateMismatch)
        );
    }

    #[test]
    fn provider_error_and_missing_code_are_reported() {
        let p = CallbackParams {
            error: Some("access_denied".to_string()),
            error_description: Some("User denied access".to_string()),
            state: Some("s1".to_string()),
            ..CallbackParams::default()
        };
        assert_eq!(
            validate_callback(&p, "s1", None),
            Err(CallbackError::Denied(
                "access_denied (User denied access)".to_string()
            ))
        );
        assert_eq!(
            validate_callback(&params(None, Some("s1")), "s1", None),
            Err(CallbackError::MissingCode)
        );
    }

    #[test]
    fn issuer_mismatch_is_rejected_when_both_sides_name_one() {
        let mut p = params(Some("c1"), Some("s1"));
        p.iss = Some("https://evil.example.test".to_string());
        assert_eq!(
            validate_callback(&p, "s1", Some("https://identity.omni.dev")),
            Err(CallbackError::IssuerMismatch)
        );
        p.iss = Some("https://identity.omni.dev/".to_string());
        assert_eq!(
            validate_callback(&p, "s1", Some("https://identity.omni.dev")),
            Ok("c1".to_string())
        );
    }

    #[test]
    fn pasted_redirect_url_is_parsed() {
        let p = parse_pasted(
            "  http://127.0.0.1:41234/callback?code=abc%2Fdef&state=xyz&iss=https%3A%2F%2Fidentity.omni.dev\n",
        )
        .unwrap();
        assert_eq!(p.code.as_deref(), Some("abc/def"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
        assert_eq!(p.iss.as_deref(), Some("https://identity.omni.dev"));
    }

    #[test]
    fn pasted_bare_query_is_parsed() {
        let p = parse_pasted("?code=abc&state=xyz").unwrap();
        assert_eq!(p.code.as_deref(), Some("abc"));
        let p = parse_pasted("code=abc&state=xyz").unwrap();
        assert_eq!(p.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn pasted_error_redirect_is_parsed() {
        let p = parse_pasted("http://127.0.0.1:1/callback?error=access_denied&state=s").unwrap();
        assert_eq!(p.error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn pasted_garbage_is_rejected() {
        assert!(parse_pasted("").is_err());
        assert!(parse_pasted("hello there").is_err());
        assert!(parse_pasted("https://identity.omni.dev/sign-in").is_err());
    }

    #[test]
    fn request_target_only_matches_the_callback_path() {
        let p = parse_request_target("/callback?code=c&state=s").unwrap();
        assert_eq!(p.code.as_deref(), Some("c"));
        assert!(parse_request_target("/favicon.ico").is_none());
        assert!(parse_request_target("/callbackx?code=c").is_none());
    }

    #[test]
    fn refresh_decision_uses_a_sixty_second_skew() {
        let now = 1_000_000;
        assert!(!needs_refresh(None, now));
        assert!(!needs_refresh(Some(now + 3600), now));
        assert!(!needs_refresh(Some(now + 61), now));
        assert!(needs_refresh(Some(now + 60), now));
        assert!(needs_refresh(Some(now + 5), now));
        assert!(needs_refresh(Some(now - 10), now));
    }

    #[test]
    fn browser_is_skipped_over_ssh_or_without_a_display() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert!(should_open_browser(false, true, env(&[("DISPLAY", ":0")])));
        assert!(should_open_browser(
            false,
            true,
            env(&[("WAYLAND_DISPLAY", "wayland-0")])
        ));
        assert!(!should_open_browser(false, true, env(&[])));
        assert!(should_open_browser(false, false, env(&[])));
        assert!(!should_open_browser(true, false, env(&[("DISPLAY", ":0")])));
        assert!(!should_open_browser(
            false,
            false,
            env(&[("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")])
        ));
        assert!(!should_open_browser(
            false,
            true,
            env(&[("DISPLAY", ":0"), ("SSH_TTY", "/dev/pts/1")])
        ));
    }

    fn jwt(payload: &Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap())
        )
    }

    #[test]
    fn jwt_claims_decode_and_workspaces_extract() {
        let token = jwt(&serde_json::json!({
            "sub": "u1",
            "email": "ada@example.test",
            ORGANIZATIONS_CLAIM: [{ "id": "org_a" }, { "nope": 1 }, { "id": "org_b", "name": "Beta", "slug": "beta" }],
        }));
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(
            claim_str(&claims, "email").as_deref(),
            Some("ada@example.test")
        );
        let ws = workspaces(&claims);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].id, "org_a");
        assert_eq!(ws[1].to_string(), "Beta (beta)");
        assert!(decode_jwt_claims("ftk_not_a_jwt").is_none());
        assert!(workspaces(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn token_set_keeps_the_old_refresh_token_when_not_rotated() {
        let resp = TokenResponse {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_in: Some(3600),
            id_token: None,
        };
        let set = TokenSet::from_response(resp, 100, Some("old".to_string()));
        assert_eq!(set.refresh_token.as_deref(), Some("old"));
        assert_eq!(set.expires_at, Some(3700));
    }

    #[test]
    fn token_set_falls_back_to_the_jwt_exp_claim() {
        let resp = TokenResponse {
            access_token: jwt(&serde_json::json!({ "exp": 4242 })),
            refresh_token: Some("new".to_string()),
            expires_in: None,
            id_token: None,
        };
        let set = TokenSet::from_response(resp, 100, Some("old".to_string()));
        assert_eq!(set.refresh_token.as_deref(), Some("new"));
        assert_eq!(set.expires_at, Some(4242));
    }

    #[tokio::test]
    async fn loopback_listener_ignores_strays_and_returns_the_code() {
        let listener = bind_loopback().await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move {
            await_redirect(&listener, "good", None).await.map(|(c, s)| {
                drop(s);
                c
            })
        });

        let get = move |path: &'static str| async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out).await;
            out
        };
        assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let forged = get("/callback?code=x&state=bad").await;
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        assert!(!forged.contains("state"), "page must not leak details");

        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /callback?code=real&state=good HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(waiter.await.unwrap().unwrap(), "real");
    }

    #[tokio::test]
    async fn provider_error_on_the_listener_ends_the_wait() {
        let listener = bind_loopback().await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move {
            await_redirect(&listener, "good", None)
                .await
                .map(|(c, _)| c)
        });
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /callback?error=access_denied&state=good HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn success_page_has_no_internal_details() {
        let html = page(SUCCESS_PAGE);
        assert!(html.contains("return to your terminal"));
        for leak in ["token", "code", "127.0.0.1", "gatekeeper"] {
            assert!(!html.to_lowercase().contains(leak), "{leak}");
        }
    }

    #[test]
    fn token_errors_never_echo_the_request() {
        let e = TokenError::Rejected {
            status: 400,
            error: Some("invalid_grant".to_string()),
            description: Some("invalid refresh token".to_string()),
        };
        assert_eq!(
            e.to_string(),
            "the identity provider refused the request (400: invalid_grant, invalid refresh token)"
        );
    }

    #[test]
    fn client_id_defaults_to_the_registered_client() {
        assert_eq!(BUILTIN_CLIENT_ID.len(), 32);
        assert!(BUILTIN_CLIENT_ID.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
