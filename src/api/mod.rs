//! Hosted transport: a small client for the Fractal API. GraphQL
//! (`<api>/graphql`) covers projects, services, env, scale, and API
//! tokens; REST endpoints under `/v1` cover deploys, logs, and project
//! members. Every request carries the caller's bearer token, either a
//! Fractal API token (`ftk_...`) or the access token from `fractal
//! login`, which is refreshed silently before it expires.

pub mod model;
pub mod resolve;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::config::{AuthConfig, Config, config_path};
use crate::oauth::{self, TokenSet};

/// Production API base URL
pub const DEFAULT_API_URL: &str = "https://api.fractal.omni.dev";

/// Preferred env var for an API token
pub const TOKEN_ENV: &str = "FRACTAL_API_TOKEN";

/// Older env var, still honored so existing CI keeps working
pub const LEGACY_TOKEN_ENV: &str = "FRACTAL_AUTH_TOKEN";

/// Where the active credential came from
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// `FRACTAL_API_TOKEN`
    ApiTokenEnv,
    /// `FRACTAL_AUTH_TOKEN`
    AuthTokenEnv,
    /// `~/.config/fractal/config.toml`
    Config,
}

impl CredentialSource {
    /// Short label for `whoami` output
    pub const fn label(self) -> &'static str {
        match self {
            Self::ApiTokenEnv => TOKEN_ENV,
            Self::AuthTokenEnv => LEGACY_TOKEN_ENV,
            Self::Config => "config",
        }
    }

    /// Stable machine value for `whoami --json`
    pub const fn json_value(self) -> &'static str {
        match self {
            Self::ApiTokenEnv | Self::AuthTokenEnv => "env",
            Self::Config => "config",
        }
    }
}

/// A bearer credential and where it came from
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub token: String,
    pub source: CredentialSource,
}

fn non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// Pick the credential: `FRACTAL_API_TOKEN`, then `FRACTAL_AUTH_TOKEN`,
/// then the persisted token. Empty values count as unset.
pub fn select_credential(
    api_env: Option<&str>,
    auth_env: Option<&str>,
    config_token: Option<&str>,
) -> Option<Credential> {
    let pick = |v: Option<&str>, source| {
        non_empty(v).map(|t| Credential {
            token: t.to_string(),
            source,
        })
    };
    pick(api_env, CredentialSource::ApiTokenEnv)
        .or_else(|| pick(auth_env, CredentialSource::AuthTokenEnv))
        .or_else(|| pick(config_token, CredentialSource::Config))
}

/// Resolve the credential from the environment and an already-loaded
/// config
pub fn credential_from(cfg: &Config) -> Option<Credential> {
    select_credential(
        std::env::var(TOKEN_ENV).ok().as_deref(),
        std::env::var(LEGACY_TOKEN_ENV).ok().as_deref(),
        cfg.auth.access_token.as_deref(),
    )
}

/// Resolve the credential from the environment and the config on disk
pub fn credential() -> Option<Credential> {
    let cfg = Config::load().unwrap_or_default();
    credential_from(&cfg)
}

/// Pick the API base URL: `FRACTAL_API_URL`, then config, then the
/// production default. Trailing slashes are trimmed.
pub fn select_api_url(env: Option<&str>, config: Option<&str>) -> String {
    non_empty(env)
        .or_else(|| non_empty(config))
        .unwrap_or(DEFAULT_API_URL)
        .trim_end_matches('/')
        .to_string()
}

/// Resolve the API base URL from the environment and config
pub fn api_url() -> String {
    let cfg = Config::load().unwrap_or_default();
    select_api_url(
        std::env::var("FRACTAL_API_URL").ok().as_deref(),
        cfg.api_url.as_deref(),
    )
}

/// Message shown when no credential is configured
pub const NOT_AUTHENTICATED: &str = "Not authenticated. Run `fractal login`, or set FRACTAL_API_TOKEN to a \
     Fractal API token (see https://docs.omni.dev/products/fractal/api-tokens).";

/// Message shown when the API rejects the credential
const REJECTED: &str = "The Fractal API rejected your credential (it may be invalid, expired, or \
     revoked). Run `fractal login`, or create a new API token and set FRACTAL_API_TOKEN.";

/// The refresh token for a credential that can be refreshed: a browser
/// login saved in config. An env token always wins and is never
/// refreshed, and a saved API token has no refresh token.
pub fn refreshable(cred: &Credential, auth: &AuthConfig) -> Option<String> {
    if cred.source != CredentialSource::Config || !auth.is_oauth() {
        return None;
    }
    auth.refresh_token
        .as_deref()
        .and_then(|t| non_empty(Some(t)))
        .map(str::to_string)
}

/// How to refresh a browser-login credential, and where to save the
/// new tokens
pub struct Session {
    auth_base: String,
    client_id: String,
    persist_to: Option<PathBuf>,
}

impl Session {
    /// A session against an explicit identity provider. `persist_to`
    /// is the config file refreshed tokens are written back to.
    pub const fn new(auth_base: String, client_id: String, persist_to: Option<PathBuf>) -> Self {
        Self {
            auth_base,
            client_id,
            persist_to,
        }
    }

    /// Trade `refresh_token` for new tokens and persist them
    ///
    /// # Errors
    ///
    /// Returns [`oauth::SESSION_EXPIRED`] when the provider refuses the
    /// refresh token, or [`oauth::REFRESH_UNREACHABLE`] when it can't be
    /// reached.
    pub async fn refresh(&self, http: &reqwest::Client, refresh_token: &str) -> Result<TokenSet> {
        let discovery = oauth::discover(http, &self.auth_base)
            .await
            .map_err(|_| anyhow::anyhow!(oauth::REFRESH_UNREACHABLE))?;
        let resp = oauth::refresh_tokens(
            http,
            &discovery.token_endpoint,
            &self.client_id,
            refresh_token,
        )
        .await
        .map_err(|e| match e {
            oauth::TokenError::Unreachable(_) => anyhow::anyhow!(oauth::REFRESH_UNREACHABLE),
            _ => anyhow::anyhow!(oauth::SESSION_EXPIRED),
        })?;
        let set = TokenSet::from_response(resp, oauth::unix_now(), Some(refresh_token.to_string()));
        if let Some(path) = &self.persist_to {
            persist_tokens(path, &set)?;
        }
        Ok(set)
    }
}

/// Write refreshed tokens back to the config, unless the saved login
/// was replaced or cleared in the meantime
fn persist_tokens(path: &Path, set: &TokenSet) -> Result<()> {
    let mut cfg = Config::load_from(path)?;
    if !cfg.auth.is_oauth() {
        return Ok(());
    }
    cfg.auth.access_token = Some(set.access_token.clone());
    cfg.auth.refresh_token.clone_from(&set.refresh_token);
    cfg.auth.expires_at = set.expires_at;
    cfg.save_to(path)
}

/// The bearer a client sends, plus what it needs to renew it
struct Bearer {
    access: String,
    refresh: Option<String>,
    expires_at: Option<i64>,
}

/// Authenticated client for the Fractal API. A browser login is
/// refreshed shortly before it expires, and once more when the API
/// answers 401.
pub struct ApiClient {
    base: String,
    http: reqwest::Client,
    bearer: Mutex<Bearer>,
    session: Option<Session>,
}

impl ApiClient {
    /// Build a client from the resolved URL and credential
    ///
    /// # Errors
    ///
    /// Returns an error when no credential is configured.
    pub fn from_env() -> Result<Self> {
        let path = config_path().ok();
        let cfg = path
            .as_deref()
            .and_then(|p| Config::load_from(p).ok())
            .unwrap_or_default();
        let cred = credential_from(&cfg).context(NOT_AUTHENTICATED)?;
        let client = Self::new(&api_url(), &cred.token)?;
        Ok(match refreshable(&cred, &cfg.auth) {
            Some(refresh) => client.with_session(
                refresh,
                cfg.auth.expires_at,
                Session::new(oauth::auth_url(), oauth::client_id(), path),
            ),
            None => client,
        })
    }

    /// Build a client against an explicit base URL and token
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client can't be constructed.
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("fractal-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            bearer: Mutex::new(Bearer {
                access: token.to_string(),
                refresh: None,
                expires_at: None,
            }),
            session: None,
        })
    }

    /// Make the credential refreshable
    pub fn with_session(
        mut self,
        refresh_token: String,
        expires_at: Option<i64>,
        session: Session,
    ) -> Self {
        let bearer = self.bearer.get_mut();
        bearer.refresh = Some(refresh_token);
        bearer.expires_at = expires_at;
        self.session = Some(session);
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The bearer to send, refreshed first when it expires within a
    /// minute
    ///
    /// # Errors
    ///
    /// Returns an error when a needed refresh fails.
    pub async fn bearer_token(&self) -> Result<String> {
        let mut bearer = self.bearer.lock().await;
        if let Some(session) = &self.session {
            if oauth::needs_refresh(bearer.expires_at, oauth::unix_now()) {
                self.refresh_locked(&mut bearer, session).await?;
            }
        }
        Ok(bearer.access.clone())
    }

    /// Workspace ids carried by the bearer's organizations claim. Empty
    /// for an API token, which isn't a JWT.
    pub async fn token_workspace_ids(&self) -> Vec<String> {
        let bearer = self.bearer.lock().await;
        oauth::decode_jwt_claims(&bearer.access)
            .map(|claims| {
                oauth::workspaces(&claims)
                    .into_iter()
                    .map(|w| w.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// After the API rejected `rejected`, refresh once. Returns the
    /// token to retry with, or `None` when the credential can't be
    /// refreshed.
    async fn refresh_after_rejection(&self, rejected: &str) -> Result<Option<String>> {
        let Some(session) = &self.session else {
            return Ok(None);
        };
        let mut bearer = self.bearer.lock().await;
        // A concurrent request may already have refreshed
        if bearer.access == rejected {
            self.refresh_locked(&mut bearer, session).await?;
        }
        Ok(Some(bearer.access.clone()))
    }

    async fn refresh_locked(&self, bearer: &mut Bearer, session: &Session) -> Result<()> {
        let refresh = bearer
            .refresh
            .clone()
            .ok_or_else(|| anyhow::anyhow!(oauth::SESSION_EXPIRED))?;
        let set = session.refresh(&self.http, &refresh).await?;
        bearer.access = set.access_token;
        bearer.refresh = set.refresh_token;
        bearer.expires_at = set.expires_at;
        Ok(())
    }

    /// Send a request with the current bearer. On a 401 from a
    /// refreshable credential, refresh once and retry once.
    async fn execute<F>(&self, build: F) -> Result<(u16, String)>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let token = self.bearer_token().await?;
        let (status, body) = dispatch(build(&self.http, &token)).await?;
        if is_unauthorized(status, &body) {
            if let Some(fresh) = self.refresh_after_rejection(&token).await? {
                return dispatch(build(&self.http, &fresh)).await;
            }
        }
        Ok((status, body))
    }

    /// Run a GraphQL operation and decode its `data`
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure, a non-2xx status, or any
    /// GraphQL error in the response.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let url = self.url("/graphql");
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let (status, body) = self
            .execute(|http, token| http.post(&url).bearer_auth(token).json(&payload))
            .await?;
        parse_graphql(status, &body)
    }

    /// `GET` a REST endpoint with query parameters and decode the JSON
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or a non-2xx status.
    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path);
        let (status, body) = self
            .execute(|http, token| http.get(&url).bearer_auth(token).query(query))
            .await?;
        parse_rest(status, &body)
    }

    /// Send a JSON body to a REST endpoint and decode the JSON reply
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or a non-2xx status.
    pub async fn send<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        let (status, text) = self
            .execute(|http, token| {
                http.request(method.clone(), &url)
                    .bearer_auth(token)
                    .json(body)
            })
            .await?;
        parse_rest(status, &text)
    }
}

async fn dispatch(req: reqwest::RequestBuilder) -> Result<(u16, String)> {
    let resp = req.send().await.map_err(|e| unreachable_error(&e))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

/// Whether a response means the bearer was rejected: a 401, or a
/// GraphQL `UNAUTHENTICATED` error
fn is_unauthorized(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if !body.contains("UNAUTHENTICATED") {
        return false;
    }
    serde_json::from_str::<GraphqlEnvelope<serde_json::Value>>(body).is_ok_and(|env| {
        env.errors
            .iter()
            .any(|e| e.code() == Some("UNAUTHENTICATED"))
    })
}

fn unreachable_error(err: &reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::anyhow!("the Fractal API did not respond in time; try again shortly")
    } else {
        anyhow::anyhow!("could not reach the Fractal API ({err})")
    }
}

/// Pull a human message out of an error body: `{ "error": "..." }`,
/// `{ "message": "..." }`, or plain text
fn error_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for key in ["error", "message"] {
            if let Some(s) = v.get(key).and_then(serde_json::Value::as_str) {
                return Some(s.to_string());
            }
        }
        return None;
    }
    // Plain-text bodies are short framework messages; cap them so an HTML
    // error page never floods the terminal
    if trimmed.starts_with('<') {
        return None;
    }
    Some(trimmed.chars().take(300).collect())
}

/// Turn a non-2xx response into a user-facing error
fn http_error(status: u16, body: &str) -> anyhow::Error {
    let msg = error_message(body);
    match status {
        401 => anyhow::anyhow!(REJECTED),
        403 => anyhow::anyhow!(
            "permission denied{}",
            msg.map_or_else(String::new, |m| format!(": {m}"))
        ),
        404 => anyhow::anyhow!("{}", msg.unwrap_or_else(|| "not found".to_string())),
        400 | 422 => anyhow::anyhow!(
            "invalid request{}",
            msg.map_or_else(String::new, |m| format!(": {m}"))
        ),
        500..=599 => anyhow::anyhow!(
            "the Fractal API returned an error ({status}){}; try again shortly",
            msg.map_or_else(String::new, |m| format!(": {m}"))
        ),
        _ => anyhow::anyhow!(
            "request failed ({status}){}",
            msg.map_or_else(String::new, |m| format!(": {m}"))
        ),
    }
}

/// Decode a REST response
///
/// # Errors
///
/// Returns an error for a non-2xx status or an undecodable body.
pub fn parse_rest<T: DeserializeOwned>(status: u16, body: &str) -> Result<T> {
    if !(200..300).contains(&status) {
        return Err(http_error(status, body));
    }
    let body = if body.trim().is_empty() { "null" } else { body };
    serde_json::from_str(body).context("unexpected response from the Fractal API")
}

#[derive(serde::Deserialize)]
struct GraphqlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(serde::Deserialize)]
struct GraphqlError {
    message: String,
    #[serde(default)]
    extensions: Option<serde_json::Value>,
}

impl GraphqlError {
    fn code(&self) -> Option<&str> {
        self.extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(serde_json::Value::as_str)
    }
}

/// Decode a GraphQL response, surfacing GraphQL errors as one message
///
/// # Errors
///
/// Returns an error for a non-2xx status, any GraphQL error, or a
/// response with no `data`.
pub fn parse_graphql<T: DeserializeOwned>(status: u16, body: &str) -> Result<T> {
    if status == 401 {
        return Err(http_error(status, body));
    }
    let envelope: GraphqlEnvelope<T> = match serde_json::from_str(body) {
        Ok(env) => env,
        Err(_) if !(200..300).contains(&status) => return Err(http_error(status, body)),
        Err(e) => {
            return Err(anyhow::anyhow!(e).context("unexpected response from the Fractal API"));
        }
    };
    if !envelope.errors.is_empty() {
        if envelope
            .errors
            .iter()
            .any(|e| e.code() == Some("UNAUTHENTICATED"))
        {
            anyhow::bail!(REJECTED);
        }
        let joined = envelope
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("{joined}");
    }
    if !(200..300).contains(&status) {
        return Err(http_error(status, body));
    }
    envelope.data.context("the Fractal API returned no data")
}

#[cfg(test)]
pub mod test_server {
    //! One-shot HTTP stub for client tests. Accepts a single connection,
    //! records the raw request, and replies with a canned status + body.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Start the stub and return its base URL plus a receiver for the
    /// raw request text
    pub async fn serve(status: u16, body: &'static str) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(idx) = text.find("\r\n\r\n") {
                    let len = text[..idx]
                        .lines()
                        .find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= idx + 4 + len {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
        });
        (format!("http://{addr}"), rx)
    }

    /// Read one request (head plus `content-length` body)
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(idx) = text.find("\r\n\r\n") {
                let len = text[..idx]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= idx + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// Serve a scripted sequence of replies, one connection each, and
    /// return every raw request. `script` receives the stub's base URL
    /// so replies (e.g. a discovery document) can point back at it.
    pub async fn serve_script(
        script: impl FnOnce(&str) -> Vec<(u16, String)>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let replies = script(&base);
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                seen.push(read_request(&mut sock).await);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
            }
            let _ = tx.send(seen);
        });
        (base, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_token_env_wins_over_legacy_env_and_config() {
        let c = select_credential(Some("ftk_a"), Some("legacy"), Some("cfg")).unwrap();
        assert_eq!(c.token, "ftk_a");
        assert_eq!(c.source, CredentialSource::ApiTokenEnv);
    }

    #[test]
    fn legacy_env_wins_over_config() {
        let c = select_credential(None, Some("legacy"), Some("cfg")).unwrap();
        assert_eq!(c.token, "legacy");
        assert_eq!(c.source, CredentialSource::AuthTokenEnv);
    }

    #[test]
    fn config_token_used_last_and_blank_env_ignored() {
        let c = select_credential(Some(""), Some("  "), Some("cfg")).unwrap();
        assert_eq!(c.token, "cfg");
        assert_eq!(c.source, CredentialSource::Config);
        assert!(select_credential(None, None, None).is_none());
        assert!(select_credential(None, None, Some("")).is_none());
    }

    #[test]
    fn api_url_precedence_and_trimming() {
        assert_eq!(
            select_api_url(Some("http://localhost:4000/"), Some("https://cfg")),
            "http://localhost:4000"
        );
        assert_eq!(select_api_url(None, Some("https://cfg/")), "https://cfg");
        assert_eq!(select_api_url(Some(""), None), DEFAULT_API_URL);
        assert_eq!(select_api_url(None, None), "https://api.fractal.omni.dev");
    }

    #[derive(serde::Deserialize, Debug)]
    struct Data {
        ok: bool,
    }

    #[test]
    fn graphql_data_decodes() {
        let d: Data = parse_graphql(200, r#"{"data":{"ok":true}}"#).unwrap();
        assert!(d.ok);
    }

    #[test]
    fn graphql_errors_are_joined() {
        let err = parse_graphql::<Data>(
            200,
            r#"{"data":null,"errors":[{"message":"a"},{"message":"b"}]}"#,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "a; b");
    }

    #[test]
    fn graphql_unauthenticated_maps_to_rejected() {
        let err = parse_graphql::<Data>(
            200,
            r#"{"errors":[{"message":"Authentication required","extensions":{"code":"UNAUTHENTICATED"}}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("rejected your credential"));
        let err = parse_graphql::<Data>(401, "invalid token").unwrap_err();
        assert!(err.to_string().contains("rejected your credential"));
    }

    #[test]
    fn rest_errors_surface_server_message() {
        let err = parse_rest::<Data>(403, r#"{"error":"requires developer role"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: requires developer role"
        );
        let err = parse_rest::<Data>(404, r#"{"error":"FractalService x not found"}"#).unwrap_err();
        assert_eq!(err.to_string(), "FractalService x not found");
        let err = parse_rest::<Data>(400, r#"{"error":"tailLines must be positive"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid request: tailLines must be positive"
        );
        let err = parse_rest::<Data>(502, "<html>bad gateway</html>").unwrap_err();
        assert_eq!(
            err.to_string(),
            "the Fractal API returned an error (502); try again shortly"
        );
    }

    #[test]
    fn rest_success_decodes() {
        let d: Data = parse_rest(200, r#"{"ok":true}"#).unwrap();
        assert!(d.ok);
    }

    #[tokio::test]
    async fn client_sends_bearer_to_graphql_endpoint() {
        let (base, rx) = test_server::serve(200, r#"{"data":{"ok":true}}"#).await;
        let client = ApiClient::new(&format!("{base}/"), "ftk_test").unwrap();
        let d: Data = client
            .graphql("query { ok }", serde_json::json!({ "x": 1 }))
            .await
            .unwrap();
        assert!(d.ok);
        let req = rx.await.unwrap();
        assert!(req.starts_with("POST /graphql HTTP/1.1"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: bearer ftk_test")
        );
        assert!(req.contains(r#""variables":{"x":1}"#), "{req}");
    }

    #[tokio::test]
    async fn client_encodes_rest_query_params() {
        let (base, rx) = test_server::serve(200, r#"{"ok":true}"#).await;
        let client = ApiClient::new(&base, "t").unwrap();
        let _: Data = client
            .get(
                "/v1/services/logs",
                &[
                    ("namespace", "fractal-demo".to_string()),
                    ("tailLines", "5".to_string()),
                ],
            )
            .await
            .unwrap();
        let req = rx.await.unwrap();
        assert!(
            req.starts_with("GET /v1/services/logs?namespace=fractal-demo&tailLines=5 HTTP/1.1"),
            "{req}"
        );
    }
    fn discovery(base: &str) -> String {
        serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth2/authorize"),
            "token_endpoint": format!("{base}/oauth2/token"),
        })
        .to_string()
    }

    fn oauth_config(path: &std::path::Path, access: &str, refresh: &str) {
        let cfg = Config {
            auth: AuthConfig {
                access_token: Some(access.to_string()),
                kind: Some(crate::config::TokenKind::Oauth),
                refresh_token: Some(refresh.to_string()),
                expires_at: Some(1),
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        cfg.save_to(path).unwrap();
    }

    fn session_client(base: &str, expires_at: Option<i64>, path: &std::path::Path) -> ApiClient {
        ApiClient::new(base, "old_access").unwrap().with_session(
            "old_refresh".to_string(),
            expires_at,
            Session::new(
                base.to_string(),
                "cid".to_string(),
                Some(path.to_path_buf()),
            ),
        )
    }

    fn bearer_of(req: &str) -> Option<String> {
        req.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("authorization: bearer ")
                .map(str::to_string)
        })
    }

    #[tokio::test]
    async fn a_401_refreshes_once_retries_once_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        oauth_config(&path, "old_access", "old_refresh");
        let (base, rx) = test_server::serve_script(|base| {
            vec![
                (401, "invalid token".to_string()),
                (200, discovery(base)),
                (
                    200,
                    r#"{"access_token":"new_access","refresh_token":"new_refresh","expires_in":3600}"#
                        .to_string(),
                ),
                (200, r#"{"data":{"ok":true}}"#.to_string()),
            ]
        })
        .await;
        let client = session_client(&base, None, &path);
        let d: Data = client
            .graphql("query { ok }", serde_json::json!({}))
            .await
            .unwrap();
        assert!(d.ok);

        let reqs = rx.await.unwrap();
        assert!(reqs[0].starts_with("POST /graphql"), "{}", reqs[0]);
        assert_eq!(bearer_of(&reqs[0]).as_deref(), Some("old_access"));
        assert!(reqs[1].starts_with("GET /.well-known/openid-configuration"));
        assert!(reqs[2].starts_with("POST /oauth2/token"));
        assert!(reqs[2].contains("grant_type=refresh_token"), "{}", reqs[2]);
        assert!(reqs[2].contains("refresh_token=old_refresh"));
        assert!(reqs[2].contains("client_id=cid"));
        assert!(reqs[3].starts_with("POST /graphql"));
        assert_eq!(bearer_of(&reqs[3]).as_deref(), Some("new_access"));

        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.auth.access_token.as_deref(), Some("new_access"));
        assert_eq!(saved.auth.refresh_token.as_deref(), Some("new_refresh"));
        assert!(saved.auth.expires_at.unwrap() > oauth::unix_now());
    }

    #[tokio::test]
    async fn a_token_near_expiry_is_refreshed_before_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        oauth_config(&path, "old_access", "old_refresh");
        let (base, rx) = test_server::serve_script(|base| {
            vec![
                (200, discovery(base)),
                (
                    200,
                    r#"{"access_token":"new_access","expires_in":3600}"#.to_string(),
                ),
                (200, r#"{"ok":true}"#.to_string()),
            ]
        })
        .await;
        let client = session_client(&base, Some(oauth::unix_now() + 30), &path);
        let _: Data = client.get("/v1/x", &[]).await.unwrap();

        let reqs = rx.await.unwrap();
        assert!(reqs[1].starts_with("POST /oauth2/token"));
        assert!(reqs[2].starts_with("GET /v1/x"));
        assert_eq!(bearer_of(&reqs[2]).as_deref(), Some("new_access"));
        // The provider didn't rotate, so the old refresh token is kept
        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.auth.refresh_token.as_deref(), Some("old_refresh"));
    }

    #[tokio::test]
    async fn a_fresh_token_is_used_without_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (base, rx) =
            test_server::serve_script(|_| vec![(200, r#"{"ok":true}"#.to_string())]).await;
        let client = session_client(&base, Some(oauth::unix_now() + 3000), &path);
        let _: Data = client.get("/v1/x", &[]).await.unwrap();
        let reqs = rx.await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(bearer_of(&reqs[0]).as_deref(), Some("old_access"));
    }

    #[tokio::test]
    async fn a_refused_refresh_reports_session_expired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        oauth_config(&path, "old_access", "old_refresh");
        let (base, _rx) = test_server::serve_script(|base| {
            vec![
                (
                    200,
                    r#"{"errors":[{"message":"Authentication required","extensions":{"code":"UNAUTHENTICATED"}}]}"#
                        .to_string(),
                ),
                (200, discovery(base)),
                (
                    400,
                    r#"{"error":"invalid_grant","error_description":"invalid refresh token"}"#
                        .to_string(),
                ),
            ]
        })
        .await;
        let client = session_client(&base, None, &path);
        let err = client
            .graphql::<Data>("query { ok }", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), oauth::SESSION_EXPIRED);
        // The saved login is left in place
        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.auth.access_token.as_deref(), Some("old_access"));
    }

    #[tokio::test]
    async fn an_api_token_401_is_not_retried() {
        let (base, rx) =
            test_server::serve_script(|_| vec![(401, "invalid token".to_string())]).await;
        let client = ApiClient::new(&base, "ftk_x").unwrap();
        let err = client.get::<Data>("/v1/x", &[]).await.unwrap_err();
        assert!(err.to_string().contains("rejected your credential"));
        assert!(err.to_string().contains("fractal login"));
        assert_eq!(rx.await.unwrap().len(), 1);
    }

    fn cred(source: CredentialSource) -> Credential {
        Credential {
            token: "t".to_string(),
            source,
        }
    }

    #[test]
    fn only_a_saved_browser_login_is_refreshable() {
        let oauth_auth = AuthConfig {
            kind: Some(crate::config::TokenKind::Oauth),
            refresh_token: Some("rt".to_string()),
            ..AuthConfig::default()
        };
        assert_eq!(
            refreshable(&cred(CredentialSource::Config), &oauth_auth).as_deref(),
            Some("rt")
        );
        // An env token wins over the saved login and is never refreshed
        assert!(refreshable(&cred(CredentialSource::ApiTokenEnv), &oauth_auth).is_none());
        assert!(refreshable(&cred(CredentialSource::AuthTokenEnv), &oauth_auth).is_none());
        let api_token = AuthConfig {
            kind: Some(crate::config::TokenKind::ApiToken),
            refresh_token: Some("rt".to_string()),
            ..AuthConfig::default()
        };
        assert!(refreshable(&cred(CredentialSource::Config), &api_token).is_none());
        let no_refresh = AuthConfig {
            kind: Some(crate::config::TokenKind::Oauth),
            refresh_token: Some(String::new()),
            ..AuthConfig::default()
        };
        assert!(refreshable(&cred(CredentialSource::Config), &no_refresh).is_none());
    }

    #[test]
    fn unauthorized_detection_covers_status_and_graphql_code() {
        assert!(is_unauthorized(401, ""));
        assert!(is_unauthorized(
            200,
            r#"{"errors":[{"message":"x","extensions":{"code":"UNAUTHENTICATED"}}]}"#
        ));
        assert!(!is_unauthorized(200, r#"{"data":{"ok":true}}"#));
        assert!(!is_unauthorized(
            403,
            r#"{"error":"UNAUTHENTICATED is a word"}"#
        ));
    }
}
