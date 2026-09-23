//! Bounded per-signal buffering and the export worker.
//!
//! The request path only ever calls [`BoundedQueue::push`]: a short mutex, a
//! `VecDeque` push and, when the queue is full, eviction of the oldest record.
//! Everything that can wait on the network runs on the telemetry runtime with
//! a timeout per attempt, a bounded number of retries and capped backoff.
//! Memory per signal is bounded by the queue capacity plus the one batch the
//! worker is sending.

use super::settings::{ExportPolicy, QueuePolicy};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::{Notify, watch};

/// Target of the exporter's own diagnostics. Kept out of exported logs.
pub(crate) const DIAG_TARGET: &str = "pkg6depotd::telemetry";

/// An OTLP signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    pub const ALL: [Signal; 3] = [Signal::Traces, Signal::Metrics, Signal::Logs];

    pub const fn as_str(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }

    /// OTLP/HTTP path appended to the collector base URL.
    pub const fn path(self) -> &'static str {
        match self {
            Signal::Traces => "/v1/traces",
            Signal::Metrics => "/v1/metrics",
            Signal::Logs => "/v1/logs",
        }
    }
}

/// Counters for one signal. Units are records (spans, log records) for traces
/// and logs, and collected snapshots for metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignalStats {
    /// Accepted into the queue (or collected, for metrics).
    pub enqueued: u64,
    /// Acknowledged by the collector.
    pub exported: u64,
    /// Evicted because the queue was full; always the oldest record.
    pub dropped_overflow: u64,
    /// Given up after the last retry failed or timed out.
    pub dropped_export_failed: u64,
    /// Still queued at the shutdown deadline, or produced after shutdown.
    pub dropped_shutdown: u64,
    /// Export attempts that failed or timed out, retries included.
    pub failed_attempts: u64,
    /// Currently waiting in the queue.
    pub queued: u64,
}

impl SignalStats {
    pub fn dropped(&self) -> u64 {
        self.dropped_overflow + self.dropped_export_failed + self.dropped_shutdown
    }
}

/// Counters of all three signals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TelemetryStats {
    pub traces: SignalStats,
    pub metrics: SignalStats,
    pub logs: SignalStats,
}

impl TelemetryStats {
    pub fn signal(&self, signal: Signal) -> &SignalStats {
        match signal {
            Signal::Traces => &self.traces,
            Signal::Metrics => &self.metrics,
            Signal::Logs => &self.logs,
        }
    }
}

/// Why a record was dropped; the `reason` attribute of the drop counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    Overflow,
    ExportFailed,
    Shutdown,
}

impl DropReason {
    pub(crate) const ALL: [DropReason; 3] = [
        DropReason::Overflow,
        DropReason::ExportFailed,
        DropReason::Shutdown,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            DropReason::Overflow => "overflow",
            DropReason::ExportFailed => "export_failed",
            DropReason::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    enqueued: AtomicU64,
    exported: AtomicU64,
    dropped_overflow: AtomicU64,
    dropped_export_failed: AtomicU64,
    dropped_shutdown: AtomicU64,
    failed_attempts: AtomicU64,
    /// Whether the last attempt failed; only transitions are logged.
    failing: AtomicBool,
}

impl Counters {
    pub(crate) fn enqueued(&self, n: u64) {
        self.enqueued.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn exported(&self, n: u64) {
        self.exported.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn dropped(&self, reason: DropReason, n: u64) {
        let counter = match reason {
            DropReason::Overflow => &self.dropped_overflow,
            DropReason::ExportFailed => &self.dropped_export_failed,
            DropReason::Shutdown => &self.dropped_shutdown,
        };
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn dropped_by(&self, reason: DropReason) -> u64 {
        match reason {
            DropReason::Overflow => self.dropped_overflow.load(Ordering::Relaxed),
            DropReason::ExportFailed => self.dropped_export_failed.load(Ordering::Relaxed),
            DropReason::Shutdown => self.dropped_shutdown.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn exported_total(&self) -> u64 {
        self.exported.load(Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self, queued: usize) -> SignalStats {
        SignalStats {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            exported: self.exported.load(Ordering::Relaxed),
            dropped_overflow: self.dropped_overflow.load(Ordering::Relaxed),
            dropped_export_failed: self.dropped_export_failed.load(Ordering::Relaxed),
            dropped_shutdown: self.dropped_shutdown.load(Ordering::Relaxed),
            failed_attempts: self.failed_attempts.load(Ordering::Relaxed),
            queued: queued as u64,
        }
    }

    fn attempt_failed(&self, signal: Signal, error: &str) {
        self.failed_attempts.fetch_add(1, Ordering::Relaxed);
        if !self.failing.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: DIAG_TARGET,
                signal = signal.as_str(),
                error = %truncate(error, 300),
                "OTLP export failing; records are buffered and dropped oldest-first when full"
            );
        }
    }

    fn attempt_succeeded(&self, signal: Signal) {
        if self.failing.swap(false, Ordering::Relaxed) {
            tracing::info!(target: DIAG_TARGET, signal = signal.as_str(), "OTLP export recovered");
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

thread_local! {
    static ON_TELEMETRY_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as exporting telemetry. Records produced on such
/// a thread (HTTP client logs, exporter diagnostics) are not exported, which
/// rules out feedback loops independently of target filters.
pub(crate) fn mark_telemetry_thread() {
    ON_TELEMETRY_THREAD.with(|flag| flag.set(true));
}

pub(crate) fn on_telemetry_thread() -> bool {
    ON_TELEMETRY_THREAD.with(Cell::get)
}

/// Sets the telemetry-thread mark for a scope on a thread we do not own
/// (the SDK's periodic metric reader).
pub(crate) struct TelemetryThreadScope(bool);

impl TelemetryThreadScope {
    pub(crate) fn enter() -> Self {
        Self(ON_TELEMETRY_THREAD.with(|flag| flag.replace(true)))
    }
}

impl Drop for TelemetryThreadScope {
    fn drop(&mut self) {
        let previous = self.0;
        ON_TELEMETRY_THREAD.with(|flag| flag.set(previous));
    }
}

/// Fixed-capacity FIFO. When full, `push` evicts the oldest record.
#[derive(Debug)]
pub(crate) struct BoundedQueue<T> {
    items: Mutex<VecDeque<T>>,
    capacity: usize,
    batch_size: usize,
    counters: Arc<Counters>,
    wake: Notify,
    closed: AtomicBool,
}

impl<T> BoundedQueue<T> {
    pub(crate) fn new(policy: &QueuePolicy, counters: Arc<Counters>) -> Self {
        Self {
            items: Mutex::new(VecDeque::with_capacity(policy.capacity)),
            capacity: policy.capacity,
            batch_size: policy.batch_size,
            counters,
            wake: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<T>> {
        // A panic while holding the lock cannot leave the deque inconsistent.
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Never blocks on I/O. Wakes the worker once a full batch is ready.
    pub(crate) fn push(&self, item: T) {
        if self.closed.load(Ordering::Acquire) {
            self.counters.dropped(DropReason::Shutdown, 1);
            return;
        }
        let (len, evicted) = {
            let mut items = self.lock();
            let evicted = if items.len() >= self.capacity {
                items.pop_front()
            } else {
                None
            };
            items.push_back(item);
            (items.len(), evicted)
        };
        // Drop the evicted record outside the lock.
        if evicted.is_some() {
            self.counters.dropped(DropReason::Overflow, 1);
        }
        self.counters.enqueued(1);
        if len >= self.batch_size {
            self.wake.notify_one();
        }
    }

    /// Up to `batch_size` records, oldest first.
    pub(crate) fn take_batch(&self) -> Vec<T> {
        let mut items = self.lock();
        let n = items.len().min(self.batch_size);
        items.drain(..n).collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Refuse further records and count what is left as dropped.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let left = std::mem::take(&mut *self.lock());
        self.counters
            .dropped(DropReason::Shutdown, left.len() as u64);
    }

    pub(crate) fn stats(&self) -> SignalStats {
        self.counters.snapshot(self.len())
    }

    pub(crate) fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }
}

/// Shutdown broadcast: `None` while running, the flush deadline afterwards.
pub(crate) type ShutdownRx = watch::Receiver<Option<Instant>>;

/// Resolves once shutdown was requested and its deadline has passed.
pub(crate) async fn deadline_passed(mut rx: ShutdownRx) {
    let Ok(Some(deadline)) = rx.wait_for(Option::is_some).await.map(|d| *d) else {
        // Sender gone without a deadline: nobody is waiting for a flush.
        return;
    };
    tokio::time::sleep_until(deadline.into()).await;
}

async fn shutdown_requested(mut rx: ShutdownRx) {
    let _ = rx.wait_for(Option::is_some).await;
}

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Run `attempt` until it succeeds, retries are used up, or shutdown's
/// deadline passes. Every attempt is bounded by `policy.timeout`; once
/// shutdown is requested no further retries are made, and a permanent
/// rejection is never retried.
pub(crate) async fn export_with_retry<'a, F>(
    signal: Signal,
    policy: &ExportPolicy,
    counters: &Counters,
    shutdown: &ShutdownRx,
    mut attempt: F,
) -> bool
where
    F: FnMut() -> BoxFuture<'a, OTelSdkResult>,
{
    let mut backoff = policy.initial_backoff;
    let mut retries = 0;
    loop {
        let outcome = tokio::select! {
            r = tokio::time::timeout(policy.timeout, attempt()) => match r {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("timed out after {:?}", policy.timeout)),
            },
            _ = deadline_passed(shutdown.clone()) => Err("shutdown deadline reached".to_string()),
        };
        let permanent = match outcome {
            Ok(()) => {
                counters.attempt_succeeded(signal);
                return true;
            }
            Err(e) => {
                counters.attempt_failed(signal, &e);
                is_permanent_rejection(&e)
            }
        };
        if permanent || retries >= policy.max_retries || shutdown.borrow().is_some() {
            return false;
        }
        retries += 1;
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown_requested(shutdown.clone()) => return false,
        }
        backoff = (backoff * 2).min(policy.max_backoff);
    }
}

/// Whether a failed attempt was a 4xx answer that resending the same payload
/// cannot fix (bad request, auth, wrong path, too large). 408 and 429 are
/// transient per the OTLP/HTTP spec and are retried like network errors.
///
/// The 0.31 exporter only reports errors as text: the reqwest client turns
/// non-2xx into `reqwest::Error { kind: Status(400, ..) }`, a non-erroring
/// client would give `Status Code: 400`. Anything unrecognised is retried.
fn is_permanent_rejection(error: &str) -> bool {
    let status = ["Status(", "Status Code: "].iter().find_map(|marker| {
        let rest = &error[error.find(marker)? + marker.len()..];
        let digits = rest
            .get(..3)
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))?;
        digits.parse::<u16>().ok()
    });
    matches!(status, Some(400..=499)) && !matches!(status, Some(408 | 429))
}

/// Sends one batch of queued records; implemented per signal.
pub(crate) trait BatchExport<T>: Send + Sync + 'static {
    /// The returned future owns what it needs, so it can be retried and raced
    /// against timeouts.
    fn export(&self, batch: Arc<Vec<T>>) -> BoxFuture<'static, OTelSdkResult>;
}

/// Drain `queue` into `exporter` until shutdown; then flush what is left
/// until the deadline and close the queue.
pub(crate) async fn run_worker<T, E>(
    signal: Signal,
    queue: Arc<BoundedQueue<T>>,
    exporter: E,
    policy: ExportPolicy,
    flush_interval: std::time::Duration,
    shutdown: ShutdownRx,
) where
    T: Send + Sync + 'static,
    E: BatchExport<T>,
{
    let counters = queue.counters.clone();
    let send = |batch: Vec<T>| {
        let batch = Arc::new(batch);
        let n = batch.len() as u64;
        let exporter = &exporter;
        let counters = &counters;
        let shutdown = &shutdown;
        async move {
            let ok = export_with_retry(signal, &policy, counters, shutdown, || {
                exporter.export(batch.clone())
            })
            .await;
            if ok {
                counters.exported(n);
            } else {
                counters.dropped(DropReason::ExportFailed, n);
            }
        }
    };

    while shutdown.borrow().is_none() {
        if queue.len() < queue.batch_size {
            tokio::select! {
                _ = queue.wake.notified() => {}
                _ = tokio::time::sleep(flush_interval) => {}
                _ = shutdown_requested(shutdown.clone()) => break,
            }
        }
        loop {
            let batch = queue.take_batch();
            if batch.is_empty() {
                break;
            }
            send(batch).await;
            if shutdown.borrow().is_some() {
                break;
            }
        }
    }

    // Shutdown: one pass over what is queued, bounded by the deadline.
    let deadline = shutdown.borrow().unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        let batch = queue.take_batch();
        if batch.is_empty() {
            break;
        }
        send(batch).await;
    }
    queue.close();
}

/// Adapter for SDK exporters that report `OTelSdkError`.
pub(crate) fn sdk_error(message: impl Into<String>) -> OTelSdkError {
    OTelSdkError::InternalFailure(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn policy(capacity: usize, batch_size: usize) -> QueuePolicy {
        QueuePolicy {
            capacity,
            batch_size,
            flush_interval: Duration::from_millis(20),
        }
    }

    fn export_policy(timeout_ms: u64, max_retries: u32) -> ExportPolicy {
        ExportPolicy {
            timeout: Duration::from_millis(timeout_ms),
            max_retries,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
        }
    }

    #[test]
    fn overflow_evicts_the_oldest_record_and_counts_it() {
        let counters = Arc::new(Counters::default());
        let q = BoundedQueue::new(&policy(4, 4), counters.clone());
        for i in 0..10 {
            q.push(i);
        }
        let stats = q.stats();
        assert_eq!(stats.enqueued, 10);
        assert_eq!(stats.dropped_overflow, 6);
        assert_eq!(stats.queued, 4);
        assert_eq!(q.take_batch(), vec![6, 7, 8, 9]);
    }

    #[test]
    fn take_batch_is_fifo_and_bounded() {
        let q = BoundedQueue::new(&policy(10, 3), Arc::new(Counters::default()));
        for i in 0..5 {
            q.push(i);
        }
        assert_eq!(q.take_batch(), vec![0, 1, 2]);
        assert_eq!(q.take_batch(), vec![3, 4]);
        assert!(q.take_batch().is_empty());
    }

    #[test]
    fn closed_queue_drops_and_counts() {
        let q = BoundedQueue::new(&policy(10, 3), Arc::new(Counters::default()));
        q.push(1);
        q.push(2);
        q.close();
        q.push(3);
        let stats = q.stats();
        assert_eq!(stats.dropped_shutdown, 3);
        assert_eq!(stats.queued, 0);
    }

    /// Fails the first `fail` calls, hangs if `hang`, records batches.
    struct Fake {
        calls: Arc<AtomicUsize>,
        fail: usize,
        hang: bool,
        seen: Arc<Mutex<Vec<Vec<u32>>>>,
    }

    impl BatchExport<u32> for Fake {
        fn export(&self, batch: Arc<Vec<u32>>) -> BoxFuture<'static, OTelSdkResult> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = call < self.fail;
            let hang = self.hang;
            let seen = self.seen.clone();
            Box::pin(async move {
                if hang {
                    std::future::pending::<()>().await;
                }
                if fail {
                    return Err(sdk_error("boom"));
                }
                seen.lock().unwrap().push(batch.to_vec());
                Ok(())
            })
        }
    }

    type Seen = Arc<Mutex<Vec<Vec<u32>>>>;

    fn fake(fail: usize, hang: bool) -> (Fake, Arc<AtomicUsize>, Seen) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Fake {
                calls: calls.clone(),
                fail,
                hang,
                seen: seen.clone(),
            },
            calls,
            seen,
        )
    }

    #[tokio::test]
    async fn retries_are_finite_and_success_is_counted() {
        let counters = Arc::new(Counters::default());
        let q = Arc::new(BoundedQueue::new(&policy(16, 4), counters.clone()));
        let (tx, rx) = watch::channel(None);
        // Two failures, then success: within max_retries = 2.
        let (exporter, calls, seen) = fake(2, false);
        let worker = tokio::spawn(run_worker(
            Signal::Logs,
            q.clone(),
            exporter,
            export_policy(200, 2),
            Duration::from_millis(10),
            rx,
        ));
        for i in 0..4 {
            q.push(i);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(Some(Instant::now() + Duration::from_millis(200)))
            .unwrap();
        worker.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(*seen.lock().unwrap(), vec![vec![0, 1, 2, 3]]);
        let stats = q.stats();
        assert_eq!(stats.exported, 4);
        assert_eq!(stats.failed_attempts, 2);
        assert_eq!(stats.dropped(), 0);
    }

    #[test]
    fn only_4xx_answers_other_than_408_and_429_are_permanent() {
        let reqwest = |code: u16| {
            format!(
                "reqwest::Error {{ kind: Status({code}, None), url: \"http://127.0.0.1:4318/v1/logs\" }}"
            )
        };
        for code in [400, 401, 403, 404, 413, 415] {
            assert!(is_permanent_rejection(&reqwest(code)), "{code}");
        }
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(!is_permanent_rejection(&reqwest(code)), "{code}");
        }
        assert!(is_permanent_rejection(
            "OpenTelemetry trace export failed. Url: x, Status Code: 400, Response: b\"\""
        ));
        assert!(!is_permanent_rejection("timed out after 5s"));
        assert!(!is_permanent_rejection(
            "reqwest::Error { kind: Request, source: ConnectError(\"tcp connect error\") }"
        ));
    }

    #[tokio::test]
    async fn permanent_rejection_is_not_retried() {
        struct Rejecting(Arc<AtomicUsize>);
        impl BatchExport<u32> for Rejecting {
            fn export(&self, _: Arc<Vec<u32>>) -> BoxFuture<'static, OTelSdkResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(sdk_error(
                        "reqwest::Error { kind: Status(400, None), url: \"http://c/v1/logs\" }",
                    ))
                })
            }
        }
        let q = Arc::new(BoundedQueue::new(
            &policy(16, 2),
            Arc::new(Counters::default()),
        ));
        let (tx, rx) = watch::channel(None);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = tokio::spawn(run_worker(
            Signal::Logs,
            q.clone(),
            Rejecting(calls.clone()),
            export_policy(200, 5),
            Duration::from_millis(10),
            rx,
        ));
        q.push(1);
        q.push(2);
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(Some(Instant::now())).unwrap();
        worker.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no retries after a 400");
        let stats = q.stats();
        assert_eq!(stats.failed_attempts, 1);
        assert_eq!(stats.dropped_export_failed, 2);
    }

    #[tokio::test]
    async fn exhausted_retries_drop_the_batch() {
        let q = Arc::new(BoundedQueue::new(
            &policy(16, 2),
            Arc::new(Counters::default()),
        ));
        let (tx, rx) = watch::channel(None);
        let (exporter, calls, _) = fake(usize::MAX, false);
        let worker = tokio::spawn(run_worker(
            Signal::Traces,
            q.clone(),
            exporter,
            export_policy(200, 1),
            Duration::from_millis(10),
            rx,
        ));
        q.push(1);
        q.push(2);
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(Some(Instant::now())).unwrap();
        worker.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "1 attempt + 1 retry");
        assert_eq!(q.stats().dropped_export_failed, 2);
    }

    #[tokio::test]
    async fn hung_exporter_is_bounded_by_timeout_and_shutdown_deadline() {
        let q = Arc::new(BoundedQueue::new(
            &policy(8, 2),
            Arc::new(Counters::default()),
        ));
        let (tx, rx) = watch::channel(None);
        let (exporter, _, _) = fake(0, true);
        let worker = tokio::spawn(run_worker(
            Signal::Logs,
            q.clone(),
            exporter,
            // Long per-attempt timeout: only the shutdown deadline ends it.
            export_policy(60_000, 10),
            Duration::from_millis(10),
            rx,
        ));
        for i in 0..20 {
            q.push(i);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = Instant::now();
        tx.send(Some(Instant::now() + Duration::from_millis(100)))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker must stop at the deadline")
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let stats = q.stats();
        assert_eq!(stats.exported, 0);
        assert_eq!(stats.queued, 0);
        assert_eq!(stats.enqueued, 20);
        assert_eq!(stats.dropped(), 20, "{stats:?}");
        assert!(stats.dropped_overflow >= 10);
    }
}
