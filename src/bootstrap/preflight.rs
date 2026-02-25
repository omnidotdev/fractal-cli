use anyhow::{bail, Result};
use colored::Colorize;

use super::BootstrapMode;

/// Verify that required binaries are available on `$PATH`.
///
/// Each bootstrap mode has different requirements:
/// - **Dev**: `docker`, `k3d`
/// - **Production**: `curl`
/// - **Connect**: `kubectl`
///
/// # Errors
///
/// Returns an error if any required binary is missing.
pub fn check(mode: &BootstrapMode) -> Result<()> {
    let required: &[&str] = match mode {
        BootstrapMode::Dev => &["docker", "k3d"],
        BootstrapMode::Production => &["curl"],
        BootstrapMode::Connect => &["kubectl"],
    };

    let mut missing = Vec::new();

    for bin in required {
        if which::which(bin).is_ok() {
            println!("{} {} found", "\u{2713}".green(), bin.cyan());
        } else {
            println!("{} {} not found", "\u{2717}".red(), bin.cyan());
            missing.push(*bin);
        }
    }

    if missing.is_empty() {
        println!(
            "\n{} All preflight checks passed",
            "\u{2713}".green()
        );
        Ok(())
    } else {
        bail!(
            "missing required tools: {}",
            missing.join(", ")
        )
    }
}
