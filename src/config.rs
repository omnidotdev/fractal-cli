//! Persistent CLI configuration. Stored as `fractal/config.toml` in
//! the OS config dir: `$XDG_CONFIG_HOME/fractal/config.toml` on Linux
//! (usually `~/.config/fractal/config.toml`) and
//! `~/Library/Application Support/fractal/config.toml` on macOS. Holds the credential from
//! `fractal login` (a browser login's access and refresh tokens, or a
//! saved API token), the preferred transport (`api` or `kube`), and an
//! optional API base URL. Kubeconfig stays on disk where kubectl keeps
//! it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    /// Preferred transport (`api` or `kube`). Written as `kube` by
    /// `fractal cluster connect` so BYOC users who also log in keep
    /// talking to their own cluster. Flag and env var override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Fractal API base URL. `FRACTAL_API_URL` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    #[serde(default)]
    pub auth: AuthConfig,
}

/// What kind of credential `access_token` holds
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Gatekeeper access token from the browser login, refreshable with
    /// `refresh_token`
    Oauth,
    /// Fractal API token (`ftk_...`) saved with `--token`
    ApiToken,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Bearer token sent to the API. Persisted as-is; the file lives at
    /// 0600 so other users on the host can't read it.
    pub access_token: Option<String>,
    /// Which kind of credential `access_token` is. Absent in configs
    /// written before the browser login existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TokenKind>,
    /// Refresh token from the browser login, used to mint a new access
    /// token before the current one expires
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Access token expiry, in Unix seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// HIDRA user id ("sub" claim) from the most recent login, kept
    /// alongside the token so `fractal whoami` works without a network
    /// round-trip.
    pub user_id: Option<String>,
    /// Display email from the most recent login. Cosmetic only.
    pub email: Option<String>,
    /// Display name from the most recent login. Cosmetic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl AuthConfig {
    /// Whether the saved credential is a refreshable browser login
    pub fn is_oauth(&self) -> bool {
        self.kind == Some(TokenKind::Oauth)
    }
}

/// Compute the path to the CLI's config file: `fractal/config.toml` in
/// the OS config dir (`$XDG_CONFIG_HOME` or `~/.config` on Linux,
/// `~/Library/Application Support` on macOS)
pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(base.join("fractal").join("config.toml"))
}

impl Config {
    /// Load the config from disk. Returns a default (empty) config when
    /// the file does not exist - this is normal for a fresh install.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or
    /// parsed.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    /// Load the config from an explicit path, defaulting when absent
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or
    /// parsed.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Persist the config to disk, creating parent directories as
    /// needed. The file is 0600 on Unix so the tokens aren't readable
    /// by other users.
    ///
    /// # Errors
    ///
    /// Returns an error on filesystem or serialization failure.
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path()?)
    }

    /// Persist the config to an explicit path (0600 on Unix)
    ///
    /// # Errors
    ///
    /// Returns an error on filesystem or serialization failure.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let serialized = toml::to_string(self).context("failed to serialize config")?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Create the file owner-only so a token is never briefly
        // world-readable between the write and a chmod
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(serialized.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        // `mode` only applies on create, so tighten a pre-existing file too
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Forget every saved credential, keeping transport and API URL
    pub fn clear_auth(&mut self) {
        self.auth = AuthConfig::default();
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthConfig, Config, TokenKind};

    #[test]
    fn parses_legacy_config_without_new_fields() {
        let cfg: Config = toml::from_str("[auth]\naccess_token = \"abc\"\n").unwrap();
        assert_eq!(cfg.auth.access_token.as_deref(), Some("abc"));
        assert!(cfg.auth.kind.is_none());
        assert!(cfg.auth.refresh_token.is_none());
        assert!(!cfg.auth.is_oauth());
        assert!(cfg.transport.is_none());
        assert!(cfg.api_url.is_none());
    }

    #[test]
    fn round_trips_transport_and_api_url() {
        let cfg = Config {
            transport: Some("kube".to_string()),
            api_url: Some("https://api.example.test".to_string()),
            ..Config::default()
        };
        let out = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(back.transport.as_deref(), Some("kube"));
        assert_eq!(back.api_url.as_deref(), Some("https://api.example.test"));
    }

    #[test]
    fn persists_an_oauth_session_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let cfg = Config {
            transport: Some("api".to_string()),
            auth: AuthConfig {
                access_token: Some("at".to_string()),
                kind: Some(TokenKind::Oauth),
                refresh_token: Some("rt".to_string()),
                expires_at: Some(1_700_000_000),
                user_id: Some("u1".to_string()),
                email: Some("a@example.test".to_string()),
                name: Some("Ada".to_string()),
            },
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("kind = \"oauth\""), "{raw}");

        let back = Config::load_from(&path).unwrap();
        assert!(back.auth.is_oauth());
        assert_eq!(back.auth.access_token.as_deref(), Some("at"));
        assert_eq!(back.auth.refresh_token.as_deref(), Some("rt"));
        assert_eq!(back.auth.expires_at, Some(1_700_000_000));
        assert_eq!(back.auth.name.as_deref(), Some("Ada"));
        assert_eq!(back.transport.as_deref(), Some("api"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn tightens_an_existing_world_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        Config::default().save_to(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn clear_auth_keeps_transport() {
        let mut cfg = Config {
            transport: Some("kube".to_string()),
            auth: AuthConfig {
                access_token: Some("at".to_string()),
                kind: Some(TokenKind::Oauth),
                refresh_token: Some("rt".to_string()),
                ..AuthConfig::default()
            },
            ..Config::default()
        };
        cfg.clear_auth();
        assert!(cfg.auth.access_token.is_none());
        assert!(cfg.auth.refresh_token.is_none());
        assert_eq!(cfg.transport.as_deref(), Some("kube"));
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.toml")).unwrap();
        assert!(cfg.auth.access_token.is_none());
    }
}
