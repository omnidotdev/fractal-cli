use thiserror::Error;

/// Errors from Fractal CLI operations
#[expect(dead_code, reason = "variants used in Tasks 2.2-2.5")]
#[derive(Debug, Error)]
pub enum FractalError {
    /// Kubernetes API call failed
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// TOML parse error
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
}
