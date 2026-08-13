use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use colored::Colorize;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::{Client, ResourceExt};
use rand::TryRngCore;
use rand::rngs::OsRng;

use fractal_operator::crd::api_token::{FractalApiToken, FractalApiTokenSpec, hash_token};

/// Namespace where tokens live. Tokens are workspace-scoped, not
/// project-scoped, so they all land in a single namespace per workspace.
const TOKENS_NS: &str = "fractal-system";

/// Generate 32 bytes of OS-grade entropy and base32-encode into a
/// human-friendly bearer token. Format: `ftk_<52 base32 chars>`.
///
/// 32 bytes = 256 bits of entropy. Base32 (no padding) keeps the token
/// shell-safe + double-click-selectable (no `+`/`/` from base64).
fn generate_plaintext_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS entropy source is required for token generation");

    // Quick-and-correct base32 encoder: process 5-byte groups → 8 base32
    // chars. 32 bytes = 6 full groups + 2 trailing bytes (4 more chars).
    let mut out = String::from("ftk_");
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0b11111) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0b11111) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// Default CR `metadata.name` for a new token. Workspace-scoped so the
/// `name` label can repeat across projects, but the CR name has to be
/// unique within `fractal-system`.
fn token_resource_name(label: &str) -> String {
    let suffix: String = generate_plaintext_token().chars().skip(4).take(8).collect();
    format!(
        "{}-{}",
        label
            .to_ascii_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), "-"),
        suffix
    )
}

/// Create a new API token, printing the plaintext exactly once.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable or the CR cannot be
/// created (e.g. duplicate name collision -- callers should retry).
pub async fn create_token(
    label: &str,
    subject: &str,
    scopes: Vec<String>,
    expires_in_days: Option<i64>,
) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalApiToken> = Api::namespaced(client, TOKENS_NS);

    let plaintext = generate_plaintext_token();
    let now = Utc::now();
    let expires_at = expires_in_days.map(|d| (now + Duration::days(d)).to_rfc3339());

    let spec = FractalApiTokenSpec {
        name: label.to_string(),
        subject: subject.to_string(),
        scopes,
        // Personal token (acts only as `subject`); the CLI has no org flag yet.
        // The field is `#[serde(default)]`, so empty is the backward-compatible
        // personal-token default
        organizations: Vec::new(),
        token_hash: hash_token(&plaintext),
        expires_at: expires_at.clone(),
        issued_at: now.to_rfc3339(),
        revoked_at: None,
    };

    let resource_name = token_resource_name(label);
    let cr = FractalApiToken::new(&resource_name, spec);

    api.create(&PostParams::default(), &cr)
        .await
        .context("failed to create FractalApiToken")?;

    println!(
        "{} token {} created (subject {})",
        "\u{2713}".green(),
        label.cyan(),
        subject.dimmed(),
    );
    if let Some(exp) = expires_at {
        println!("  {} {}", "Expires:".bold(), exp.yellow());
    } else {
        println!("  {} {}", "Expires:".bold(), "never".yellow());
    }
    println!();
    println!(
        "{} {}",
        "Token (copy now -- it will not be shown again):"
            .bold()
            .red(),
        plaintext.cyan()
    );
    println!();
    println!("  {} {}", "Resource name:".bold(), resource_name.dimmed());
    Ok(())
}

/// List API tokens. Plaintext values are never displayed -- only labels,
/// subjects, scopes, expiry, and usage telemetry.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable.
pub async fn list_tokens() -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalApiToken> = Api::namespaced(client, TOKENS_NS);

    let tokens = api
        .list(&kube::api::ListParams::default())
        .await
        .context("failed to list FractalApiTokens")?;

    if tokens.items.is_empty() {
        println!("{}", "(no tokens)".dimmed());
        return Ok(());
    }

    println!(
        "{:<24}  {:<24}  {:<16}  {}",
        "NAME".bold(),
        "SUBJECT".bold(),
        "EXPIRES".bold(),
        "STATUS".bold()
    );
    for tok in tokens.items {
        let status = if tok.spec.revoked_at.is_some() {
            "revoked".red().to_string()
        } else if is_expired(tok.spec.expires_at.as_ref()) {
            "expired".yellow().to_string()
        } else {
            "active".green().to_string()
        };
        let expires = tok.spec.expires_at.as_deref().unwrap_or("never");
        let last_used = tok
            .status
            .as_ref()
            .and_then(|s| s.last_used_at.clone())
            .map_or_else(|| "never".dimmed().to_string(), |t| t.dimmed().to_string());
        println!(
            "{:<24}  {:<24}  {:<16}  {}  (last used {})",
            tok.spec.name.cyan(),
            tok.spec.subject,
            expires.yellow(),
            status,
            last_used,
        );
    }
    Ok(())
}

/// Revoke a token by patching `spec.revokedAt`.
///
/// Uses the human-readable label (`spec.name`) so callers don't have to
/// remember the random CR name. If multiple tokens share the same label
/// (allowed), the most recently issued one is revoked.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable, no token matches
/// the label, or the patch fails.
pub async fn revoke_token(label: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalApiToken> = Api::namespaced(client, TOKENS_NS);

    let tokens = api
        .list(&kube::api::ListParams::default())
        .await
        .context("failed to list FractalApiTokens")?;

    let target = tokens
        .items
        .into_iter()
        .filter(|t| t.spec.name == label && t.spec.revoked_at.is_none())
        .max_by(|a, b| a.spec.issued_at.cmp(&b.spec.issued_at))
        .with_context(|| format!("no active token found with label '{label}'"))?;

    let resource_name = target.name_any();
    let patch = serde_json::json!({
        "spec": { "revokedAt": Utc::now().to_rfc3339() },
    });

    api.patch(
        &resource_name,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .context("failed to revoke token")?;

    println!(
        "{} token {} revoked ({})",
        "\u{2713}".green(),
        label.cyan(),
        resource_name.dimmed()
    );
    Ok(())
}

/// Delete a token CR outright. Prefer `revoke` (preserves audit trail);
/// this is for cleanup of already-revoked or expired tokens.
///
/// # Errors
///
/// Returns an error if the apiserver is unreachable or the CR is missing.
pub async fn delete_token(label: &str) -> Result<()> {
    let client = Client::try_default().await?;
    let api: Api<FractalApiToken> = Api::namespaced(client, TOKENS_NS);

    let tokens = api
        .list(&kube::api::ListParams::default())
        .await
        .context("failed to list FractalApiTokens")?;

    let target = tokens
        .items
        .into_iter()
        .find(|t| t.spec.name == label)
        .with_context(|| format!("no token found with label '{label}'"))?;

    let resource_name = target.name_any();
    api.delete(&resource_name, &DeleteParams::default())
        .await
        .context("failed to delete token CR")?;

    println!(
        "{} token {} deleted ({})",
        "\u{2713}".green(),
        label.cyan(),
        resource_name.dimmed()
    );
    Ok(())
}

fn is_expired(expires_at: Option<&String>) -> bool {
    let Some(s) = expires_at else { return false };
    chrono::DateTime::parse_from_rfc3339(s).is_ok_and(|t| t < Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_token_has_ftk_prefix_and_is_base32() {
        let t = generate_plaintext_token();
        assert!(t.starts_with("ftk_"), "got {t}");
        let suffix = &t[4..];
        assert!(suffix.len() >= 50, "got {} chars", suffix.len());
        for c in suffix.chars() {
            assert!(
                ('a'..='z').contains(&c) || ('2'..='7').contains(&c),
                "non-base32 char in {t}: {c}",
            );
        }
    }

    #[test]
    fn plaintext_tokens_are_unique() {
        let t1 = generate_plaintext_token();
        let t2 = generate_plaintext_token();
        assert_ne!(t1, t2);
    }

    #[test]
    fn token_resource_name_normalizes_label() {
        let name = token_resource_name("CI / Deploys");
        assert!(name.starts_with("ci---deploys-"));
        assert!(!name.chars().any(|c| c == ' ' || c == '/' || c == 'I'));
    }

    #[test]
    fn is_expired_handles_none() {
        assert!(!is_expired(None));
    }

    #[test]
    fn is_expired_recognises_past_dates() {
        let s = "2000-01-01T00:00:00Z".to_string();
        assert!(is_expired(Some(&s)));
    }

    #[test]
    fn is_expired_rejects_future_dates() {
        let s = "2100-01-01T00:00:00Z".to_string();
        assert!(!is_expired(Some(&s)));
    }

    #[test]
    fn is_expired_handles_invalid_format() {
        // Treat malformed timestamps as not-yet-expired -- safer than
        // accidentally revoking a token because of a parsing edge case.
        let s = "not-a-date".to_string();
        assert!(!is_expired(Some(&s)));
    }
}
