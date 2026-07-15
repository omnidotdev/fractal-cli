use std::time::SystemTime;

use anyhow::{Context, Result};
use colored::Colorize;
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams, LogParams};

use fractal_operator::crd::service::FractalService;

/// Stream logs from a service's pods.
///
/// `since` is an optional human-readable duration ("5m", "1h"). `grep` is
/// an optional substring filter applied to every line client-side.
/// `follow=true` keeps the stream open and tails new lines.
///
/// # Errors
///
/// Returns an error if the service or its pods cannot be located, or if
/// the log stream fails to open.
pub async fn tail_logs(
    service: &str,
    follow: bool,
    since: Option<&str>,
    tail: Option<i64>,
    grep: Option<&str>,
) -> Result<()> {
    let client = Client::try_default().await?;

    // Verify the FractalService exists. Resolves the namespace from
    // kubeconfig context (user is expected to `kubectl config set
    // namespace fractal-{project}` before invoking).
    let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());
    svc_api.get(service).await.context(format!(
        "service '{service}' not found in current namespace"
    ))?;

    let pod_api: Api<Pod> = Api::default_namespaced(client);
    let pods = pod_api
        .list(&ListParams::default().labels(&format!("app={service}")))
        .await
        .context("failed to list pods")?;

    if pods.items.is_empty() {
        anyhow::bail!("no pods found for service '{service}' (label app={service})");
    }

    let since_seconds = since.and_then(parse_duration_secs);

    let mut log_params = LogParams {
        follow,
        tail_lines: tail,
        timestamps: true,
        ..Default::default()
    };
    if let Some(s) = since_seconds {
        log_params.since_seconds = Some(s);
    }

    // Multiplex log streams across all matching pods. Each line is prefixed
    // with the pod name (truncated to keep output readable).
    let mut tasks = Vec::with_capacity(pods.items.len());
    for pod in pods.items {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let api = pod_api.clone();
        let params = log_params.clone();
        let grep_owned = grep.map(String::from);
        tasks.push(tokio::spawn(async move {
            stream_pod_logs(&api, &pod_name, &params, grep_owned.as_deref()).await
        }));
    }

    for t in tasks {
        if let Err(e) = t.await? {
            eprintln!("{} {e}", "log stream error:".yellow());
        }
    }
    Ok(())
}

async fn stream_pod_logs(
    api: &Api<Pod>,
    pod_name: &str,
    params: &LogParams,
    grep: Option<&str>,
) -> Result<()> {
    let mut stream = api
        .log_stream(pod_name, params)
        .await
        .context(format!("failed to open log stream for pod '{pod_name}'"))?
        .lines();

    let short = pod_name.split('-').next_back().unwrap_or(pod_name);
    let prefix = format!("[{short}]").dimmed().to_string();

    while let Some(line) = stream.next().await {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{} {e}", "read error:".yellow());
                continue;
            }
        };
        if grep.is_some_and(|needle| !line.contains(needle)) {
            continue;
        }
        println!("{prefix} {line}");
    }

    Ok(())
}

/// Parse "5m", "1h", "30s", "2d" into seconds.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num_part.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        "d" => Some(n * 86400),
        _ => None,
    }
}

#[allow(dead_code)]
fn _current_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
    }
    #[test]
    fn parses_minutes() {
        assert_eq!(parse_duration_secs("5m"), Some(300));
    }
    #[test]
    fn parses_hours() {
        assert_eq!(parse_duration_secs("2h"), Some(7200));
    }
    #[test]
    fn parses_days() {
        assert_eq!(parse_duration_secs("1d"), Some(86400));
    }
    #[test]
    fn rejects_invalid_unit() {
        assert_eq!(parse_duration_secs("10x"), None);
    }
    #[test]
    fn rejects_empty() {
        assert_eq!(parse_duration_secs(""), None);
    }
    #[test]
    fn rejects_bare_number() {
        assert_eq!(parse_duration_secs("60"), None);
    }
}
