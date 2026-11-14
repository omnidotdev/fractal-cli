//! Transport selection: whether a command talks to the hosted Fractal
//! API with a bearer token, or straight to a Kubernetes cluster through
//! kubeconfig (BYOC and platform admins).
//!
//! Precedence, highest first:
//!   1. `--transport api|kube`
//!   2. `FRACTAL_TRANSPORT`
//!   3. `transport` in the config file (`~/.config/fractal/config.toml`
//!      on Linux, `~/Library/Application Support/fractal/config.toml` on
//!      macOS; see [`crate::config::config_path`]). Written as `kube` by
//!      `fractal cluster connect` and cleared by `fractal cluster
//!      disconnect`
//!   4. `api` when an API credential is present, else `kube`

use std::fmt;

use anyhow::{Result, bail};
use clap::ValueEnum;

/// How a command reaches the platform
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// The hosted Fractal API, authenticated with a bearer token
    Api,
    /// A Kubernetes cluster, authenticated through kubeconfig
    Kube,
}

impl Transport {
    /// Parse a transport name from an env var or config value
    ///
    /// # Errors
    ///
    /// Returns an error when the value is neither `api` nor `kube`.
    pub fn parse(raw: &str, origin: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "api" => Ok(Self::Api),
            "kube" | "kubernetes" => Ok(Self::Kube),
            other => bail!("invalid transport '{other}' in {origin} (expected 'api' or 'kube')"),
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Api => "api",
            Self::Kube => "kube",
        })
    }
}

/// Pick the transport from every input, highest precedence first. Pure
/// so the rules are unit-testable without touching env or disk.
///
/// # Errors
///
/// Returns an error when the env var or config holds an unknown value.
pub fn select(
    flag: Option<Transport>,
    env: Option<&str>,
    config: Option<&str>,
    has_api_credential: bool,
) -> Result<Transport> {
    if let Some(t) = flag {
        return Ok(t);
    }
    if let Some(raw) = env.filter(|v| !v.trim().is_empty()) {
        return Transport::parse(raw, "FRACTAL_TRANSPORT");
    }
    if let Some(raw) = config.filter(|v| !v.trim().is_empty()) {
        return Transport::parse(raw, "config.toml");
    }
    Ok(if has_api_credential {
        Transport::Api
    } else {
        Transport::Kube
    })
}

/// Resolve the transport from the flag, the environment, and the
/// persisted config
///
/// # Errors
///
/// Returns an error when the config can't be read or holds an unknown
/// transport value.
pub fn resolve(flag: Option<Transport>) -> Result<Transport> {
    if let Some(t) = flag {
        return Ok(t);
    }
    let env = std::env::var("FRACTAL_TRANSPORT").ok();
    let cfg = crate::config::Config::load()?;
    let has_credential = crate::api::credential_from(&cfg).is_some();
    select(
        None,
        env.as_deref(),
        cfg.transport.as_deref(),
        has_credential,
    )
}

/// Error for a command that has no hosted equivalent yet. `hint` tells
/// the user what to do instead.
pub fn unavailable(command: &str, hint: &str) -> anyhow::Error {
    anyhow::anyhow!("`fractal {command}` isn't available for hosted workspaces yet; {hint}")
}

#[cfg(test)]
mod tests {
    use super::{Transport, select, unavailable};

    #[test]
    fn flag_beats_everything() {
        let t = select(Some(Transport::Kube), Some("api"), Some("api"), true).unwrap();
        assert_eq!(t, Transport::Kube);
        let t = select(Some(Transport::Api), Some("kube"), Some("kube"), false).unwrap();
        assert_eq!(t, Transport::Api);
    }

    #[test]
    fn env_beats_config_and_credential() {
        assert_eq!(
            select(None, Some("kube"), Some("api"), true).unwrap(),
            Transport::Kube
        );
        assert_eq!(
            select(None, Some("API"), Some("kube"), false).unwrap(),
            Transport::Api
        );
    }

    #[test]
    fn config_beats_credential() {
        // A BYOC user who ran `cluster connect` and later logged in keeps kube
        assert_eq!(
            select(None, None, Some("kube"), true).unwrap(),
            Transport::Kube
        );
        assert_eq!(
            select(None, None, Some("api"), false).unwrap(),
            Transport::Api
        );
    }

    #[test]
    fn credential_picks_api_otherwise_kube() {
        assert_eq!(select(None, None, None, true).unwrap(), Transport::Api);
        assert_eq!(select(None, None, None, false).unwrap(), Transport::Kube);
    }

    #[test]
    fn blank_values_fall_through() {
        assert_eq!(
            select(None, Some(""), Some("  "), true).unwrap(),
            Transport::Api
        );
    }

    #[test]
    fn unknown_values_are_rejected_with_origin() {
        let err = select(None, Some("grpc"), None, true).unwrap_err();
        assert!(err.to_string().contains("FRACTAL_TRANSPORT"));
        let err = select(None, None, Some("ssh"), true).unwrap_err();
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn display_round_trips() {
        for t in [Transport::Api, Transport::Kube] {
            assert_eq!(Transport::parse(&t.to_string(), "test").unwrap(), t);
        }
    }

    #[test]
    fn unavailable_message_is_actionable() {
        let msg = unavailable("exec", "use the Shell tab in the dashboard").to_string();
        assert_eq!(
            msg,
            "`fractal exec` isn't available for hosted workspaces yet; use the Shell tab in the dashboard"
        );
        assert!(!msg.to_lowercase().contains("kubeconfig"));
    }
}
