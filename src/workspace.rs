//! Workspace selection for a browser login.
//!
//! A browser login's access token lists every workspace the person
//! belongs to but names none as active, so the CLI picks one and sends
//! its id as the `X-Fractal-Workspace` header on every API request. The
//! API checks the id against the token's verified memberships.
//!
//! The workspace comes from, in order: the `--workspace` flag, the
//! `FRACTAL_WORKSPACE` env var (slug or id, resolved to an id), the
//! workspace saved by `fractal login` or `fractal workspace use`, and
//! finally the only workspace the login belongs to. A Fractal API token
//! (`ftk_...`) never sends the header: it is bound to its workspaces
//! when it is created.

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Serialize;

use crate::api::{self, CredentialSource};
use crate::config::Config;
use crate::oauth::{self, Workspace};

/// Env var naming the workspace commands act in (slug or id)
pub const WORKSPACE_ENV: &str = "FRACTAL_WORKSPACE";

/// Request header carrying the selected workspace id
pub const WORKSPACE_HEADER: &str = "X-Fractal-Workspace";

/// The `--workspace` flag, set once from `main` before any command runs
static FLAG: OnceLock<Option<String>> = OnceLock::new();

/// Record the global `--workspace` flag
pub fn set_flag(value: Option<String>) {
    let _ = FLAG.set(value);
}

fn flag() -> Option<String> {
    FLAG.get().cloned().flatten()
}

/// Where a workspace selection came from
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `--workspace`
    Flag,
    /// `FRACTAL_WORKSPACE`
    Env,
    /// Saved by `fractal login` or `fractal workspace use`
    Saved,
    /// The only workspace the login belongs to
    Sole,
}

impl Source {
    /// Short label for human output
    pub const fn label(self) -> &'static str {
        match self {
            Self::Flag => "--workspace",
            Self::Env => WORKSPACE_ENV,
            Self::Saved => "saved",
            Self::Sole => "only workspace",
        }
    }
}

/// A workspace selection before it is resolved to an id: a slug or id
/// from the flag or env var, or a saved workspace's id
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requested {
    pub value: String,
    pub source: Source,
}

fn non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// Pick the requested workspace: `--workspace`, then
/// `FRACTAL_WORKSPACE`, then the saved workspace, then the only
/// workspace in `memberships`. Blank values count as unset.
pub fn requested_from(
    flag: Option<&str>,
    env: Option<&str>,
    saved: Option<&Workspace>,
    memberships: &[String],
) -> Option<Requested> {
    let pick = |value: &str, source| Requested {
        value: value.to_string(),
        source,
    };
    non_empty(flag)
        .map(|v| pick(v, Source::Flag))
        .or_else(|| non_empty(env).map(|v| pick(v, Source::Env)))
        .or_else(|| {
            saved
                .and_then(|w| non_empty(Some(&w.id)))
                .map(|v| pick(v, Source::Saved))
        })
        .or_else(|| match memberships {
            [one] => non_empty(Some(one)).map(|v| pick(v, Source::Sole)),
            _ => None,
        })
}

/// The requested workspace for this process: the flag, the env var,
/// the saved workspace (browser login only), then the only membership
/// in `memberships`
pub fn requested(cfg: &Config, browser_login: bool, memberships: &[String]) -> Option<Requested> {
    let env = std::env::var(WORKSPACE_ENV).ok();
    let saved = if browser_login {
        cfg.workspace.as_ref()
    } else {
        None
    };
    requested_from(flag().as_deref(), env.as_deref(), saved, memberships)
}

/// Find a membership by id, then by slug (case-insensitive)
pub fn find<'a>(wanted: &str, all: &'a [Workspace]) -> Option<&'a Workspace> {
    let wanted = wanted.trim();
    all.iter().find(|w| w.id == wanted).or_else(|| {
        all.iter().find(|w| {
            w.slug
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(wanted))
        })
    })
}

/// How a requested workspace resolves without a network call
#[derive(Debug, PartialEq, Eq)]
pub enum Offline {
    /// Resolved to this id
    Id(String),
    /// A saved workspace the login no longer belongs to
    Stale,
    /// A slug (or unknown id) that needs the membership list
    NeedsLookup,
}

/// Resolve a requested workspace using only the access token's
/// membership ids and the saved workspace. An empty `token_ids` means
/// the token carries no membership claim, so nothing can be ruled out
/// locally and the API makes the call.
pub fn resolve_offline(
    req: &Requested,
    token_ids: &[String],
    saved: Option<&Workspace>,
) -> Offline {
    let is_member = |id: &str| token_ids.is_empty() || token_ids.iter().any(|t| t == id);
    match req.source {
        Source::Saved | Source::Sole => {
            if is_member(&req.value) {
                Offline::Id(req.value.clone())
            } else {
                Offline::Stale
            }
        }
        Source::Flag | Source::Env => {
            if token_ids.iter().any(|t| t == &req.value) {
                return Offline::Id(req.value.clone());
            }
            if let Some(saved) =
                saved.filter(|s| find(&req.value, std::slice::from_ref(s)).is_some())
            {
                if is_member(&saved.id) {
                    return Offline::Id(saved.id.clone());
                }
            }
            Offline::NeedsLookup
        }
    }
}

/// Error for a slug or id the login isn't a member of
pub fn not_a_member(wanted: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "you aren't a member of a workspace `{wanted}`; run `fractal workspace list` to see yours"
    )
}

/// Fetch the login's workspaces (with names and slugs) from userinfo
///
/// # Errors
///
/// Returns an error when the identity provider can't be reached or has
/// no userinfo endpoint.
pub async fn fetch_memberships(access_token: &str) -> Result<Vec<Workspace>> {
    let http = oauth::http_client()?;
    let discovery = oauth::discover(&http, &oauth::auth_url()).await?;
    let endpoint = discovery
        .userinfo_endpoint
        .context("the identity provider advertises no userinfo endpoint")?;
    let claims = oauth::userinfo(&http, &endpoint, access_token).await?;
    Ok(oauth::workspaces(&claims))
}

/// Sort key for listing workspaces: name, else slug, else id
fn sort_key(w: &Workspace) -> String {
    w.name
        .as_deref()
        .or(w.slug.as_deref())
        .unwrap_or(&w.id)
        .to_lowercase()
}

/// Workspaces sorted by name for display
pub fn sorted(all: &[Workspace]) -> Vec<Workspace> {
    let mut out = all.to_vec();
    out.sort_by_key(sort_key);
    out
}

/// What `fractal login` does about the workspace
#[derive(Debug, PartialEq, Eq)]
pub enum LoginChoice {
    /// The login belongs to no workspace
    Nothing,
    /// Exactly one workspace: select it without asking
    Only(Workspace),
    /// Ask on the terminal. `default` indexes `options` (the saved
    /// workspace, when it is still a membership)
    Pick {
        options: Vec<Workspace>,
        default: Option<usize>,
    },
    /// Several workspaces and no terminal: keep the saved workspace
    /// when it is still a membership, and print how to pick
    Hint { keep: Option<Workspace> },
}

/// Decide how `fractal login` selects a workspace
pub fn plan_login(all: &[Workspace], saved: Option<&Workspace>, interactive: bool) -> LoginChoice {
    match all {
        [] => LoginChoice::Nothing,
        [one] => LoginChoice::Only(one.clone()),
        _ => {
            let still_valid = saved.and_then(|s| all.iter().find(|w| w.id == s.id));
            if interactive {
                let options = sorted(all);
                let default = still_valid.and_then(|v| options.iter().position(|o| o.id == v.id));
                LoginChoice::Pick { options, default }
            } else {
                LoginChoice::Hint {
                    keep: still_valid.cloned(),
                }
            }
        }
    }
}

/// Parse a picker answer: a 1-based number, or empty for the default.
/// Returns the 0-based index.
pub fn parse_pick(input: &str, count: usize, default: Option<usize>) -> Result<usize, String> {
    let input = input.trim();
    if input.is_empty() {
        return default.ok_or_else(|| format!("enter a number from 1 to {count}"));
    }
    match input.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok(n - 1),
        _ => Err(format!("enter a number from 1 to {count}")),
    }
}

/// Whether the CLI can ask questions: stdin and stderr are terminals
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Show the numbered picker on stderr and read the answer from
/// `lines` (stdin, through [`oauth::spawn_line_reader`], so a paste
/// login's reader and the picker never compete for input)
///
/// # Errors
///
/// Returns an error when stdin closes before a valid answer.
pub async fn prompt_pick(
    options: &[Workspace],
    default: Option<usize>,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<Workspace> {
    let mut err = std::io::stderr();
    writeln!(err, "\n{}", "Select a workspace:".bold())?;
    for (i, w) in options.iter().enumerate() {
        let marker = if Some(i) == default { "*" } else { " " };
        writeln!(err, " {marker} {:>2}) {w}", i + 1)?;
    }
    loop {
        let hint = default.map_or_else(String::new, |d| format!(" [{}]", d + 1));
        write!(err, "Workspace number{hint}: ")?;
        err.flush()?;
        let line = lines
            .recv()
            .await
            .context("no workspace selected; run `fractal workspace use <slug>` to pick one")?;
        match parse_pick(&line, options.len(), default) {
            Ok(i) => return Ok(options[i].clone()),
            Err(msg) => writeln!(err, "{} {msg}", "Error:".red())?,
        }
    }
}

/// Print how to pick a workspace when several are available and none
/// is selected
pub fn print_pick_hint() {
    println!(
        "  {} {}; run {} to pick one ({} shows them)",
        "Workspace:".bold(),
        "none selected".yellow(),
        "fractal workspace use <slug>".cyan(),
        "fractal workspace list".cyan()
    );
}

/// The browser login's access token, or an error explaining why
/// workspace selection doesn't apply
async fn browser_login_token() -> Result<(Config, String)> {
    let cfg = Config::load()?;
    let cred = api::credential_from(&cfg).context(api::NOT_AUTHENTICATED)?;
    if cred.source != CredentialSource::Config {
        anyhow::bail!(
            "{} is set and takes precedence; workspace selection applies only to a browser login (`fractal login`)",
            cred.source.label()
        );
    }
    if !cfg.auth.is_oauth() {
        anyhow::bail!(
            "workspace selection applies only to a browser login (`fractal login`); an API token is bound to its workspaces when it is created"
        );
    }
    let token = api::ApiClient::from_env()?.bearer_token().await?;
    Ok((cfg, token))
}

/// The workspace commands act in, with names filled in from `all`
fn active(cfg: &Config, all: &[Workspace]) -> Option<(Workspace, Source)> {
    let ids: Vec<String> = all.iter().map(|w| w.id.clone()).collect();
    let req = requested(cfg, true, &ids)?;
    find(&req.value, all).cloned().map(|w| (w, req.source))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListedWorkspace {
    #[serde(flatten)]
    workspace: Workspace,
    current: bool,
}

/// `fractal workspace list`
///
/// # Errors
///
/// Returns an error when not logged in through the browser or the
/// identity provider can't be reached.
pub async fn list(json: bool) -> Result<()> {
    let (cfg, token) = browser_login_token().await?;
    let all = sorted(&fetch_memberships(&token).await?);
    let current = active(&cfg, &all).map(|(w, _)| w.id);
    if json {
        let rows: Vec<_> = all
            .into_iter()
            .map(|w| ListedWorkspace {
                current: current.as_deref() == Some(w.id.as_str()),
                workspace: w,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if all.is_empty() {
        println!("{}", "You don't belong to any workspace yet".dimmed());
        return Ok(());
    }
    for w in &all {
        if current.as_deref() == Some(w.id.as_str()) {
            println!("{} {}", "*".green(), w.to_string().bold());
        } else {
            println!("  {w}");
        }
    }
    if current.is_none() {
        println!();
        print_pick_hint();
    }
    Ok(())
}

/// `fractal workspace use <slug|id>`
///
/// # Errors
///
/// Returns an error when not logged in through the browser, the
/// workspace isn't one of the login's, or the config can't be saved.
pub async fn use_workspace(wanted: &str, json: bool) -> Result<()> {
    let (mut cfg, token) = browser_login_token().await?;
    let all = fetch_memberships(&token).await?;
    let chosen = find(wanted, &all)
        .cloned()
        .ok_or_else(|| not_a_member(wanted))?;
    cfg.workspace = Some(chosen.clone());
    cfg.save()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&chosen)?);
        return Ok(());
    }
    println!(
        "{} Using workspace {}",
        "\u{2713}".green(),
        chosen.to_string().cyan()
    );
    let env = std::env::var(WORKSPACE_ENV).ok();
    if let Some(env) = non_empty(env.as_deref()) {
        println!(
            "{} {WORKSPACE_ENV}={env} is set and takes precedence over the saved workspace",
            "Note:".yellow()
        );
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentWorkspace {
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<Workspace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<Source>,
}

/// `fractal workspace current`
///
/// # Errors
///
/// Returns an error when not logged in through the browser or the
/// identity provider can't be reached.
pub async fn current(json: bool) -> Result<()> {
    let (cfg, token) = browser_login_token().await?;
    let all = fetch_memberships(&token).await?;
    let active = active(&cfg, &all);
    if json {
        let (workspace, source) = active.map_or((None, None), |(w, s)| (Some(w), Some(s)));
        println!(
            "{}",
            serde_json::to_string_pretty(&CurrentWorkspace { workspace, source })?
        );
        return Ok(());
    }
    match active {
        Some((w, source)) => println!(
            "{} {} {}",
            "Workspace:".bold(),
            w.to_string().cyan(),
            format!("({})", source.label()).dimmed()
        ),
        None if all.is_empty() => println!("{}", "You don't belong to any workspace yet".dimmed()),
        None => print_pick_hint(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, name: &str, slug: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            name: Some(name.to_string()),
            slug: Some(slug.to_string()),
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn flag_beats_env_beats_saved_beats_sole_membership() {
        let saved = ws("org_s", "Saved", "saved");
        let one = ids(&["org_1"]);
        let r = requested_from(Some("flag"), Some("env"), Some(&saved), &one).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("flag", Source::Flag));
        let r = requested_from(None, Some("env"), Some(&saved), &one).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("env", Source::Env));
        let r = requested_from(None, None, Some(&saved), &one).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("org_s", Source::Saved));
        let r = requested_from(None, None, None, &one).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("org_1", Source::Sole));
    }

    #[test]
    fn blank_values_count_as_unset_and_several_memberships_pick_nothing() {
        let r = requested_from(Some("  "), Some(""), None, &ids(&["a", "b"]));
        assert_eq!(r, None);
        let r = requested_from(Some(" omni "), None, None, &[]).unwrap();
        assert_eq!(r.value, "omni");
    }

    #[test]
    fn find_matches_id_then_slug_case_insensitively() {
        let all = [ws("org_a", "Alpha", "alpha"), ws("beta", "Beta", "org_a")];
        // An exact id wins over another workspace's slug
        assert_eq!(find("org_a", &all).unwrap().name.as_deref(), Some("Alpha"));
        assert_eq!(find("ALPHA", &all).unwrap().id, "org_a");
        assert_eq!(find(" beta ", &all).unwrap().id, "beta");
        assert!(find("gamma", &all).is_none());
    }

    #[test]
    fn env_slug_or_id_resolves_to_an_id() {
        let saved = ws("org_s", "Saved", "saved");
        let token = ids(&["org_a", "org_s"]);
        let by_id = Requested {
            value: "org_a".to_string(),
            source: Source::Env,
        };
        assert_eq!(
            resolve_offline(&by_id, &token, Some(&saved)),
            Offline::Id("org_a".to_string())
        );
        let saved_slug = Requested {
            value: "saved".to_string(),
            source: Source::Env,
        };
        assert_eq!(
            resolve_offline(&saved_slug, &token, Some(&saved)),
            Offline::Id("org_s".to_string())
        );
        let other_slug = Requested {
            value: "omni".to_string(),
            source: Source::Flag,
        };
        assert_eq!(
            resolve_offline(&other_slug, &token, Some(&saved)),
            Offline::NeedsLookup
        );
    }

    #[test]
    fn a_saved_workspace_the_login_left_is_stale() {
        let saved = Requested {
            value: "org_gone".to_string(),
            source: Source::Saved,
        };
        assert_eq!(
            resolve_offline(&saved, &ids(&["org_a"]), None),
            Offline::Stale
        );
        // No membership claim on the token: the API decides
        assert_eq!(
            resolve_offline(&saved, &[], None),
            Offline::Id("org_gone".to_string())
        );
    }

    #[test]
    fn login_selects_a_sole_workspace_automatically() {
        let a = ws("a", "Alpha", "alpha");
        assert_eq!(
            plan_login(std::slice::from_ref(&a), None, true),
            LoginChoice::Only(a.clone())
        );
        assert_eq!(
            plan_login(std::slice::from_ref(&a), None, false),
            LoginChoice::Only(a)
        );
        assert_eq!(plan_login(&[], None, true), LoginChoice::Nothing);
    }

    #[test]
    fn login_picker_sorts_by_name_and_preselects_a_still_valid_saved_workspace() {
        let all = [
            ws("z", "zeta", "zeta"),
            ws("o", "Omni", "omni"),
            ws("a", "acme", "acme"),
        ];
        let saved = ws("o", "Old name", "omni");
        let LoginChoice::Pick { options, default } = plan_login(&all, Some(&saved), true) else {
            panic!("expected a picker");
        };
        let names: Vec<_> = options.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(names, ["a", "o", "z"]);
        assert_eq!(default, Some(1));

        let gone = ws("x", "Gone", "gone");
        let LoginChoice::Pick { default, .. } = plan_login(&all, Some(&gone), true) else {
            panic!("expected a picker");
        };
        assert_eq!(default, None);
    }

    #[test]
    fn login_without_a_terminal_never_prompts() {
        let all = [ws("a", "Alpha", "alpha"), ws("b", "Beta", "beta")];
        let saved = ws("b", "Beta", "beta");
        assert_eq!(
            plan_login(&all, Some(&saved), false),
            LoginChoice::Hint {
                keep: Some(all[1].clone())
            }
        );
        assert_eq!(
            plan_login(&all, Some(&ws("x", "X", "x")), false),
            LoginChoice::Hint { keep: None }
        );
    }

    #[test]
    fn picker_answers_are_one_based_with_an_optional_default() {
        assert_eq!(parse_pick("2", 3, None), Ok(1));
        assert_eq!(parse_pick(" 1 \n", 3, None), Ok(0));
        assert_eq!(parse_pick("", 3, Some(2)), Ok(2));
        assert!(parse_pick("", 3, None).is_err());
        assert!(parse_pick("0", 3, None).is_err());
        assert!(parse_pick("4", 3, None).is_err());
        assert!(parse_pick("omni", 3, None).is_err());
    }
}
