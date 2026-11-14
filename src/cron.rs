//! `fractal cron` - control scheduled jobs (`serviceType: cronJob`): list
//! them, run one now, pause or resume a schedule, and read a job's runs
//! and their logs.
//!
//! The api transport uses the Fractal API's `/v1/services/cron/*` routes
//! and `GET /v1/services/logs?job=`. The kube transport does the same
//! through kubeconfig. Either way a run is requested from the operator by
//! stamping `fractal.omni.dev/run-now` on the `FractalService`; the CLI
//! never creates Jobs itself, so the Job name is known up front.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use colored::Colorize;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams, LogParams, Patch, PatchParams};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use fractal_operator::controllers::cron_run::{
    MANUAL_RUN_LABEL, RUN_NOW_ANNOTATION, RUN_NOW_MAX_AGE_SECS, manual_job_name,
};
use fractal_operator::crd::service::{FractalService, ServiceType};

use crate::api;
use crate::logs::{self, Dedup, LogsReply, PodLog};
use crate::services::service_type_label;

/// The service type `fractal cron` works on, as the API names it
const CRON_SERVICE_TYPE: &str = "cronJob";

/// Most runs the API returns in one request
pub const MAX_RUNS_LIMIT: u32 = 100;

/// Runs fetched when looking for one run by name while following it
const FOLLOW_RUNS_LIMIT: u32 = 20;

/// Lines per pod read the first time a run's logs are fetched (the API
/// maximum), so a short job's whole output is printed
const FIRST_FETCH_TAIL: i64 = 5000;

/// Label the operator's `CronJob` template stamps with the service name
const SERVICE_LABEL: &str = "app.kubernetes.io/name";

/// Label the operator stamps with the owning project
const PROJECT_LABEL: &str = "fractal.omni.dev/project";

/// Label Kubernetes puts on a Job's pods
const JOB_NAME_LABEL: &str = "batch.kubernetes.io/job-name";

/// Status of one run, as the runs endpoint reports it. A pending,
/// running, or retrying run is `Running`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

impl RunStatus {
    const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
        }
    }

    fn colored(self) -> String {
        match self {
            Self::Running => self.label().yellow().to_string(),
            Self::Succeeded => self.label().green().to_string(),
            Self::Failed => self.label().red().to_string(),
        }
    }
}

/// One run (Kubernetes Job) of a scheduled job. The JSON shape matches
/// the API's, so `--json` output is the same on both transports.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRun {
    pub name: String,
    #[serde(default)]
    pub manual: bool,
    pub status: RunStatus,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub completion_time: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
}

/// `GET /v1/services/cron/runs` reply
#[derive(Debug, Deserialize)]
struct RunsReply {
    #[serde(default)]
    runs: Vec<CronRun>,
}

/// `POST /v1/services/cron/run` reply
#[derive(Debug, Deserialize)]
struct RunRequested {
    job: String,
}

/// The service a cron request targets
#[derive(Debug, Serialize)]
struct Target<'a> {
    namespace: &'a str,
    project: &'a str,
    service: &'a str,
}

/// `POST /v1/services/cron/suspend` body
#[derive(Debug, Serialize)]
struct SuspendBody<'a> {
    #[serde(flatten)]
    target: Target<'a>,
    suspend: bool,
}

/// Query for `GET /v1/services/cron/runs`
fn runs_query(target: &Target<'_>, limit: u32) -> Vec<(&'static str, String)> {
    vec![
        ("namespace", target.namespace.to_string()),
        ("project", target.project.to_string()),
        ("service", target.service.to_string()),
        ("limit", limit.to_string()),
    ]
}

/// Query for one run's logs: the service logs query narrowed by `job`
fn run_logs_query(
    target: &Target<'_>,
    job: &str,
    tail: Option<i64>,
    since_seconds: Option<i64>,
) -> Vec<(&'static str, String)> {
    let mut q = logs::log_query(
        target.namespace,
        target.project,
        target.service,
        tail,
        since_seconds,
        false,
    );
    q.push(("job", job.to_string()));
    q
}

/// Fail fast, with a clear message, for a service that is not a cron job
fn ensure_cron(service: &str, service_type: &str) -> Result<()> {
    if service_type == CRON_SERVICE_TYPE {
        return Ok(());
    }
    let kind = if service_type.is_empty() {
        "unknown"
    } else {
        service_type
    };
    bail!(
        "service '{service}' is not a scheduled job (its type is {kind}); `fractal cron` works on cronJob services"
    )
}

/// Operations a transport provides for one scheduled job
trait CronOps {
    /// Request a run now; returns the name of the Job the operator creates
    async fn request_run(&self) -> Result<String>;
    /// Pause (`true`) or resume (`false`) the schedule
    async fn set_suspended(&self, suspend: bool) -> Result<()>;
    /// Recent runs, newest first
    async fn runs(&self, limit: u32) -> Result<Vec<CronRun>>;
    /// One run's logs, with timestamps
    async fn run_logs(
        &self,
        job: &str,
        tail: Option<i64>,
        since_seconds: Option<i64>,
    ) -> Result<LogsReply>;
}

/// A scheduled job reached through the hosted API
struct ApiCron {
    client: api::ApiClient,
    namespace: String,
    project: String,
    service: String,
}

impl ApiCron {
    async fn locate(project: Option<&str>, service: &str) -> Result<Self> {
        let client = api::ApiClient::from_env()?;
        let at = api::resolve::locate_service(&client, project, service).await?;
        ensure_cron(service, &at.service.spec.service_type)?;
        Ok(Self {
            namespace: at.namespace(),
            project: at.project,
            service: service.to_string(),
            client,
        })
    }

    fn target(&self) -> Target<'_> {
        Target {
            namespace: &self.namespace,
            project: &self.project,
            service: &self.service,
        }
    }
}

impl CronOps for ApiCron {
    async fn request_run(&self) -> Result<String> {
        let reply: RunRequested = self
            .client
            .send(Method::POST, "/v1/services/cron/run", &self.target())
            .await?;
        Ok(reply.job)
    }

    async fn set_suspended(&self, suspend: bool) -> Result<()> {
        let _: serde_json::Value = self
            .client
            .send(
                Method::POST,
                "/v1/services/cron/suspend",
                &SuspendBody {
                    target: self.target(),
                    suspend,
                },
            )
            .await?;
        Ok(())
    }

    async fn runs(&self, limit: u32) -> Result<Vec<CronRun>> {
        let reply: RunsReply = self
            .client
            .get("/v1/services/cron/runs", &runs_query(&self.target(), limit))
            .await?;
        Ok(reply.runs)
    }

    async fn run_logs(
        &self,
        job: &str,
        tail: Option<i64>,
        since_seconds: Option<i64>,
    ) -> Result<LogsReply> {
        self.client
            .get(
                "/v1/services/logs",
                &run_logs_query(&self.target(), job, tail, since_seconds),
            )
            .await
    }
}

/// A scheduled job reached through kubeconfig, in the current namespace
struct KubeCron {
    client: Client,
    namespace: String,
    project: String,
    service: String,
}

impl KubeCron {
    async fn locate(service: &str) -> Result<Self> {
        let client = Client::try_default().await?;
        let namespace = client.default_namespace().to_string();
        let api: Api<FractalService> = Api::namespaced(client.clone(), &namespace);
        let svc = api
            .get(service)
            .await
            .with_context(|| format!("service '{service}' not found in namespace '{namespace}'"))?;
        ensure_cron(service, service_type_label(&svc.spec.service_type))?;
        Ok(Self::from_service(client, namespace, service, &svc))
    }

    fn from_service(
        client: Client,
        namespace: String,
        service: &str,
        svc: &FractalService,
    ) -> Self {
        Self {
            client,
            namespace,
            project: svc.spec.project.clone(),
            service: service.to_string(),
        }
    }

    fn services(&self) -> Api<FractalService> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }
}

impl CronOps for KubeCron {
    async fn request_run(&self) -> Result<String> {
        let now = Utc::now();
        let stamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let patch = serde_json::json!({
            "metadata": { "annotations": { RUN_NOW_ANNOTATION: stamp } }
        });
        self.services()
            .patch(
                &self.service,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
            .with_context(|| format!("failed to request a run of '{}'", self.service))?;
        Ok(manual_job_name(&self.service, now.timestamp_millis()))
    }

    async fn set_suspended(&self, suspend: bool) -> Result<()> {
        let patch = serde_json::json!({
            "spec": { "deploy": { "cron": { "suspend": suspend } } }
        });
        self.services()
            .patch(
                &self.service,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
            .with_context(|| format!("failed to update the schedule of '{}'", self.service))?;
        Ok(())
    }

    async fn runs(&self, limit: u32) -> Result<Vec<CronRun>> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let selector = format!(
            "{SERVICE_LABEL}={},{PROJECT_LABEL}={}",
            self.service, self.project
        );
        let list = jobs
            .list(&ListParams::default().labels(&selector))
            .await
            .context("failed to list runs")?;
        let items = list
            .items
            .iter()
            .map(|job| serde_json::to_value(job).and_then(serde_json::from_value::<JobItem>))
            .collect::<Result<Vec<_>, _>>()
            .context("failed to read runs")?;
        Ok(summarize_runs(&items, &self.service, limit))
    }

    async fn run_logs(
        &self,
        job: &str,
        tail: Option<i64>,
        since_seconds: Option<i64>,
    ) -> Result<LogsReply> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = pods
            .list(&ListParams::default().labels(&format!("{JOB_NAME_LABEL}={job}")))
            .await
            .context("failed to list the run's pods")?;
        let mut out = Vec::new();
        for pod in list.items {
            let Some(name) = pod.metadata.name.clone() else {
                continue;
            };
            let containers = pod.spec.map(|s| s.containers).unwrap_or_default();
            let Some(container) = containers
                .iter()
                .find(|c| c.name == self.service)
                .or_else(|| containers.first())
                .map(|c| c.name.clone())
            else {
                continue;
            };
            let params = LogParams {
                container: Some(container.clone()),
                timestamps: true,
                tail_lines: tail,
                since_seconds,
                ..LogParams::default()
            };
            let log = match pods.logs(&name, &params).await {
                Ok(text) => text,
                // Still starting (400) or already deleted (404): no output yet
                Err(kube::Error::Api(status)) if matches!(status.code, 400 | 404) => String::new(),
                Err(e) => return Err(e).context(format!("failed to read logs of pod '{name}'")),
            };
            out.push(PodLog {
                name,
                container: Some(container),
                log: Some(log),
            });
        }
        Ok(LogsReply {
            namespace: self.namespace.clone(),
            pods: out,
        })
    }
}

/// A transport-specific handle on one scheduled job
enum Backend {
    Api(Box<ApiCron>),
    Kube(Box<KubeCron>),
}

impl Backend {
    async fn locate(via: Via<'_>, service: &str) -> Result<Self> {
        Ok(match via {
            Via::Api { project } => Self::Api(Box::new(ApiCron::locate(project, service).await?)),
            Via::Kube => Self::Kube(Box::new(KubeCron::locate(service).await?)),
        })
    }
}

impl CronOps for Backend {
    async fn request_run(&self) -> Result<String> {
        match self {
            Self::Api(b) => b.request_run().await,
            Self::Kube(b) => b.request_run().await,
        }
    }

    async fn set_suspended(&self, suspend: bool) -> Result<()> {
        match self {
            Self::Api(b) => b.set_suspended(suspend).await,
            Self::Kube(b) => b.set_suspended(suspend).await,
        }
    }

    async fn runs(&self, limit: u32) -> Result<Vec<CronRun>> {
        match self {
            Self::Api(b) => b.runs(limit).await,
            Self::Kube(b) => b.runs(limit).await,
        }
    }

    async fn run_logs(
        &self,
        job: &str,
        tail: Option<i64>,
        since_seconds: Option<i64>,
    ) -> Result<LogsReply> {
        match self {
            Self::Api(b) => b.run_logs(job, tail, since_seconds).await,
            Self::Kube(b) => b.run_logs(job, tail, since_seconds).await,
        }
    }
}

/// Which transport a `fractal cron` command goes through
#[derive(Clone, Copy, Debug)]
pub enum Via<'a> {
    /// The hosted API; `project` narrows the service lookup
    Api { project: Option<&'a str> },
    /// kubeconfig, in the current namespace
    Kube,
}

// The slice of a Kubernetes Job the run summary reads, in wire format so
// the kube transport reduces a Job exactly the way fractal-api does

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobItem {
    #[serde(default)]
    metadata: JobMeta,
    #[serde(default)]
    status: JobStatus,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobMeta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    creation_timestamp: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    owner_references: Vec<OwnerRef>,
}

#[derive(Debug, Default, Deserialize)]
struct OwnerRef {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobStatus {
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    completion_time: Option<String>,
    #[serde(default)]
    conditions: Vec<JobCondition>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobCondition {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    last_transition_time: Option<String>,
}

impl JobItem {
    fn condition(&self, kind: &str) -> Option<&JobCondition> {
        self.status
            .conditions
            .iter()
            .find(|c| c.kind.as_deref() == Some(kind) && c.status.as_deref() == Some("True"))
    }

    /// A run of `service`: owned by its `CronJob` (scheduled or run-now) or
    /// labelled as a manual run
    fn is_run_of(&self, service: &str) -> bool {
        let owned =
            self.metadata.owner_references.iter().any(|r| {
                r.kind.as_deref() == Some("CronJob") && r.name.as_deref() == Some(service)
            });
        owned || self.is_manual()
    }

    fn is_manual(&self) -> bool {
        self.metadata
            .labels
            .get(MANUAL_RUN_LABEL)
            .is_some_and(|v| v == "true")
    }

    /// Reduce to a run summary. Terminal only once the `Complete` or
    /// `Failed` condition is set; a failed Job has no `completionTime`, so
    /// the condition's transition time stands in for it
    fn to_run(&self) -> CronRun {
        let complete = self.condition("Complete");
        let failed = self.condition("Failed");
        let status = if complete.is_some() {
            RunStatus::Succeeded
        } else if failed.is_some() {
            RunStatus::Failed
        } else {
            RunStatus::Running
        };
        let start_time = self
            .status
            .start_time
            .clone()
            .or_else(|| self.metadata.creation_timestamp.clone());
        let completion_time = match status {
            RunStatus::Succeeded => self
                .status
                .completion_time
                .clone()
                .or_else(|| complete.and_then(|c| c.last_transition_time.clone())),
            RunStatus::Failed => failed.and_then(|c| c.last_transition_time.clone()),
            RunStatus::Running => None,
        };
        let duration_seconds = match (
            start_time.as_deref().and_then(parse_time),
            completion_time.as_deref().and_then(parse_time),
        ) {
            (Some(start), Some(end)) => Some((end - start).num_seconds().max(0)),
            _ => None,
        };
        CronRun {
            name: self.metadata.name.clone().unwrap_or_default(),
            manual: self.is_manual(),
            status,
            start_time,
            completion_time,
            duration_seconds,
        }
    }
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Runs of `service` among `jobs`, newest first (a run without a start
/// time last), at most `limit`
fn summarize_runs(jobs: &[JobItem], service: &str, limit: u32) -> Vec<CronRun> {
    let mut runs: Vec<CronRun> = jobs
        .iter()
        .filter(|j| j.is_run_of(service))
        .map(JobItem::to_run)
        .collect();
    runs.sort_by_key(|r| {
        std::cmp::Reverse(
            r.start_time
                .as_deref()
                .and_then(parse_time)
                .map(|t| t.timestamp_millis()),
        )
    });
    runs.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    runs
}

/// How long `--follow` waits, injectable so tests never sleep
struct FollowTiming {
    /// Pause between polls
    poll: Duration,
    /// How long a requested run may take to appear before giving up
    start_timeout: Duration,
    /// Overall limit from `--timeout`, if any
    timeout: Option<Duration>,
}

impl FollowTiming {
    const fn new(timeout: Option<Duration>) -> Self {
        Self {
            poll: logs::POLL_INTERVAL,
            // The operator discards a run request it could not start in
            // this window, so waiting longer is pointless
            start_timeout: Duration::from_secs(RUN_NOW_MAX_AGE_SECS.unsigned_abs()),
            timeout,
        }
    }
}

/// Wait for `job` to finish, printing its log lines as they arrive, and
/// return the finished run. Fails if the run never appears or `--timeout`
/// passes first; a log read failure only warns, since the run's status is
/// what decides the outcome.
// Driven by the CLI's `block_on`, never spawned, so the future need not be Send
#[allow(clippy::future_not_send)]
async fn follow_run<O: CronOps>(
    ops: &O,
    job: &str,
    timing: &FollowTiming,
    json: bool,
) -> Result<CronRun> {
    let begun = Instant::now();
    let mut dedup = Dedup::default();
    let mut last_fetch: Option<Instant> = None;
    loop {
        let run = ops
            .runs(FOLLOW_RUNS_LIMIT)
            .await?
            .into_iter()
            .find(|r| r.name == job);
        match run {
            None if begun.elapsed() >= timing.start_timeout => bail!(
                "run {job} did not start within {}; the operator discards a run request it cannot start in that time",
                format_duration(timing.start_timeout.as_secs())
            ),
            None => {}
            Some(run) => {
                // Read logs after the status, so a finished run's last
                // lines are always included
                let since = last_fetch.map(|t| logs::poll_window(t.elapsed()));
                let fetched_at = Instant::now();
                let tail = since.is_none().then_some(FIRST_FETCH_TAIL);
                match ops.run_logs(job, tail, since).await {
                    Ok(reply) => {
                        logs::print_rendered(
                            &logs::render_reply(&reply, &mut dedup, None, false),
                            json,
                        );
                        last_fetch = Some(fetched_at);
                    }
                    Err(e) => eprintln!("{} {e}", "log read failed:".yellow()),
                }
                if run.status.is_terminal() {
                    return Ok(run);
                }
            }
        }
        if let Some(limit) = timing.timeout {
            if begun.elapsed() >= limit {
                bail!(
                    "timed out after {} waiting for run {job}; it is still running",
                    format_duration(limit.as_secs())
                );
            }
        }
        tokio::time::sleep(timing.poll).await;
    }
}

/// The exit status a followed run maps to: success only for a run that
/// succeeded
fn run_outcome(run: &CronRun) -> Result<()> {
    match run.status {
        RunStatus::Succeeded => Ok(()),
        RunStatus::Failed => bail!("run {} failed", run.name),
        RunStatus::Running => bail!("run {} has not finished", run.name),
    }
}

/// Report a followed run's result, then turn it into the exit status
fn finish_followed(service: &str, run: &CronRun, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(run)?);
    } else {
        let took = run.duration_seconds.map_or_else(String::new, |s| {
            format!(" in {}", format_duration(s.unsigned_abs()))
        });
        let mark = if run.status == RunStatus::Succeeded {
            "\u{2713}".green()
        } else {
            "\u{2717}".red()
        };
        eprintln!(
            "{mark} {} run {} {}{took}",
            service.cyan(),
            run.name,
            run.status.colored()
        );
    }
    run_outcome(run)
}

/// `fractal cron run`: request a run now and print its Job name. With
/// `--follow`, wait for it, stream its logs, and fail unless it succeeds.
///
/// # Errors
///
/// Returns an error if the service can't be found or isn't a cron job, the
/// request is rejected, `--timeout` is invalid or passes, or (with
/// `--follow`) the run fails.
pub async fn run(
    via: Via<'_>,
    service: &str,
    follow: bool,
    timeout: Option<&str>,
    json: bool,
) -> Result<()> {
    let timeout = timeout.map(parse_timeout).transpose()?;
    let ops = Backend::locate(via, service).await?;
    let job = ops.request_run().await?;

    if json {
        println!(
            "{}",
            serde_json::json!({ "service": service, "job": job, "follow": follow })
        );
    } else {
        println!(
            "{} Run requested for {}: {}",
            "\u{2713}".green(),
            service.cyan(),
            job.bold()
        );
        if !follow {
            eprintln!(
                "{}",
                format!("Follow it with `fractal cron logs {service} --job {job} --follow`")
                    .dimmed()
            );
        }
    }
    if !follow {
        return Ok(());
    }
    let run = follow_run(&ops, &job, &FollowTiming::new(timeout), json).await?;
    finish_followed(service, &run, json)
}

/// `fractal cron pause` / `resume`
///
/// # Errors
///
/// Returns an error if the service can't be found or isn't a cron job, or
/// the change is rejected.
pub async fn set_paused(via: Via<'_>, service: &str, paused: bool, json: bool) -> Result<()> {
    let ops = Backend::locate(via, service).await?;
    ops.set_suspended(paused).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "service": service, "suspended": paused })
        );
    } else if paused {
        println!(
            "{} Paused {}: no scheduled runs start until it is resumed",
            "\u{2713}".green(),
            service.cyan()
        );
    } else {
        println!("{} Resumed {}", "\u{2713}".green(), service.cyan());
    }
    Ok(())
}

/// `fractal cron runs`: recent runs, newest first
///
/// # Errors
///
/// Returns an error if the service can't be found or isn't a cron job, or
/// the runs can't be read.
pub async fn runs(via: Via<'_>, service: &str, limit: u32, json: bool) -> Result<()> {
    let ops = Backend::locate(via, service).await?;
    let runs = ops.runs(limit).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }
    if runs.is_empty() {
        println!(
            "{}",
            format!("(no runs yet; start one with `fractal cron run {service}`)").dimmed()
        );
        return Ok(());
    }
    println!(
        "{:<48}  {:<10}  {:<9}  {:<20}  {}",
        "RUN".bold(),
        "STATUS".bold(),
        "TRIGGER".bold(),
        "STARTED".bold(),
        "DURATION".bold(),
    );
    for run in &runs {
        println!(
            "{:<48}  {:<10}  {:<9}  {:<20}  {}",
            run.name.cyan(),
            pad_colored(run.status.label(), &run.status.colored(), 10),
            trigger_label(run.manual).dimmed(),
            format_start(run.start_time.as_deref()).dimmed(),
            run.duration_seconds
                .map_or_else(|| "-".to_string(), |s| format_duration(s.unsigned_abs()))
                .dimmed(),
        );
    }
    Ok(())
}

/// `fractal cron logs`: one run's output, the newest run by default.
/// `--follow` waits for a running run to finish and fails unless it
/// succeeds.
///
/// # Errors
///
/// Returns an error if the service can't be found or isn't a cron job, it
/// has no runs, the logs can't be read, or (with `--follow`) the run fails.
pub async fn logs(
    via: Via<'_>,
    service: &str,
    job: Option<&str>,
    tail: Option<i64>,
    follow: bool,
    json: bool,
) -> Result<()> {
    let ops = Backend::locate(via, service).await?;
    let job = match job {
        Some(j) => j.to_string(),
        None => ops
            .runs(1)
            .await?
            .into_iter()
            .next()
            .map(|r| r.name)
            .with_context(|| {
                format!("'{service}' has no runs yet; start one with `fractal cron run {service}`")
            })?,
    };
    if json {
        println!(
            "{}",
            serde_json::json!({ "service": service, "job": job, "follow": follow })
        );
    } else {
        eprintln!("{}", format!("Logs for run {job}").dimmed());
    }
    if follow {
        let run = follow_run(&ops, &job, &FollowTiming::new(None), json).await?;
        return finish_followed(service, &run, json);
    }
    let reply = ops.run_logs(&job, tail, None).await?;
    if reply.pods.is_empty() {
        eprintln!(
            "{}",
            format!(
                "No pods for run {job}. A run that was just requested may still be starting; a finished run's pods are removed once it falls outside the job history limit."
            )
            .dimmed()
        );
    }
    logs::print_rendered(
        &logs::render_reply(&reply, &mut Dedup::default(), None, false),
        json,
    );
    Ok(())
}

/// One row of `fractal cron list`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CronRow {
    project: String,
    name: String,
    schedule: Option<String>,
    time_zone: Option<String>,
    suspended: bool,
    last_run: Option<CronRun>,
}

/// `fractal cron list`: every scheduled job with its schedule, whether it
/// is paused, and its last run
///
/// # Errors
///
/// Returns an error if the services or their runs can't be read.
pub async fn list(via: Via<'_>, json: bool) -> Result<()> {
    let rows = match via {
        Via::Api { project } => list_rows_api(project).await?,
        Via::Kube => list_rows_kube().await?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", "(no scheduled jobs)".dimmed());
        return Ok(());
    }
    println!(
        "{:<24}  {:<32}  {:<20}  {:<7}  {}",
        "PROJECT".bold(),
        "NAME".bold(),
        "SCHEDULE".bold(),
        "STATE".bold(),
        "LAST RUN".bold(),
    );
    for row in &rows {
        let state = if row.suspended { "paused" } else { "active" };
        let state_colored = if row.suspended {
            state.yellow().to_string()
        } else {
            state.green().to_string()
        };
        println!(
            "{:<24}  {:<32}  {:<20}  {:<7}  {}",
            row.project.dimmed(),
            row.name.cyan(),
            schedule_label(row.schedule.as_deref(), row.time_zone.as_deref()),
            pad_colored(state, &state_colored, 7),
            last_run_label(row.last_run.as_ref()),
        );
    }
    Ok(())
}

async fn list_rows_api(project: Option<&str>) -> Result<Vec<CronRow>> {
    let client = api::ApiClient::from_env()?;
    let projects = match project {
        Some(ident) => vec![api::resolve::find_project(&client, ident).await?],
        None => api::resolve::list_projects(&client).await?,
    };
    let mut rows = Vec::new();
    for proj in projects {
        let pname = proj.name().to_string();
        for svc in api::resolve::list_services(&client, &pname).await? {
            if svc.spec.service_type != CRON_SERVICE_TYPE {
                continue;
            }
            let namespace = svc
                .namespace()
                .map_or_else(|| format!("fractal-{pname}"), str::to_string);
            let target = Target {
                namespace: &namespace,
                project: &pname,
                service: svc.name(),
            };
            let reply: RunsReply = client
                .get("/v1/services/cron/runs", &runs_query(&target, 1))
                .await?;
            let deploy = svc.spec.deploy.as_ref();
            let cron = deploy.and_then(|d| d.cron.as_ref());
            rows.push(CronRow {
                project: pname.clone(),
                name: svc.name().to_string(),
                schedule: deploy.and_then(|d| d.schedule.clone()),
                time_zone: cron.and_then(|c| c.time_zone.clone()),
                suspended: cron.and_then(|c| c.suspend).unwrap_or(false),
                last_run: reply.runs.into_iter().next(),
            });
        }
    }
    Ok(rows)
}

async fn list_rows_kube() -> Result<Vec<CronRow>> {
    let client = Client::try_default().await?;
    let namespace = client.default_namespace().to_string();
    let api: Api<FractalService> = Api::namespaced(client.clone(), &namespace);
    let services = api
        .list(&ListParams::default())
        .await
        .context("failed to list FractalServices")?;
    let mut rows = Vec::new();
    for svc in &services.items {
        if svc.spec.service_type != ServiceType::CronJob {
            continue;
        }
        let Some(name) = svc.metadata.name.as_deref() else {
            continue;
        };
        let ops = KubeCron::from_service(client.clone(), namespace.clone(), name, svc);
        let cron = svc.spec.deploy.cron.as_ref();
        rows.push(CronRow {
            project: svc.spec.project.clone(),
            name: name.to_string(),
            schedule: svc.spec.deploy.schedule.clone(),
            time_zone: cron.and_then(|c| c.time_zone.clone()),
            suspended: cron.and_then(|c| c.suspend).unwrap_or(false),
            last_run: ops.runs(1).await?.into_iter().next(),
        });
    }
    Ok(rows)
}

/// Parse `--timeout` ("30s", "10m", "2h")
fn parse_timeout(raw: &str) -> Result<Duration> {
    let secs = logs::parse_since(raw).map_err(|_| {
        anyhow::anyhow!(
            "invalid --timeout '{raw}': expected a whole number followed by s, m, h, or d (e.g. 30s, 10m, 1h)"
        )
    })?;
    Ok(Duration::from_secs(secs.unsigned_abs()))
}

/// "7s", "2m 5s", "1h 3m"
fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) if s == 0 => format!("{m}m"),
        (0, _) => format!("{m}m {s}s"),
        _ if m == 0 => format!("{h}h"),
        _ => format!("{h}h {m}m"),
    }
}

/// A start time as `2026-11-14 18:01:21 UTC`, or `-`
fn format_start(raw: Option<&str>) -> String {
    raw.and_then(parse_time).map_or_else(
        || "-".to_string(),
        |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

const fn trigger_label(manual: bool) -> &'static str {
    if manual { "manual" } else { "scheduled" }
}

fn schedule_label(schedule: Option<&str>, time_zone: Option<&str>) -> String {
    match (schedule, time_zone) {
        (Some(s), Some(tz)) => format!("{s} ({tz})"),
        (Some(s), None) => s.to_string(),
        (None, _) => "-".to_string(),
    }
}

fn last_run_label(run: Option<&CronRun>) -> String {
    run.map_or_else(
        || "-".dimmed().to_string(),
        |r| {
            format!(
                "{} {}",
                r.status.colored(),
                format!(
                    "{} ({})",
                    format_start(r.start_time.as_deref()),
                    trigger_label(r.manual)
                )
                .dimmed()
            )
        },
    )
}

/// Pad a colored cell by its visible width; escape codes would otherwise
/// throw off `{:<N}`
fn pad_colored(plain: &str, colored: &str, width: usize) -> String {
    format!(
        "{colored}{}",
        " ".repeat(width.saturating_sub(plain.chars().count()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted `CronOps`: each `runs` call pops the next reply (the last
    /// one repeats), and every call is recorded
    #[derive(Default)]
    struct Fake {
        runs: Mutex<VecDeque<Vec<CronRun>>>,
        log_calls: Mutex<Vec<(Option<i64>, Option<i64>)>>,
        logs_fail: bool,
    }

    impl Fake {
        fn scripted(replies: Vec<Vec<CronRun>>) -> Self {
            Self {
                runs: Mutex::new(replies.into()),
                ..Self::default()
            }
        }
    }

    impl CronOps for Fake {
        async fn request_run(&self) -> Result<String> {
            Ok("nightly-manual-abc".to_string())
        }

        async fn set_suspended(&self, _suspend: bool) -> Result<()> {
            Ok(())
        }

        async fn runs(&self, _limit: u32) -> Result<Vec<CronRun>> {
            let mut q = self.runs.lock().unwrap();
            Ok(if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or_default()
            })
        }

        async fn run_logs(
            &self,
            _job: &str,
            tail: Option<i64>,
            since_seconds: Option<i64>,
        ) -> Result<LogsReply> {
            self.log_calls.lock().unwrap().push((tail, since_seconds));
            if self.logs_fail {
                bail!("logs unavailable");
            }
            Ok(LogsReply::default())
        }
    }

    fn run_with(name: &str, status: RunStatus) -> CronRun {
        CronRun {
            name: name.to_string(),
            manual: true,
            status,
            start_time: Some("2026-11-14T12:00:00Z".to_string()),
            completion_time: None,
            duration_seconds: None,
        }
    }

    const fn instant() -> FollowTiming {
        FollowTiming {
            poll: Duration::ZERO,
            start_timeout: Duration::from_secs(60),
            timeout: None,
        }
    }

    const JOB: &str = "nightly-manual-abc";

    #[tokio::test]
    async fn follow_waits_for_the_run_to_appear_and_finish() {
        let other = run_with("nightly-29001", RunStatus::Succeeded);
        let fake = Fake::scripted(vec![
            vec![other.clone()],
            vec![run_with(JOB, RunStatus::Running), other.clone()],
            vec![run_with(JOB, RunStatus::Running), other.clone()],
            vec![run_with(JOB, RunStatus::Succeeded), other],
        ]);
        let run = follow_run(&fake, JOB, &instant(), false).await.unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        assert!(run_outcome(&run).is_ok());
        // Logs are read once per poll after the run appears: the first
        // read takes the whole tail, later ones a time window
        let calls = fake.log_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], (Some(FIRST_FETCH_TAIL), None));
        assert!(calls[1].0.is_none() && calls[1].1.is_some());
    }

    #[tokio::test]
    async fn a_failed_run_is_a_failing_exit() {
        let fake = Fake::scripted(vec![
            vec![run_with(JOB, RunStatus::Running)],
            vec![run_with(JOB, RunStatus::Failed)],
        ]);
        let run = follow_run(&fake, JOB, &instant(), false).await.unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        let err = run_outcome(&run).unwrap_err().to_string();
        assert_eq!(err, format!("run {JOB} failed"));
        assert!(finish_followed("nightly", &run, true).is_err());
    }

    #[tokio::test]
    async fn a_log_read_failure_does_not_decide_the_outcome() {
        let fake = Fake {
            logs_fail: true,
            ..Fake::scripted(vec![vec![run_with(JOB, RunStatus::Succeeded)]])
        };
        let run = follow_run(&fake, JOB, &instant(), false).await.unwrap();
        assert!(finish_followed("nightly", &run, false).is_ok());
    }

    #[tokio::test]
    async fn a_run_that_never_appears_gives_up() {
        let fake = Fake::scripted(vec![vec![]]);
        let timing = FollowTiming {
            start_timeout: Duration::ZERO,
            ..instant()
        };
        let err = follow_run(&fake, JOB, &timing, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not start"), "{err}");
    }

    #[tokio::test]
    async fn timeout_stops_a_run_that_keeps_running() {
        let fake = Fake::scripted(vec![vec![run_with(JOB, RunStatus::Running)]]);
        let timing = FollowTiming {
            timeout: Some(Duration::ZERO),
            ..instant()
        };
        let err = follow_run(&fake, JOB, &timing, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("still running"), "{err}");
    }

    #[test]
    fn running_is_not_a_success() {
        assert!(run_outcome(&run_with(JOB, RunStatus::Running)).is_err());
    }

    #[test]
    fn parses_the_runs_reply() {
        let reply: RunsReply = api::parse_rest(
            200,
            r#"{"service":"nightly","runs":[
              {"name":"nightly-manual-mhz1abc","manual":true,"status":"Succeeded",
               "startTime":"2026-11-14T18:01:21Z","completionTime":"2026-11-14T18:01:28Z","durationSeconds":7},
              {"name":"nightly-29001","manual":false,"status":"Running",
               "startTime":"2026-11-14T03:00:00Z","completionTime":null,"durationSeconds":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(reply.runs.len(), 2);
        assert_eq!(reply.runs[0].status, RunStatus::Succeeded);
        assert_eq!(reply.runs[0].duration_seconds, Some(7));
        assert!(reply.runs[0].manual);
        assert_eq!(reply.runs[1].status, RunStatus::Running);
        assert!(reply.runs[1].completion_time.is_none());
        // --json output keeps the API's shape
        let v = serde_json::to_value(&reply.runs[0]).unwrap();
        assert_eq!(v["startTime"], "2026-11-14T18:01:21Z");
        assert_eq!(v["durationSeconds"], 7);
    }

    #[test]
    fn parses_the_run_reply() {
        let reply: RunRequested = api::parse_rest(
            202,
            r#"{"ok":true,"requestedAt":"2026-11-14T18:01:21.776Z","job":"nightly-manual-mwgppgfk"}"#,
        )
        .unwrap();
        assert_eq!(reply.job, "nightly-manual-mwgppgfk");
    }

    #[test]
    fn a_non_cron_rejection_reads_cleanly() {
        let err =
            api::parse_rest::<RunRequested>(400, r#"{"error":"Service is not a scheduled job"}"#)
                .unwrap_err()
                .to_string();
        assert_eq!(err, "invalid request: Service is not a scheduled job");
    }

    #[test]
    fn request_shapes_match_the_api() {
        let target = Target {
            namespace: "fractal-demo",
            project: "demo",
            service: "nightly",
        };
        assert_eq!(
            serde_json::to_value(SuspendBody {
                target,
                suspend: true
            })
            .unwrap(),
            serde_json::json!({ "namespace": "fractal-demo", "project": "demo", "service": "nightly", "suspend": true })
        );
        let target = Target {
            namespace: "fractal-demo",
            project: "demo",
            service: "nightly",
        };
        assert_eq!(
            runs_query(&target, 5),
            vec![
                ("namespace", "fractal-demo".to_string()),
                ("project", "demo".to_string()),
                ("service", "nightly".to_string()),
                ("limit", "5".to_string()),
            ]
        );
        let q = run_logs_query(&target, "nightly-manual-abc", Some(100), None);
        assert!(q.contains(&("job", "nightly-manual-abc".to_string())));
        assert!(q.contains(&("tailLines", "100".to_string())));
        assert!(q.contains(&("timestamps", "true".to_string())));
    }

    #[test]
    fn rejects_non_cron_services() {
        assert!(ensure_cron("nightly", "cronJob").is_ok());
        let err = ensure_cron("web", "web").unwrap_err().to_string();
        assert!(err.contains("not a scheduled job"), "{err}");
        assert!(err.contains("type is web"), "{err}");
    }

    fn job(json: serde_json::Value) -> JobItem {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn reduces_jobs_like_the_api() {
        let jobs = vec![
            job(serde_json::json!({
                "metadata": {"name": "nightly-29001", "creationTimestamp": "2026-11-14T03:00:00Z",
                  "ownerReferences": [{"kind": "CronJob", "name": "nightly"}]},
                "status": {"startTime": "2026-11-14T03:00:01Z", "completionTime": "2026-11-14T03:01:01Z",
                  "conditions": [{"type": "Complete", "status": "True", "lastTransitionTime": "2026-11-14T03:01:01Z"}]}
            })),
            job(serde_json::json!({
                "metadata": {"name": "nightly-manual-abc", "labels": {"fractal.omni.dev/manual-run": "true"}},
                "status": {"startTime": "2026-11-14T12:00:00Z",
                  "conditions": [{"type": "Failed", "status": "True", "lastTransitionTime": "2026-11-14T12:00:09Z"}]}
            })),
            job(serde_json::json!({
                "metadata": {"name": "nightly-manual-new", "creationTimestamp": "2026-11-14T13:00:00Z",
                  "labels": {"fractal.omni.dev/manual-run": "true"}}
            })),
            // A build Job that shares the labels but is not a run
            job(serde_json::json!({
                "metadata": {"name": "nightly-build-1", "creationTimestamp": "2026-11-14T14:00:00Z"}
            })),
        ];
        let runs = summarize_runs(&jobs, "nightly", 10);
        let names: Vec<&str> = runs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["nightly-manual-new", "nightly-manual-abc", "nightly-29001"]
        );
        assert_eq!(runs[0].status, RunStatus::Running);
        assert_eq!(runs[0].start_time.as_deref(), Some("2026-11-14T13:00:00Z"));
        assert_eq!(runs[1].status, RunStatus::Failed);
        assert_eq!(runs[1].duration_seconds, Some(9));
        assert!(runs[1].manual);
        assert_eq!(runs[2].status, RunStatus::Succeeded);
        assert_eq!(runs[2].duration_seconds, Some(60));
        assert!(!runs[2].manual);
        assert_eq!(summarize_runs(&jobs, "nightly", 1).len(), 1);
    }

    #[test]
    fn kube_reduction_reads_a_real_job() {
        let mut j = Job::default();
        j.metadata.name = Some("nightly-manual-abc".to_string());
        j.metadata.labels = Some(BTreeMap::from([(
            MANUAL_RUN_LABEL.to_string(),
            "true".to_string(),
        )]));
        let item: JobItem = serde_json::from_value(serde_json::to_value(&j).unwrap()).unwrap();
        let run = item.to_run();
        assert_eq!(run.name, "nightly-manual-abc");
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.manual);
    }

    #[test]
    fn timeout_takes_the_since_format() {
        assert_eq!(parse_timeout("10m").unwrap(), Duration::from_secs(600));
        let err = parse_timeout("600").unwrap_err().to_string();
        assert!(err.starts_with("invalid --timeout '600'"), "{err}");
    }

    #[test]
    fn formats_durations_and_labels() {
        assert_eq!(format_duration(7), "7s");
        assert_eq!(format_duration(125), "2m 5s");
        assert_eq!(format_duration(600), "10m");
        assert_eq!(format_duration(3780), "1h 3m");
        assert_eq!(format_duration(7200), "2h");
        assert_eq!(
            format_start(Some("2026-11-14T18:01:21Z")),
            "2026-11-14 18:01:21"
        );
        assert_eq!(format_start(None), "-");
        assert_eq!(
            schedule_label(Some("0 3 * * *"), Some("UTC")),
            "0 3 * * * (UTC)"
        );
        assert_eq!(schedule_label(None, None), "-");
        assert_eq!(pad_colored("ok", "ok", 5), "ok   ");
    }
}
