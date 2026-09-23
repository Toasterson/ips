//! Plain-HTTP listener answering ACME http-01 challenges.
//!
//! It serves only `/.well-known/acme-challenge/{token}` for tokens of an
//! order in progress. Any other request for a configured domain is
//! redirected to HTTPS on the depot's HTTPS port; everything else gets 404.
//!
//! The listener faces the internet on port 80, so connections are capped
//! and each one is closed after a fixed lifetime: a slow or silent client
//! cannot hold the challenge route hostage.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::serve::Listener;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;

/// Key authorizations by token, shared with the ACME manager.
#[derive(Debug, Clone, Default)]
pub struct Http01Tokens(Arc<RwLock<HashMap<String, String>>>);

impl Http01Tokens {
    pub fn insert(&self, token: &str, key_authorization: &str) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.to_string(), key_authorization.to_string());
    }

    pub fn remove(&self, token: &str) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    pub fn get(&self, token: &str) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(token)
            .cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

#[derive(Clone)]
struct Http01State {
    tokens: Http01Tokens,
    domains: Arc<Vec<String>>,
    https_port: u16,
}

/// `https_port` is the port of the depot's HTTPS listener; redirects name
/// it unless it is 443.
pub fn router(tokens: Http01Tokens, domains: Vec<String>, https_port: u16) -> Router {
    Router::new()
        .route("/.well-known/acme-challenge/{token}", get(challenge))
        .fallback(redirect)
        .with_state(Http01State {
            tokens,
            domains: Arc::new(domains),
            https_port,
        })
}

/// Bounds on the plain-HTTP listener.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Open connections at most; further clients wait in the backlog.
    pub max_connections: usize,
    /// A connection is closed this long after it was accepted, whether its
    /// request is complete or not. A challenge answer takes milliseconds.
    pub connection_lifetime: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 128,
            connection_lifetime: Duration::from_secs(10),
        }
    }
}

/// Serve the http-01 router on `listener` in the background.
pub fn spawn(listener: TcpListener, tokens: Http01Tokens, domains: Vec<String>, https_port: u16) {
    spawn_with_limits(listener, tokens, domains, https_port, Limits::default());
}

pub fn spawn_with_limits(
    listener: TcpListener,
    tokens: Http01Tokens,
    domains: Vec<String>,
    https_port: u16,
    limits: Limits,
) {
    let app = router(tokens, domains, https_port);
    let listener = BoundedListener {
        tcp: listener,
        permits: Arc::new(Semaphore::new(limits.max_connections.max(1))),
        lifetime: limits.connection_lifetime,
    };
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http-01 listener stopped: {e}");
        }
    });
}

struct BoundedListener {
    tcp: TcpListener,
    permits: Arc<Semaphore>,
    lifetime: Duration,
}

impl Listener for BoundedListener {
    type Io = BoundedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok(permit) = self.permits.clone().acquire_owned().await else {
                return std::future::pending().await;
            };
            match self.tcp.accept().await {
                Ok((stream, peer)) => {
                    let io = BoundedStream {
                        inner: stream,
                        deadline: Box::pin(tokio::time::sleep(self.lifetime)),
                        _permit: permit,
                    };
                    return (io, peer);
                }
                Err(e) => {
                    // EMFILE and friends: back off instead of spinning.
                    tracing::warn!("http-01 accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// A connection that fails every read and write once its lifetime is over,
/// which makes hyper drop it, and that holds a connection permit until then.
struct BoundedStream {
    inner: TcpStream,
    deadline: Pin<Box<Sleep>>,
    _permit: OwnedSemaphorePermit,
}

impl BoundedStream {
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.deadline.as_mut().poll(cx).is_ready()
    }
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "http-01 connection lifetime exceeded",
    )
}

impl AsyncRead for BoundedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoundedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn challenge(State(state): State<Http01State>, Path(token): Path<String>) -> Response {
    let valid = !token.is_empty()
        && token.len() <= 256
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    match state.tokens.get(&token).filter(|_| valid) {
        Some(key_authorization) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            key_authorization,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn redirect(State(state): State<Http01State>, headers: HeaderMap, uri: Uri) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.rsplit_once(':').map_or(h, |(name, _)| name))
        .map(str::to_ascii_lowercase);
    match host {
        // Only for our own names: no open redirect to arbitrary Hosts.
        Some(host) if state.domains.contains(&host) => {
            let path = uri.path_and_query().map_or("/", |p| p.as_str());
            let authority = match state.https_port {
                443 => host,
                port => format!("{host}:{port}"),
            };
            Redirect::permanent(&format!("https://{authority}{path}")).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
