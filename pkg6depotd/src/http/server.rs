use crate::errors::Result;
use crate::tls::{CertStore, TlsListener};
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

pub async fn run(router: Router, listener: TcpListener) -> Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!("Listening on {}", addr);

    axum::serve(listener, router)
        .await
        .map_err(|e| crate::errors::DepotError::Server(e.to_string()))
}

/// Serve `router` over TLS with certificates from `store`.
pub async fn run_tls(
    router: Router,
    listener: TcpListener,
    store: Arc<CertStore>,
    handshake_timeout: Duration,
) -> Result<()> {
    let addr = listener.local_addr()?;
    let config = store.server_config()?;
    let listener = TlsListener::new(listener, config, handshake_timeout)?;
    tracing::info!("Listening on {} (HTTPS)", addr);

    axum::serve(listener, router)
        .await
        .map_err(|e| crate::errors::DepotError::Server(e.to_string()))
}
