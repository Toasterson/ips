//! A TLS listener for `axum::serve`.
//!
//! Handshakes run in their own tasks with a timeout, so a slow or silent
//! client cannot stall other connections, and the number of handshakes in
//! flight is capped.

use axum::serve::Listener;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// Upper bound on concurrent TLS handshakes.
const MAX_PENDING_HANDSHAKES: usize = 256;

pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl TlsListener {
    pub fn new(
        tcp: TcpListener,
        config: rustls::ServerConfig,
        handshake_timeout: Duration,
    ) -> io::Result<Self> {
        let local = tcp.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = TlsAcceptor::from(Arc::new(config));
        tokio::spawn(accept_loop(tcp, acceptor, tx, handshake_timeout));
        Ok(TlsListener { rx, local })
    }
}

async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
    handshake_timeout: Duration,
) {
    let permits = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    loop {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return;
        };
        let (stream, peer) = match tcp.accept().await {
            Ok(v) => v,
            Err(e) => {
                // EMFILE and friends: back off instead of spinning.
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if tx.is_closed() {
            return;
        }
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    let _ = tx.send((tls, peer)).await;
                }
                Ok(Err(e)) => tracing::debug!(%peer, "TLS handshake failed: {e}"),
                Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
            }
        });
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            // The accept loop only ends when the receiver is gone.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}
