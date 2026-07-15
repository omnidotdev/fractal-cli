use anyhow::{Context, Result};
use colored::Colorize;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use fractal_operator::crd::service::FractalService;

/// Port-forward a local port to a service's running pod.
///
/// Picks the first Running pod matching `app={service}` and listens on
/// `127.0.0.1:{local_port}`, forwarding each new connection to the pod's
/// `remote_port` over the K8s API. Matches `kubectl port-forward`.
///
/// # Errors
///
/// Returns an error if no Running pod is found or the local port cannot
/// be bound.
pub async fn port_forward(service: &str, local_port: u16, remote_port: u16) -> Result<()> {
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

    let pod_name = pods
        .items
        .into_iter()
        .find(|p| {
            p.status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .is_some_and(|p| p == "Running")
        })
        .and_then(|p| p.metadata.name)
        .ok_or_else(|| anyhow::anyhow!("no Running pod found for service '{service}'"))?;

    let listener = TcpListener::bind(("127.0.0.1", local_port))
        .await
        .context(format!("failed to bind local port {local_port}"))?;

    println!(
        "{} 127.0.0.1:{} -> {} (pod {})",
        "\u{2192}".green(),
        local_port.to_string().cyan(),
        format!("{remote_port}").cyan(),
        pod_name.dimmed(),
    );
    println!("{}", "Press Ctrl+C to stop.".dimmed());

    loop {
        let (mut local_stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing_ish_eprint(&format!("accept failed: {e}"));
                continue;
            }
        };

        let api = pod_api.clone();
        let pod = pod_name.clone();
        tokio::spawn(async move {
            let mut pf = match api.portforward(&pod, &[remote_port]).await {
                Ok(p) => p,
                Err(e) => {
                    tracing_ish_eprint(&format!("portforward open failed for {peer}: {e}"));
                    return;
                }
            };
            let Some(mut remote_stream) = pf.take_stream(remote_port) else {
                tracing_ish_eprint("portforward stream unavailable");
                return;
            };

            // Bidirectional copy. Either direction's EOF tears the other down.
            let mut buf_l = vec![0u8; 8192];
            let mut buf_r = vec![0u8; 8192];
            loop {
                tokio::select! {
                    res = local_stream.read(&mut buf_l) => match res {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if remote_stream.write_all(&buf_l[..n]).await.is_err() {
                                break;
                            }
                        }
                    },
                    res = remote_stream.read(&mut buf_r) => match res {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if local_stream.write_all(&buf_r[..n]).await.is_err() {
                                break;
                            }
                        }
                    },
                }
            }
        });
    }
}

fn tracing_ish_eprint(msg: &str) {
    eprintln!("{} {msg}", "port-forward:".yellow());
}
