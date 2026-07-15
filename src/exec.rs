use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, AttachParams, ListParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use fractal_operator::crd::service::FractalService;

/// Exec a command inside a service's pod.
///
/// Picks the first Running pod matching `app={service}`. Streams stdout
/// and stderr back to the user. If `cmd` is empty, defaults to a shell
/// (tries `/bin/bash`, falls back to `/bin/sh`).
///
/// # Errors
///
/// Returns an error if no Running pod is found, or if the exec channel
/// fails to open.
pub async fn exec_in_service(service: &str, cmd: &[String], tty: bool) -> Result<()> {
    let client = Client::try_default().await?;

    let svc_api: Api<FractalService> = Api::default_namespaced(client.clone());
    svc_api.get(service).await.context(format!(
        "service '{service}' not found in current namespace"
    ))?;

    let pod_api: Api<Pod> = Api::default_namespaced(client);
    let pods = pod_api
        .list(&ListParams::default().labels(&format!("app={service}")))
        .await
        .context("failed to list pods")?;

    let target = pods
        .items
        .into_iter()
        .find(|p| {
            p.status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .is_some_and(|p| p == "Running")
        })
        .ok_or_else(|| anyhow::anyhow!("no Running pod found for service '{service}'"))?;

    let pod_name = target.metadata.name.context("pod has no name")?;

    let command: Vec<String> = if cmd.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        cmd.to_vec()
    };

    println!(
        "{} Connecting to {} (cmd: {})",
        "\u{2192}".cyan(),
        pod_name.cyan(),
        command.join(" ").dimmed()
    );

    let attach = AttachParams::default()
        .stdin(true)
        .stdout(true)
        .stderr(!tty)
        .tty(tty);

    let mut attached = pod_api
        .exec(&pod_name, &command, &attach)
        .await
        .context("failed to open exec channel")?;

    // Bridge child stdout/stderr to our stdout, and our stdin to child stdin.
    let stdout = attached
        .stdout()
        .ok_or_else(|| anyhow::anyhow!("exec stdout not available"))?;
    let stdout_task = tokio::spawn(pipe_to_stdout(stdout));
    let stderr_task = attached.stderr().map(|s| tokio::spawn(pipe_to_stderr(s)));
    let stdin_task = attached.stdin().map(|s| tokio::spawn(pipe_from_stdin(s)));

    let status = attached.take_status().expect("status channel");
    let exit = status.await;

    // Cancel the I/O bridges. They'll exit on EOF anyway when the channel
    // closes, but join cleanup keeps things tidy.
    let _ = stdout_task.await;
    if let Some(t) = stderr_task {
        let _ = t.await;
    }
    if let Some(t) = stdin_task {
        t.abort();
    }

    match exit {
        Some(s) if s.status.as_deref() == Some("Success") => Ok(()),
        Some(s) => anyhow::bail!(
            "exec failed: {}",
            s.message.unwrap_or_else(|| "unknown".into())
        ),
        None => Ok(()),
    }
}

async fn pipe_to_stdout(mut stream: impl tokio::io::AsyncRead + Unpin) {
    let mut buf = vec![0u8; 4096];
    let mut out = tokio::io::stdout();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let _ = out.write_all(&buf[..n]).await;
            }
        }
    }
}

async fn pipe_to_stderr(mut stream: impl tokio::io::AsyncRead + Unpin) {
    let mut buf = vec![0u8; 4096];
    let mut err = tokio::io::stderr();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let _ = err.write_all(&buf[..n]).await;
            }
        }
    }
}

async fn pipe_from_stdin(mut stream: impl tokio::io::AsyncWrite + Unpin) {
    let mut tokio_stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    loop {
        match tokio_stdin.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
}
