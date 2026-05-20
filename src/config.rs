//! Persistent CLI configuration. Stored at
//! `$XDG_CONFIG_HOME/fractal/config.toml` (or
//! `~/.config/fractal/config.toml`). Currently holds only the bearer
//! token from `fractal auth login`; other state lives elsewhere
//! (kubeconfig on disk for cluster connectivity, env vars for the API
//! endpoint).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub auth: AuthConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Bearer token from `fractal auth login`. Persisted as-is; the
    /// file lives at 0600 so other users on the host can't read it.
    pub access_token: Option<String>,
    /// HIDRA user id ("sub" claim) from the most recent login, kept
    /// alongside the token so `fractal auth whoami` works without a
    /// network round-trip.
    pub user_id: Option<String>,
    /// Display email from the most recent login. Cosmetic only.
    pub email: Option<String>,
}

/// Compute the path to the CLI's config file. Honors
/// `XDG_CONFIG_HOME`; otherwise uses `~/.config`.
pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .context("could not determine user config directory (set XDG_CONFIG_HOME)")?;
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
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Persist the config to disk, creating parent directories as
    /// needed. Tightens permissions on the file to 0600 on Unix so
    /// the bearer token isn't world-readable.
    ///
    /// # Errors
    ///
    /// Returns an error on filesystem or serialization failure.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let serialized =
            toml::to_string(self).context("failed to serialize config")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&path, perms)?;
        }
        Ok(())
    }
}
