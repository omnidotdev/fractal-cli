use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use colored::Colorize;
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams, LogParams};
use serde::{Deserialize, Serialize};

use fractal_operator::crd::service::FractalService;

use crate::api;

/// Machine-readable description of a log stream, emitted as the first
/// line when `--json` is set. Agents read this to learn which pods and
/// parameters a tail resolved to before the NDJSON lines start.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogStreamMeta {
    service: String,
    namespace: String,
    pods: Vec<String>,
    follow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tail: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grep: Option<String>,
}

/// One log line as an NDJSON object under `--json`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogLine<'a> {
    pod: &'a str,
    message: &'a str,
}

/// Stream logs from a service's pods.
///
/// `since` is an optional human-readable duration ("5m", "1h"). `grep` is
/// an optional substring filter applied to every line client-side.
/// `follow=true` keeps the stream open and tails new lines. With `json`,
/// a `LogStreamMeta` object is printed first, then each line is emitted
/// as an NDJSON `LogLine` object.
///
/// # Errors
///
/// Returns an error if the service or its pods cannot be located, if the
/// log stream fails to open, or if JSON serialization fails.
pub async fn tail_logs(
    service: &str,
    follow: bool,
    since: Option<&str>,
    tail: Option<i64>,
    grep: Option<&str>,
    previous: bool,
    json: bool,
) -> Result<()> {
    // Reject a bad `--since` before touching the cluster
    let since_seconds = since.map(parse_since).transpose()?;
    let client = Client::try_default().await?;
    let namespace = client.default_namespace().to_string();

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

    let mut log_params = LogParams {
        follow,
        tail_lines: tail,
        timestamps: true,
        previous,
        ..Default::default()
    };
    if let Some(s) = since_seconds {
        log_params.since_seconds = Some(s);
    }

    if json {
        let meta = LogStreamMeta {
            service: service.to_string(),
            namespace,
            pods: pods
                .items
                .iter()
                .filter_map(|p| p.metadata.name.clone())
                .collect(),
            follow,
            since: since.map(String::from),
            tail,
            grep: grep.map(String::from),
        };
        println!("{}", serde_json::to_string(&meta)?);
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
            stream_pod_logs(&api, &pod_name, &params, grep_owned.as_deref(), json).await
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
    json: bool,
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
        if json {
            // NDJSON: one object per line, keyed by the short pod id.
            match serde_json::to_string(&LogLine {
                pod: short,
                message: &line,
            }) {
                Ok(encoded) => println!("{encoded}"),
                Err(e) => eprintln!("{} {e}", "encode error:".yellow()),
            }
        } else {
            println!("{prefix} {line}");
        }
    }

    Ok(())
}

/// `fractal logs` flags, shared by both transports
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct LogOptions<'a> {
    pub follow: bool,
    pub since: Option<&'a str>,
    pub tail: Option<i64>,
    pub grep: Option<&'a str>,
    pub timestamps: bool,
    pub previous: bool,
    pub json: bool,
}

/// How often `--follow` polls the API
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Extra seconds of overlap on every poll so no line falls between two
/// windows; duplicates are dropped by timestamp
const POLL_OVERLAP_SECS: i64 = 5;

/// Consecutive poll failures tolerated before `--follow` gives up
const MAX_POLL_FAILURES: u32 = 5;

/// `GET /v1/services/logs` reply
#[derive(Debug, Deserialize)]
struct LogsReply {
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    pods: Vec<PodLog>,
}

#[derive(Debug, Deserialize)]
struct PodLog {
    name: String,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    log: Option<String>,
}

/// Query parameters for one logs request
fn log_query(
    namespace: &str,
    project: &str,
    service: &str,
    tail: Option<i64>,
    since_seconds: Option<i64>,
    previous: bool,
) -> Vec<(&'static str, String)> {
    let mut q = vec![
        ("namespace", namespace.to_string()),
        ("project", project.to_string()),
        ("service", service.to_string()),
    ];
    if let Some(t) = tail {
        q.push(("tailLines", t.to_string()));
    }
    if let Some(s) = since_seconds {
        q.push(("sinceSeconds", s.to_string()));
    }
    // Always ask for timestamps: `--follow` dedupes on them, and they are
    // stripped from the output unless the user asked for them
    q.push(("timestamps", "true".to_string()));
    if previous {
        q.push(("previous", "true".to_string()));
    }
    q
}

/// Split a Kubernetes timestamp prefix off a log line
fn split_timestamp(line: &str) -> (Option<DateTime<FixedOffset>>, &str) {
    if let Some((head, rest)) = line.split_once(' ') {
        if let Ok(ts) = DateTime::parse_from_rfc3339(head) {
            return (Some(ts), rest);
        }
    }
    (None, line)
}

/// Drops lines already printed by an earlier, overlapping poll. Tracks
/// the newest timestamp per pod/container plus the lines seen at it.
#[derive(Default)]
struct Dedup {
    seen: HashMap<String, (DateTime<FixedOffset>, HashSet<String>)>,
}

impl Dedup {
    /// True when the line is new and should be printed
    fn admit(&mut self, key: &str, ts: Option<DateTime<FixedOffset>>, line: &str) -> bool {
        let Some(ts) = ts else { return true };
        match self.seen.get_mut(key) {
            None => {
                self.seen
                    .insert(key.to_string(), (ts, HashSet::from([line.to_string()])));
                true
            }
            Some((last, lines)) => match ts.cmp(last) {
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => lines.insert(line.to_string()),
                std::cmp::Ordering::Greater => {
                    *last = ts;
                    lines.clear();
                    lines.insert(line.to_string());
                    true
                }
            },
        }
    }
}

/// A line ready to print
#[derive(Debug, PartialEq, Eq)]
struct Rendered {
    pod: String,
    message: String,
}

/// Turn one reply into the lines to print, applying dedupe, grep, and
/// timestamp stripping
fn render_reply(
    reply: &LogsReply,
    dedup: &mut Dedup,
    grep: Option<&str>,
    timestamps: bool,
) -> Vec<Rendered> {
    let mut per_pod: HashMap<&str, usize> = HashMap::new();
    for pod in &reply.pods {
        *per_pod.entry(pod.name.as_str()).or_default() += 1;
    }
    let mut out = Vec::new();
    for pod in &reply.pods {
        let short = pod.name.split('-').next_back().unwrap_or(&pod.name);
        let label = match pod.container.as_deref() {
            Some(c) if per_pod.get(pod.name.as_str()).copied().unwrap_or(0) > 1 => {
                format!("{short}/{c}")
            }
            _ => short.to_string(),
        };
        let key = format!("{}/{}", pod.name, pod.container.as_deref().unwrap_or(""));
        // Continuation lines without a timestamp inherit the previous one
        let mut carry: Option<DateTime<FixedOffset>> = None;
        for raw in pod.log.as_deref().unwrap_or("").lines() {
            if raw.is_empty() {
                continue;
            }
            let (ts, text) = split_timestamp(raw);
            let ts = ts.or(carry);
            carry = ts;
            if !dedup.admit(&key, ts, raw) {
                continue;
            }
            if grep.is_some_and(|needle| !text.contains(needle)) {
                continue;
            }
            out.push(Rendered {
                pod: label.clone(),
                message: if timestamps { raw } else { text }.to_string(),
            });
        }
    }
    out
}

fn print_rendered(lines: &[Rendered], json: bool) {
    for line in lines {
        if json {
            match serde_json::to_string(&LogLine {
                pod: &line.pod,
                message: &line.message,
            }) {
                Ok(encoded) => println!("{encoded}"),
                Err(e) => eprintln!("{} {e}", "encode error:".yellow()),
            }
        } else {
            println!("{} {}", format!("[{}]", line.pod).dimmed(), line.message);
        }
    }
}

/// Seconds of history to request on the next poll: the time since the
/// previous request, plus overlap
fn poll_window(elapsed: Duration) -> i64 {
    let secs = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX / 2);
    secs.saturating_add(1).saturating_add(POLL_OVERLAP_SECS)
}

fn idle_notice(service: &str, follow: bool) -> String {
    let base =
        format!("No running pods for '{service}': the service is sleeping or scaled to zero.");
    if follow {
        format!("{base} Waiting for it to start...")
    } else {
        base
    }
}

/// A 404 that carries no specific message means the logs endpoint
/// itself is missing on this API, not that the service is
fn endpoint_missing(err: &anyhow::Error) -> bool {
    matches!(err.to_string().as_str(), "not found" | "NOT_FOUND")
}

/// Read logs over the hosted API. `--follow` polls every couple of
/// seconds and drops lines it has already printed.
///
/// # Errors
///
/// Returns an error if the service can't be found, the API rejects the
/// request, or polling keeps failing.
pub async fn tail_logs_api(
    project: Option<&str>,
    service: &str,
    opts: &LogOptions<'_>,
) -> Result<()> {
    // Reject a bad `--since` before any credential or network work
    let since_seconds = opts.since.map(parse_since).transpose()?;
    let client = api::ApiClient::from_env()?;
    let at = api::resolve::locate_service(&client, project, service).await?;
    let namespace = at.namespace();

    let mut started = Instant::now();
    let first: LogsReply = client
        .get(
            "/v1/services/logs",
            &log_query(
                &namespace,
                &at.project,
                service,
                opts.tail,
                since_seconds,
                opts.previous,
            ),
        )
        .await
        .map_err(|e| {
            if endpoint_missing(&e) {
                crate::transport::unavailable("logs", "use the Logs tab in the dashboard")
            } else {
                e
            }
        })?;

    if opts.json {
        let meta = LogStreamMeta {
            service: service.to_string(),
            namespace: if first.namespace.is_empty() {
                namespace.clone()
            } else {
                first.namespace.clone()
            },
            pods: first.pods.iter().map(|p| p.name.clone()).collect(),
            follow: opts.follow,
            since: opts.since.map(String::from),
            tail: opts.tail,
            grep: opts.grep.map(String::from),
        };
        println!("{}", serde_json::to_string(&meta)?);
    }

    let mut dedup = Dedup::default();
    let mut idle = first.pods.is_empty();
    if idle {
        eprintln!("{}", idle_notice(service, opts.follow).dimmed());
    }
    print_rendered(
        &render_reply(&first, &mut dedup, opts.grep, opts.timestamps),
        opts.json,
    );

    if !opts.follow {
        return Ok(());
    }

    let mut failures = 0;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let window = poll_window(started.elapsed());
        started = Instant::now();
        let reply: LogsReply = match client
            .get(
                "/v1/services/logs",
                &log_query(&namespace, &at.project, service, None, Some(window), false),
            )
            .await
        {
            Ok(r) => {
                failures = 0;
                r
            }
            Err(e) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Err(e.context("stopped following logs"));
                }
                eprintln!("{} {e}", "log poll failed:".yellow());
                continue;
            }
        };
        if reply.pods.is_empty() != idle {
            idle = reply.pods.is_empty();
            if idle {
                eprintln!("{}", idle_notice(service, true).dimmed());
            }
        }
        print_rendered(
            &render_reply(&reply, &mut dedup, opts.grep, opts.timestamps),
            opts.json,
        );
    }
}

/// Accepted `--since` format, quoted in the error for a bad value
const SINCE_FORMAT: &str = "a whole number followed by a unit: s (seconds), m (minutes), \
h (hours), or d (days), e.g. 30s, 5m, 1h, 2d";

/// Parse a `--since` value ("30s", "5m", "1h", "2d") into seconds.
///
/// # Errors
///
/// Returns an error naming the accepted format when the value has no
/// unit, an unknown unit, a non-positive or non-numeric amount, or
/// overflows.
pub fn parse_since(raw: &str) -> Result<i64> {
    let s = raw.trim();
    let invalid = || anyhow::anyhow!("invalid --since '{raw}': expected {SINCE_FORMAT}");
    let unit = s.chars().last().ok_or_else(invalid)?;
    let amount: i64 = s[..s.len() - unit.len_utf8()]
        .parse()
        .map_err(|_| invalid())?;
    if amount <= 0 {
        return Err(invalid());
    }
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(invalid()),
    };
    amount.checked_mul(scale).ok_or_else(invalid)
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
    fn parses_every_unit() {
        assert_eq!(parse_since("30s").unwrap(), 30);
        assert_eq!(parse_since("5m").unwrap(), 300);
        assert_eq!(parse_since("2h").unwrap(), 7200);
        assert_eq!(parse_since("1d").unwrap(), 86_400);
        assert_eq!(parse_since(" 5m ").unwrap(), 300);
    }

    #[test]
    fn rejects_invalid_since_with_the_accepted_format() {
        for bad in [
            "60",
            "10x",
            "",
            "m",
            "-5m",
            "0s",
            "5 m",
            "1.5h",
            "5µ",
            "9999999999999999999d",
        ] {
            let msg = parse_since(bad).unwrap_err().to_string();
            assert!(msg.contains(&format!("invalid --since '{bad}'")), "{msg}");
            assert!(msg.contains("e.g. 30s, 5m, 1h, 2d"), "{msg}");
        }
    }

    #[tokio::test]
    async fn api_transport_rejects_bare_number_since() {
        // Fails on the value before any credential or network work
        let opts = LogOptions {
            since: Some("60"),
            ..LogOptions::default()
        };
        let err = tail_logs_api(Some("demo"), "web", &opts).await.unwrap_err();
        assert!(err.to_string().contains("invalid --since '60'"), "{err}");
    }

    #[tokio::test]
    async fn kube_transport_rejects_bare_number_since() {
        // Fails on the value before building a kube client
        let err = tail_logs("web", false, Some("60"), None, None, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid --since '60'"), "{err}");
    }

    #[test]
    fn stream_meta_serializes_camelcase_and_omits_unset() {
        let meta = LogStreamMeta {
            service: "api".to_string(),
            namespace: "fractal-demo".to_string(),
            pods: vec!["api-abc".to_string(), "api-def".to_string()],
            follow: true,
            since: Some("5m".to_string()),
            tail: None,
            grep: None,
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["service"], "api");
        assert_eq!(v["namespace"], "fractal-demo");
        assert_eq!(v["pods"][1], "api-def");
        assert_eq!(v["follow"], true);
        assert_eq!(v["since"], "5m");
        let map = v.as_object().unwrap();
        assert!(!map.contains_key("tail"));
        assert!(!map.contains_key("grep"));
    }

    #[test]
    fn log_line_serializes_pod_and_message() {
        let v = serde_json::to_value(LogLine {
            pod: "abc",
            message: "hello world",
        })
        .unwrap();
        assert_eq!(v["pod"], "abc");
        assert_eq!(v["message"], "hello world");
    }

    fn reply(json: &str) -> LogsReply {
        crate::api::parse_rest(200, json).unwrap()
    }

    const TWO_PODS: &str = r#"{"service":"web","namespace":"fractal-demo","pods":[
      {"name":"web-7d9f-abc12","container":"web","log":"2026-11-13T10:00:00.1Z hello\n2026-11-13T10:00:01.12345Z GET /health 200\n"},
      {"name":"web-7d9f-def34","container":"web","log":"2026-11-13T10:00:02Z ready\n"}
    ]}"#;

    #[test]
    fn log_query_matches_contract() {
        let q = log_query("fractal-demo", "demo", "web", Some(50), Some(300), true);
        assert_eq!(
            q,
            vec![
                ("namespace", "fractal-demo".to_string()),
                ("project", "demo".to_string()),
                ("service", "web".to_string()),
                ("tailLines", "50".to_string()),
                ("sinceSeconds", "300".to_string()),
                ("timestamps", "true".to_string()),
                ("previous", "true".to_string()),
            ]
        );
        let q = log_query("ns", "p", "s", None, None, false);
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn splits_rfc3339_prefix_only() {
        let (ts, text) = split_timestamp("2026-11-13T10:00:00.123456789Z hello world");
        assert!(ts.is_some());
        assert_eq!(text, "hello world");
        let (ts, text) = split_timestamp("plain line here");
        assert!(ts.is_none());
        assert_eq!(text, "plain line here");
    }

    #[test]
    fn renders_without_timestamps_by_default() {
        let mut d = Dedup::default();
        let lines = render_reply(&reply(TWO_PODS), &mut d, None, false);
        assert_eq!(
            lines,
            vec![
                Rendered {
                    pod: "abc12".into(),
                    message: "hello".into()
                },
                Rendered {
                    pod: "abc12".into(),
                    message: "GET /health 200".into()
                },
                Rendered {
                    pod: "def34".into(),
                    message: "ready".into()
                },
            ]
        );
    }

    #[test]
    fn keeps_timestamps_when_asked_and_applies_grep() {
        let mut d = Dedup::default();
        let lines = render_reply(&reply(TWO_PODS), &mut d, Some("health"), true);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].message,
            "2026-11-13T10:00:01.12345Z GET /health 200"
        );
    }

    #[test]
    fn follow_poll_drops_lines_already_printed() {
        let mut d = Dedup::default();
        assert_eq!(render_reply(&reply(TWO_PODS), &mut d, None, false).len(), 3);
        // The next window overlaps: old lines repeat, one new line per pod
        let next = r#"{"service":"web","namespace":"fractal-demo","pods":[
          {"name":"web-7d9f-abc12","container":"web","log":"2026-11-13T10:00:01.12345Z GET /health 200\n2026-11-13T10:00:01.2Z GET / 200\n"},
          {"name":"web-7d9f-def34","container":"web","log":"2026-11-13T10:00:02Z ready\n2026-11-13T10:00:02Z ready again\n"}
        ]}"#;
        let lines = render_reply(&reply(next), &mut d, None, false);
        let msgs: Vec<&str> = lines.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(msgs, vec!["GET / 200", "ready again"]);
    }

    #[test]
    fn dedup_compares_timestamps_numerically() {
        // ".1234Z" sorts after ".12345Z" as text but is earlier in time
        let mut d = Dedup::default();
        let a = DateTime::parse_from_rfc3339("2026-11-13T10:00:00.12345Z").ok();
        let b = DateTime::parse_from_rfc3339("2026-11-13T10:00:00.1234Z").ok();
        assert!(d.admit("k", a, "x"));
        assert!(!d.admit("k", b, "y"));
    }

    #[test]
    fn labels_containers_when_a_pod_has_several() {
        let body = r#"{"service":"web","namespace":"ns","pods":[
          {"name":"web-1-aa","container":"app","log":"2026-11-13T10:00:00Z a\n"},
          {"name":"web-1-aa","container":"proxy","log":"2026-11-13T10:00:00Z b\n"}
        ]}"#;
        let lines = render_reply(&reply(body), &mut Dedup::default(), None, false);
        assert_eq!(lines[0].pod, "aa/app");
        assert_eq!(lines[1].pod, "aa/proxy");
    }

    #[test]
    fn empty_pods_means_idle_not_error() {
        let r = reply(r#"{"service":"web","namespace":"ns","pods":[]}"#);
        assert!(r.pods.is_empty());
        assert!(render_reply(&r, &mut Dedup::default(), None, false).is_empty());
        assert!(idle_notice("web", false).contains("sleeping or scaled to zero"));
        assert!(idle_notice("web", true).contains("Waiting"));
    }

    #[test]
    fn poll_window_covers_elapsed_plus_overlap() {
        assert_eq!(
            poll_window(Duration::from_millis(2100)),
            2 + 1 + POLL_OVERLAP_SECS
        );
    }

    #[test]
    fn bare_404_means_endpoint_missing() {
        assert!(endpoint_missing(&anyhow::anyhow!("not found")));
        assert!(!endpoint_missing(&anyhow::anyhow!(
            "FractalService ns/web not found"
        )));
    }
}
