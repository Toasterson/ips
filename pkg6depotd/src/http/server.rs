use crate::errors::Result;
use axum::Router;
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpListener;

/// How long in-flight requests may run after a shutdown signal before the
/// server stops anyway, leaving time to flush telemetry within SMF's stop
/// timeout.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(router: Router, listener: TcpListener) -> Result<()> {
    run_until(router, listener, std::future::pending()).await
}

/// Serve until `shutdown` resolves, then stop accepting and give in-flight
/// requests up to [`DRAIN_TIMEOUT`] to finish.
pub async fn run_until<F>(router: Router, listener: TcpListener, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let addr = listener.local_addr()?;
    tracing::info!("Listening on {}", addr);

    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown.await;
        let _ = stopping_tx.send(());
    });
    let drain_expired = async move {
        if stopping_rx.await.is_ok() {
            tokio::time::sleep(DRAIN_TIMEOUT).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        served = server.into_future() => {
            served.map_err(|e| crate::errors::DepotError::Server(e.to_string()))
        }
        _ = drain_expired => {
            tracing::warn!(
                "requests still running {}s after shutdown was requested; stopping anyway",
                DRAIN_TIMEOUT.as_secs()
            );
            Ok(())
        }
    }
}

/// Resolves on SIGTERM (SMF stop) or SIGINT.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}; only SIGINT stops the server");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("Shutdown requested; draining requests");
}
