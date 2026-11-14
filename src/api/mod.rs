//! Hosted transport: a small client for the Fractal API. GraphQL
//! (`<api>/graphql`) covers projects, services, env, scale, and API
//! tokens; REST endpoints under `/v1` cover deploys, logs, and project
//! members. Every request carries the caller's bearer token, either a
//! Fractal API token (`ftk_...`) or the token from `fractal auth login`.

pub mod model;
pub mod resolve;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;

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
pub const NOT_AUTHENTICATED: &str = "Not authenticated. Set FRACTAL_API_TOKEN to a Fractal API token \
     (see https://docs.omni.dev/products/fractal/api-tokens) or run `fractal auth login`.";

/// Message shown when the API rejects the credential
const REJECTED: &str = "The Fractal API rejected your credential (it may be invalid, expired, or \
     revoked). Create a new API token and set FRACTAL_API_TOKEN, or run `fractal auth login`.";

/// Authenticated client for the Fractal API
pub struct ApiClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// Build a client from the resolved URL and credential
    ///
    /// # Errors
    ///
    /// Returns an error when no credential is configured.
    pub fn from_env() -> Result<Self> {
        let cred = credential().context(NOT_AUTHENTICATED)?;
        Self::new(&api_url(), &cred.token)
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
            token: token.to_string(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
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
        let resp = self
            .http
            .post(self.url("/graphql"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|e| unreachable_error(&e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
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
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .query(query)
            .send()
            .await
            .map_err(|e| unreachable_error(&e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
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
        let resp = self
            .http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| unreachable_error(&e))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        parse_rest(status, &text)
    }
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
}
