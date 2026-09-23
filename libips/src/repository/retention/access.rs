//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Durable accounting of qualifying accesses, and proof that it is complete.
//!
//! Manifest and payload GETs served by the depot qualify; catalog, search,
//! info and web UI requests, and the repository's own scans, do not. Each
//! depot (a *recorder*, named by host and listen address) appends to
//! `<repo>/retention/access/<YYYYMMDD>.log` (UTC day of the record):
//!
//! ```text
//! <unix-seconds> TAB m TAB <publisher> TAB pkg://<publisher>/<name>@<version>
//! <unix-seconds> TAB f TAB <publisher> TAB <payload-hash>
//! <unix-seconds> TAB s TAB <recorder> TAB <run> TAB 0 TAB <fence>|- TAB <after> TAB <failed-since>|-
//! <unix-seconds> TAB h|e TAB <recorder> TAB <run> TAB <seq> TAB <fence>|-
//! <unix-seconds> TAB x TAB <recorder> TAB <operator>
//! ```
//!
//! `<after>` chains a run to the recorder's previous one: `<run>:<seq>` of
//! its last control line, `-` for a recorder that never recorded before, or
//! `?` if the recorder could not read the records when it started.
//! `<failed-since>` (unix seconds) is set when the new run follows failed
//! writes of the same process.
//!
//! Access lines are written at most once per version (or payload) and day.
//! Control lines prove that nothing was lost: a recorder starts a *run* with
//! `s`, writes an `h` heartbeat every minute and as the first line of every
//! day log it writes to, and ends a clean shutdown with `e`. Every control
//! line carries the next sequence number of its run and means "no access
//! record of this run failed since my previous control line". A failed
//! write ends the run on the spot (no further heartbeats), and the recorder
//! starts a new run once writing works again. Reading the records back:
//!
//! - a run followed by a new run of the same recorder without `e` (a
//!   crash, or a failed write), a new run whose `<after>` is not the last
//!   control line read of that recorder, a sequence number that skips (a
//!   deleted or truncated day log, even one holding whole runs), a run
//!   started after failed writes (from `<failed-since>`), or an unreadable
//!   line is a *gap*: accesses in it may be missing;
//! - a run whose last line is older than ten minutes is *stale*: its depot
//!   may be alive but unable to write anything (a full disk), so nothing
//!   about access since then is proven;
//! - `x` is an operator's statement that a stale recorder is gone
//!   (`pkg6repo retire close-depot`); the time up to it is a gap. If the
//!   recorder writes again afterwards, it was not gone: the time from `x` to
//!   its next record is a gap as well.
//!
//! Each recorder also keeps `head.<hex of recorder>` next to the logs,
//! atomically replaced after every control line it appends:
//!
//! ```text
//! <recorder> TAB <run> TAB <seq> TAB s|h|e TAB <unix-seconds>
//! ```
//!
//! Reading the records back, a recorder whose logs never reach the
//! position its head names lost its newest records (deleted logs, even of
//! runs that ended cleanly while no depot runs): the time from its last
//! record read up to the head is a gap, and the head stands in for its
//! state, so a new run chains to it and the gap stays after a restart. A
//! recorder with records but no head is a gap up to its last record. A
//! failed head update fails the run like a failed record. Deleting heads
//! and logs together is deliberate tampering and is not detected.
//!
//! Access history is *proven* from the first run start or the end of the
//! latest gap, whichever is later, provided no run is stale. Retirement for
//! inactivity counts only proven history (see [`AccessLog::coverage`]).
//! Periods in which no depot runs are covered: this assumes pkg6depotd is
//! the only server of the repository.
//!
//! Appends are single `write(2)` calls with `O_APPEND`, so concurrent depot
//! processes interleave whole lines. Manifest and control lines are
//! `fdatasync`ed; a control line's sync also flushes the payload lines
//! before it, and a control line in a new day log first syncs the previous
//! day log if it holds unsynced payload lines. Access lines are appended under a shared `flock(2)` of
//! `fence.lock` (never waited for: while apply holds it exclusively, records
//! are queued in memory and appended afterwards), so the final check of an
//! apply sees every access recorded before its generation switch
//! ([`AccessFence`]). Control lines need no lock, so recorders can confirm
//! the fence while apply holds it. HTTP requests never fail because of
//! recording and never wait for an apply, but a request that writes a
//! record waits for its `fdatasync`.
//!
//! The writer lock holder folds day logs older than yesterday into
//! `summary.json` (atomic replace, noting the last folded day), then deletes
//! them; a crash in between leaves logs that the next fold deletes unread.
//! Access records live outside the generations, so recording an access
//! never changes repository content or catalogs.

use super::{IoContext, Result, RetentionError, clock};
use crate::repository::generation::GenerationStore;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::warn;

/// Directory below the repository root.
pub const ACCESS_DIR: &str = "retention/access";
/// Folded access records.
pub const SUMMARY_FILE: &str = "summary.json";
/// Nonce of the latest apply fence, confirmed by recorders.
pub const FENCE_FILE: &str = "fence";
/// Shared by access appends, exclusive during an apply's final check.
pub const FENCE_LOCK_FILE: &str = "fence.lock";
/// A running recorder writes a heartbeat at least this often.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// How often a recorder's heartbeat thread checks for a fence.
pub const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// A run whose last line is older than this is stale.
pub const STALE_AFTER_SECS: i64 = 600;
/// Access records held back during a fence before the run is given up.
const MAX_PENDING: usize = 100_000;
/// Prefix of a recorder's head file.
const HEAD_PREFIX: &str = "head.";

/// What was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    /// `manifest/0` of an exact package version.
    Manifest,
    /// `file/0` or `file/1` of a payload.
    Payload,
}

impl AccessKind {
    fn tag(self) -> &'static str {
        match self {
            AccessKind::Manifest => "m",
            AccessKind::Payload => "f",
        }
    }
}

/// Result of [`AccessRecorder::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Appended to the day log.
    Written,
    /// Held back while an apply holds the fence; appended after it.
    Queued,
    /// Already recorded today, or not recordable (unmanaged repository,
    /// unusable key, recorder shut down).
    Skipped,
    /// The write failed. The run ended, so access history is no longer
    /// proven from before the next successful write.
    Failed,
}

pub fn access_dir(repo: &Path) -> PathBuf {
    repo.join(ACCESS_DIR)
}

fn day_file(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{}.log", day.format("%Y%m%d")))
}

fn head_file(dir: &Path, instance: &str) -> PathBuf {
    let hex: String = instance.bytes().map(|b| format!("{:02x}", b)).collect();
    dir.join(format!("{}{}", HEAD_PREFIX, hex))
}

/// The newest control line a recorder appended, kept outside the day logs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    instance: String,
    run: String,
    seq: u64,
    ended: bool,
    at: i64,
}

impl Head {
    fn parse(text: &str) -> Option<Self> {
        let line = text.strip_suffix('\n')?;
        let [instance, run, seq, tag, at] = line.split('\t').collect::<Vec<_>>()[..] else {
            return None;
        };
        let ended = match tag {
            "e" => true,
            "s" | "h" => false,
            _ => return None,
        };
        let at = at.parse::<i64>().ok().filter(|t| ts(*t).is_some())?;
        let seq = seq.parse().ok()?;
        (usable(instance) && usable(run)).then(|| Head {
            instance: instance.to_string(),
            run: run.to_string(),
            seq,
            ended,
            at,
        })
    }

    /// Replace the head atomically and durably.
    fn write(&self, dir: &Path) -> Result<()> {
        let path = head_file(dir, &self.instance);
        let tmp = dir.join(format!(
            ".{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\n",
            self.instance,
            self.run,
            self.seq,
            if self.ended { "e" } else { "h" },
            self.at
        );
        {
            let mut f = File::create(&tmp).at(&tmp)?;
            f.write_all(line.as_bytes()).at(&tmp)?;
            f.sync_all().at(&tmp)?;
        }
        fs::rename(&tmp, &path).at(&path)?;
        File::open(dir).and_then(|d| d.sync_all()).at(dir)
    }
}

/// Every recorder's head. A head that cannot be read is refused rather
/// than ignored: it may be the only trace of deleted records.
fn read_heads(dir: &Path) -> Result<Vec<Head>> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).at(dir),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.at(dir)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(HEAD_PREFIX) {
            continue;
        }
        let path = entry.path();
        let text = fs::read_to_string(&path).at(&path)?;
        let head = Head::parse(&text)
            .filter(|h| head_file(dir, &h.instance) == path)
            .ok_or_else(|| {
                RetentionError::Corrupt(format!("{}: unreadable access head", path.display()))
            })?;
        out.push(head);
    }
    Ok(out)
}

fn ts(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

fn at_secs(secs: i64) -> DateTime<Utc> {
    ts(secs).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn usable(s: &str) -> bool {
    !s.is_empty() && !s.contains(['\t', '\n', '\r'])
}

/// A token unique to this process and call.
fn new_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn append_line(path: &Path, line: &str, sync: bool) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .at(path)?;
    f.write_all(line.as_bytes()).at(path)?;
    if sync {
        sync_data(&f, path)?;
    }
    Ok(())
}

/// `fdatasync` a log that already exists.
fn sync_log(path: &Path) -> Result<()> {
    sync_data(&File::open(path).at(path)?, path)
}

fn sync_data(f: &File, path: &Path) -> Result<()> {
    f.sync_data().at(path)?;
    #[cfg(test)]
    tests::SYNCED.with(|s| s.borrow_mut().push(path.to_path_buf()));
    Ok(())
}

fn read_fence(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(FENCE_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| usable(s))
}

fn open_fence_lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir).at(dir)?;
    let path = dir.join(FENCE_LOCK_FILE);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .at(&path)
}

struct Run {
    id: String,
    seq: u64,
    last_write: DateTime<Utc>,
    /// Day log that holds this run's latest control line.
    link_day: NaiveDate,
    /// Day log holding payload lines not yet synced.
    unsynced: Option<NaiveDate>,
    fence: Option<String>,
}

/// What a new run's `s` line chains to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChainRef {
    /// The recorder never recorded before.
    First,
    /// The last control line of the recorder's previous run.
    After { run: String, seq: u64 },
    /// The records could not be read; never matches, so always a gap.
    Unknown,
}

impl ChainRef {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "-" => Some(ChainRef::First),
            "?" => Some(ChainRef::Unknown),
            _ => {
                let (run, seq) = s.rsplit_once(':')?;
                Some(ChainRef::After {
                    run: run.to_string(),
                    seq: seq.parse().ok()?,
                })
            }
        }
    }
}

impl std::fmt::Display for ChainRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainRef::First => f.write_str("-"),
            ChainRef::After { run, seq } => write!(f, "{}:{}", run, seq),
            ChainRef::Unknown => f.write_str("?"),
        }
    }
}

struct PendingLine {
    at: DateTime<Utc>,
    kind: AccessKind,
    publisher: String,
    key: String,
}

struct RecorderState {
    day: Option<NaiveDate>,
    seen: HashSet<(AccessKind, String, String)>,
    /// `None`: not started, or ended by a failed write.
    run: Option<Run>,
    /// Previous run of this process; `None` before the first run, when it
    /// is read from the records.
    chain: Option<ChainRef>,
    /// Earliest time from which records may have been lost to failed
    /// writes not yet followed by a new run.
    failed_since: Option<DateTime<Utc>>,
    pending: Vec<PendingLine>,
    /// After a run start whose head could not be written, no new run starts
    /// before this: each attempt appends an `s` line.
    retry_at: Option<DateTime<Utc>>,
    lock: Option<File>,
    last_warning: Option<Instant>,
    closed: bool,
}

/// Records qualifying accesses for a depot. Cheap to call on every request:
/// each (kind, publisher, key) is written at most once per UTC day, and
/// failures are logged (rate-limited) but never fail the request. A request
/// that writes a record waits for its sync, never for an apply's fence.
/// A depot starts [`AccessRecorder::spawn_heartbeat`] once it serves, and
/// dropping the recorder (or [`AccessRecorder::shutdown`]) ends its run
/// cleanly.
pub struct AccessRecorder {
    repo: PathBuf,
    instance: String,
    state: Mutex<RecorderState>,
}

impl AccessRecorder {
    /// A recorder named `instance`, which must stay the same across restarts
    /// of the same depot (a restart then closes the previous run) and differ
    /// between depots serving the repository at the same time.
    pub fn new(repo: &Path, instance: &str) -> Self {
        let instance: String = instance
            .chars()
            .map(|c| if c.is_whitespace() { '_' } else { c })
            .collect();
        Self {
            repo: repo.to_path_buf(),
            instance: if instance.is_empty() {
                "depot".to_string()
            } else {
                instance
            },
            state: Mutex::new(RecorderState {
                day: None,
                seen: HashSet::new(),
                run: None,
                chain: None,
                failed_since: None,
                pending: Vec::new(),
                retry_at: None,
                lock: None,
                last_warning: None,
                closed: false,
            }),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Record an access now. Only generation-managed repositories keep
    /// access records.
    pub fn record(&self, kind: AccessKind, publisher: &str, key: &str) -> Recorded {
        if !usable(publisher) || !usable(key) {
            return Recorded::Skipped;
        }
        let at = clock::now();
        let Ok(mut st) = self.state.lock() else {
            return Recorded::Failed;
        };
        if st.closed {
            return Recorded::Skipped;
        }
        let day = at.date_naive();
        if st.day != Some(day) {
            st.day = Some(day);
            st.seen.clear();
        }
        let id = (kind, publisher.to_string(), key.to_string());
        if st.seen.contains(&id) {
            return Recorded::Skipped;
        }
        if !GenerationStore::is_managed(&self.repo) {
            return Recorded::Skipped;
        }
        let line = PendingLine {
            at,
            kind,
            publisher: publisher.to_string(),
            key: key.to_string(),
        };
        match self.write_access(&mut st, line) {
            Ok(outcome) => {
                st.seen.insert(id);
                outcome
            }
            Err(e) => {
                self.fail(&mut st, &e);
                Recorded::Failed
            }
        }
    }

    /// Heartbeat: start a run if there is none, append queued records once
    /// the fence is released, and write a heartbeat when due or when an
    /// apply's fence waits for confirmation. Called every
    /// [`TICK_INTERVAL`] by [`AccessRecorder::spawn_heartbeat`].
    pub fn tick(&self) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if st.closed || !GenerationStore::is_managed(&self.repo) {
            return;
        }
        let now = clock::now();
        let result = (|| -> Result<()> {
            if st.run.is_none() {
                self.start_run(&mut st, now)?;
            }
            if !st.pending.is_empty() {
                self.with_shared(&mut st, |me, st| me.flush_pending(st))?;
            }
            let fence = read_fence(&access_dir(&self.repo));
            let run = st.run.as_ref().expect("run started above");
            let due = now - run.last_write
                >= Duration::from_std(HEARTBEAT_INTERVAL).unwrap_or(Duration::MAX)
                || run.link_day != now.date_naive()
                || (fence.is_some() && fence != run.fence);
            if due {
                self.control(&mut st, "h", now)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.fail(&mut st, &e);
        }
    }

    /// End the run cleanly. Records still queued behind a fence are
    /// appended first; if that is not possible now, the run is left open
    /// (shown as a gap once the depot restarts) rather than claimed
    /// complete.
    pub fn shutdown(&self) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if st.closed {
            return;
        }
        st.closed = true;
        if st.run.is_none() {
            return;
        }
        let result = (|| -> Result<()> {
            if !st.pending.is_empty() {
                self.with_shared(&mut st, |me, st| me.flush_pending(st))?;
            }
            if st.pending.is_empty() {
                self.control(&mut st, "e", clock::now())?;
            } else {
                warn!(
                    queued = st.pending.len(),
                    "depot stops while retirement is being applied; queued access records are lost and its access history ends here"
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.fail(&mut st, &e);
        }
    }

    /// Run [`AccessRecorder::tick`] every [`TICK_INTERVAL`] on a thread
    /// until the recorder is dropped. Start it after daemonising.
    pub fn spawn_heartbeat(self: &Arc<Self>) -> io::Result<std::thread::JoinHandle<()>> {
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("pkg6-access-heartbeat".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK_INTERVAL);
                    match weak.upgrade() {
                        Some(r) => r.tick(),
                        None => break,
                    }
                }
            })
    }

    fn fail(&self, st: &mut RecorderState, err: &RetentionError) {
        let lost = st.pending.len();
        // Records may be lost from the run's last confirmed control line on.
        let from = st.run.as_ref().map_or_else(clock::now, |r| r.last_write);
        st.failed_since = Some(st.failed_since.map_or(from, |f| f.min(from)));
        if let Some(run) = st.run.take() {
            st.chain = Some(ChainRef::After {
                run: run.id,
                seq: run.seq,
            });
        }
        st.pending.clear();
        let quiet = st
            .last_warning
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(600));
        if !quiet {
            warn!(
                error = %err,
                queued_lost = lost,
                "cannot record package access; retirement will not count access history from before recording works again. Make {} writable by the depot and free space",
                access_dir(&self.repo).display()
            );
            st.last_warning = Some(Instant::now());
        }
    }

    fn start_run(&self, st: &mut RecorderState, at: DateTime<Utc>) -> Result<()> {
        if let Some(r) = st.retry_at
            && at < r
        {
            return Err(RetentionError::Corrupt(format!(
                "access recording failed; retrying from {}",
                r.to_rfc3339()
            )));
        }
        let dir = access_dir(&self.repo);
        fs::create_dir_all(&dir).at(&dir)?;
        let chain = match &st.chain {
            Some(c) => c.clone(),
            None => match AccessLog::load(&self.repo) {
                Ok(log) => log
                    .recorders
                    .get(&self.instance)
                    .map_or(ChainRef::First, |p| ChainRef::After {
                        run: p.run.clone(),
                        seq: p.seq,
                    }),
                Err(e) => {
                    warn!(error = %e, "cannot read access records; access history restarts with this depot");
                    ChainRef::Unknown
                }
            },
        };
        let id = new_token();
        let fence = read_fence(&dir);
        let line = format!(
            "{}\ts\t{}\t{}\t0\t{}\t{}\t{}\n",
            at.timestamp(),
            self.instance,
            id,
            fence.as_deref().unwrap_or("-"),
            chain,
            st.failed_since
                .map_or_else(|| "-".to_string(), |t| t.timestamp().to_string())
        );
        append_line(&day_file(&dir, at.date_naive()), &line, true)?;
        st.chain = Some(chain);
        st.failed_since = None;
        st.retry_at = None;
        let head = Head {
            instance: self.instance.clone(),
            run: id.clone(),
            seq: 0,
            ended: false,
            at: at.timestamp(),
        };
        st.run = Some(Run {
            id,
            seq: 0,
            last_write: at,
            link_day: at.date_naive(),
            unsynced: None,
            fence,
        });
        // A failure ends the run just started (see `fail`).
        head.write(&dir).inspect_err(|_| {
            st.retry_at =
                Some(at + Duration::from_std(HEARTBEAT_INTERVAL).unwrap_or(Duration::MAX));
        })
    }

    fn control(&self, st: &mut RecorderState, tag: &str, at: DateTime<Utc>) -> Result<()> {
        let dir = access_dir(&self.repo);
        let fence = read_fence(&dir);
        let run = st
            .run
            .as_mut()
            .ok_or_else(|| RetentionError::Corrupt("access recorder has no run".into()))?;
        let seq = run.seq + 1;
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            at.timestamp(),
            tag,
            self.instance,
            run.id,
            seq,
            fence.as_deref().unwrap_or("-")
        );
        // This line vouches for the payload lines before it, but its sync
        // flushes only its own day log.
        if let Some(day) = run.unsynced
            && day != at.date_naive()
        {
            sync_log(&day_file(&dir, day))?;
        }
        append_line(&day_file(&dir, at.date_naive()), &line, true)?;
        run.unsynced = None;
        run.seq = seq;
        run.last_write = at;
        run.link_day = at.date_naive();
        run.fence = fence;
        Head {
            instance: self.instance.clone(),
            run: run.id.clone(),
            seq,
            ended: tag == "e",
            at: at.timestamp(),
        }
        .write(&dir)
    }

    /// Run `f` holding the shared fence lock; `Ok(None)` if an apply holds
    /// it exclusively.
    fn with_shared<T>(
        &self,
        st: &mut RecorderState,
        f: impl FnOnce(&Self, &mut RecorderState) -> Result<T>,
    ) -> Result<Option<T>> {
        let dir = access_dir(&self.repo);
        let file = match st.lock.take() {
            Some(f) => f,
            None => open_fence_lock(&dir)?,
        };
        let lock_path = dir.join(FENCE_LOCK_FILE);
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                st.lock = Some(file);
                return Ok(None);
            }
            Err(TryLockError::Error(e)) => return Err(e).at(&lock_path),
        }
        let out = f(self, st);
        let unlocked = file.unlock();
        st.lock = Some(file);
        let out = out?;
        unlocked.at(&lock_path)?;
        Ok(Some(out))
    }

    fn write_access(&self, st: &mut RecorderState, line: PendingLine) -> Result<Recorded> {
        if st.run.is_none() {
            self.start_run(st, line.at)?;
        }
        let mut line = Some(line);
        let written = self.with_shared(st, |me, st| {
            me.flush_pending(st)?;
            me.append_access(st, line.as_ref().expect("taken below only"))
        })?;
        if written.is_some() {
            return Ok(Recorded::Written);
        }
        if st.pending.len() >= MAX_PENDING {
            return Err(RetentionError::Corrupt(format!(
                "more than {} access records queued behind a retirement fence",
                MAX_PENDING
            )));
        }
        st.pending.push(line.take().expect("not written"));
        Ok(Recorded::Queued)
    }

    fn flush_pending(&self, st: &mut RecorderState) -> Result<()> {
        let pending = std::mem::take(&mut st.pending);
        for p in &pending {
            self.append_access(st, p)?;
        }
        Ok(())
    }

    fn append_access(&self, st: &mut RecorderState, p: &PendingLine) -> Result<()> {
        let day = p.at.date_naive();
        // Every day log a run writes to starts with a link of its chain, so
        // losing any of them shows as a gap.
        if st.run.as_ref().is_some_and(|r| r.link_day != day) {
            self.control(st, "h", p.at)?;
        }
        let line = format!(
            "{}\t{}\t{}\t{}\n",
            p.at.timestamp(),
            p.kind.tag(),
            p.publisher,
            p.key
        );
        // Manifest records drive retirement; make them survive power loss.
        // Payload records are flushed by the next control line.
        let dir = access_dir(&self.repo);
        let sync = p.kind == AccessKind::Manifest;
        let run = st
            .run
            .as_mut()
            .ok_or_else(|| RetentionError::Corrupt("access recorder has no run".into()))?;
        if !sync && let Some(prev) = run.unsynced.filter(|d| *d != day) {
            // Only one unsynced day log at a time.
            sync_log(&day_file(&dir, prev))?;
        }
        append_line(&day_file(&dir, day), &line, sync)?;
        if !sync {
            run.unsynced = Some(day);
        }
        Ok(())
    }
}

impl Drop for AccessRecorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Held by apply from its final access check through the generation
/// switch. While it is held, recorders queue access records instead of
/// appending them, so every access recorded before the switch was checked;
/// accesses queued meanwhile are appended (and count) after it.
pub struct AccessFence {
    _lock: File,
}

impl AccessFence {
    /// Take the fence and wait until every open run has confirmed it, which
    /// proves that none of them failed a write before. Fails after `timeout`
    /// naming the recorders that did not confirm.
    pub fn acquire(repo: &Path, timeout: std::time::Duration) -> Result<Self> {
        let dir = access_dir(repo);
        let lock = open_fence_lock(&dir)?;
        let lock_path = dir.join(FENCE_LOCK_FILE);
        lock.lock().at(&lock_path)?;
        let nonce = new_token();
        let fence = dir.join(FENCE_FILE);
        let tmp = dir.join(format!(".{}.tmp", FENCE_FILE));
        fs::write(&tmp, &nonce).at(&tmp)?;
        fs::rename(&tmp, &fence).at(&fence)?;
        let deadline = Instant::now() + timeout;
        loop {
            let log = AccessLog::load(repo)?;
            let waiting: Vec<OpenRun> = log
                .open_runs()
                .into_iter()
                .filter(|r| r.confirmed_fence.as_deref() != Some(nonce.as_str()))
                .collect();
            if waiting.is_empty() {
                return Ok(Self { _lock: lock });
            }
            if Instant::now() >= deadline {
                return Err(RetentionError::AccessUnconfirmed {
                    depots: waiting
                        .iter()
                        .map(|r| {
                            format!(
                                "{} (last record {})",
                                r.instance,
                                r.last_record.to_rfc3339()
                            )
                        })
                        .collect(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// Declare that a stale recorder is gone (its depot is not running). The
/// time since its last record becomes a gap, so access history is proven
/// again from now on. Refused while the recorder is still reporting.
pub fn close_recorder(repo: &Path, instance: &str, actor: &str) -> Result<DateTime<Utc>> {
    let now = clock::now();
    let log = AccessLog::load(repo)?;
    let Some(run) = log.open_runs().into_iter().find(|r| r.instance == instance) else {
        return Err(RetentionError::UnknownDepot {
            instance: instance.to_string(),
            known: log.open_runs().into_iter().map(|r| r.instance).collect(),
        });
    };
    if now - run.last_record < Duration::seconds(STALE_AFTER_SECS) {
        return Err(RetentionError::DepotStillReporting {
            instance: instance.to_string(),
            last: run.last_record.to_rfc3339(),
        });
    }
    let actor: String = actor
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    let dir = access_dir(repo);
    let line = format!("{}\tx\t{}\t{}\n", now.timestamp(), instance, actor);
    append_line(&day_file(&dir, now.date_naive()), &line, true)?;
    // The line may have created today's log.
    File::open(&dir).and_then(|d| d.sync_all()).at(&dir)?;
    Ok(now)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InstanceState {
    run: String,
    seq: u64,
    last: i64,
    open: bool,
    #[serde(default)]
    fence: Option<String>,
    /// When an operator closed the run (`x`).
    #[serde(default)]
    closed_at: Option<i64>,
}

/// The chain fields of an `s` line.
struct RunStart {
    after: ChainRef,
    failed_since: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GapRecord {
    recorder: String,
    from: i64,
    until: i64,
    reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Summary {
    #[serde(default)]
    manifests: BTreeMap<String, i64>,
    /// Keyed `<publisher>/<hash>`.
    #[serde(default)]
    payloads: BTreeMap<String, i64>,
    #[serde(default)]
    tracking_since: Option<i64>,
    #[serde(default)]
    gaps: Vec<GapRecord>,
    /// Run state of every recorder at the end of the folded logs.
    #[serde(default)]
    recorders: BTreeMap<String, InstanceState>,
    /// Newest day log folded (`YYYYMMDD`); older or equal logs are ignored.
    #[serde(default)]
    folded_through: Option<String>,
    #[serde(default)]
    malformed_lines: usize,
}

/// An interval in which accesses may have gone unrecorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub recorder: String,
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub reason: String,
}

/// A recorder run that has not ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRun {
    pub instance: String,
    pub last_record: DateTime<Utc>,
    pub confirmed_fence: Option<String>,
}

/// How much access history is proven complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// No access can have gone unrecorded since `since`.
    Proven { since: DateTime<Utc> },
    /// Nothing is proven now.
    Unproven { reason: String },
}

/// All access records of a repository.
#[derive(Debug, Default, Clone)]
pub struct AccessLog {
    /// Since when accesses are recorded (the first run start); `None` if no
    /// depot has recorded yet.
    pub tracking_since: Option<DateTime<Utc>>,
    /// Last access per canonical FMRI.
    pub manifests: HashMap<String, DateTime<Utc>>,
    /// Last access per (publisher, payload hash).
    pub payloads: HashMap<(String, String), DateTime<Utc>>,
    /// Lines that could not be parsed (each is a gap over its day).
    pub malformed_lines: usize,
    gaps: Vec<GapRecord>,
    recorders: BTreeMap<String, InstanceState>,
    folded_through: Option<NaiveDate>,
}

impl AccessLog {
    /// Read the summary and every day log not folded into it.
    pub fn load(repo: &Path) -> Result<Self> {
        let dir = access_dir(repo);
        let heads = read_heads(&dir)?;
        let mut log = Self::load_summary(&dir)?;
        let mut reached: HashSet<String> = HashSet::new();
        let mut check = |log: &Self, recorder: &str| {
            if let (Some(h), Some(s)) = (
                heads.iter().find(|h| h.instance == recorder),
                log.recorders.get(recorder),
            ) && h.run == s.run
                && h.seq == s.seq
            {
                reached.insert(recorder.to_string());
            }
        };
        let known: Vec<String> = log.recorders.keys().cloned().collect();
        for r in &known {
            check(&log, r);
        }
        for (day, path) in day_logs(&dir)? {
            if log.folded_through.is_none_or(|f| day > f) {
                log.read_day_log(&path, day, &mut check)?;
            }
        }
        log.reconcile_heads(&heads, &reached);
        Ok(log)
    }

    /// Compare the records read with every recorder's head (see the module
    /// documentation).
    fn reconcile_heads(&mut self, heads: &[Head], reached: &HashSet<String>) {
        for h in heads {
            if reached.contains(&h.instance) {
                continue;
            }
            let from = match self.recorders.get(&h.instance) {
                Some(s) => s.last,
                None => self.tracking_since.map_or(h.at, |t| t.timestamp()),
            };
            self.started(from.min(h.at));
            self.gap(
                &h.instance,
                from,
                h.at,
                "the newest records of this depot are missing (a deleted or truncated access log)",
            );
            self.recorders.insert(
                h.instance.clone(),
                InstanceState {
                    run: h.run.clone(),
                    seq: h.seq,
                    last: h.at,
                    open: !h.ended,
                    fence: None,
                    closed_at: None,
                },
            );
        }
        let headless: Vec<(String, i64)> = self
            .recorders
            .iter()
            .filter(|(r, _)| !heads.iter().any(|h| &h.instance == *r))
            .map(|(r, s)| (r.clone(), s.last))
            .collect();
        for (r, last) in headless {
            self.gap(
                &r,
                last,
                last,
                "this depot's access head is missing; its newest records may be missing too",
            );
        }
    }

    fn load_summary(dir: &Path) -> Result<Self> {
        let path = dir.join(SUMMARY_FILE);
        let s: Summary = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| RetentionError::Corrupt(format!("{}: {}", path.display(), e)))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).at(&path),
        };
        let folded_through = match &s.folded_through {
            Some(d) => Some(NaiveDate::parse_from_str(d, "%Y%m%d").map_err(|e| {
                RetentionError::Corrupt(format!("{}: folded_through: {}", path.display(), e))
            })?),
            None => None,
        };
        // Folding only ever reaches days before yesterday. A later value
        // would hide current day logs from `load` and let `compact` delete
        // them unread.
        let cutoff = (clock::now() - Duration::days(1)).date_naive();
        if let Some(f) = folded_through
            && f >= cutoff
        {
            return Err(RetentionError::Corrupt(format!(
                "{}: folded_through {} is not before {}; the access summary is from the future (clock moved back, or edited)",
                path.display(),
                f,
                cutoff
            )));
        }
        let mut log = AccessLog {
            tracking_since: s.tracking_since.and_then(ts),
            gaps: s.gaps,
            recorders: s.recorders,
            malformed_lines: s.malformed_lines,
            folded_through,
            ..Default::default()
        };
        // A dropped entry would be a lost access: refuse instead.
        let corrupt = |what: &str, k: &str| {
            RetentionError::Corrupt(format!(
                "{}: unreadable {} entry {:?}",
                path.display(),
                what,
                k
            ))
        };
        for (k, v) in s.manifests {
            let t = ts(v)
                .filter(|_| usable(&k))
                .ok_or_else(|| corrupt("manifest", &k))?;
            log.note(AccessKind::Manifest, None, k, t);
        }
        for (k, v) in s.payloads {
            let (Some((p, h)), Some(t)) = (k.split_once('/'), ts(v)) else {
                return Err(corrupt("payload", &k));
            };
            if !usable(p) || !usable(h) {
                return Err(corrupt("payload", &k));
            }
            log.note(AccessKind::Payload, Some(p.to_string()), h.to_string(), t);
        }
        Ok(log)
    }

    fn to_summary(&self) -> Summary {
        Summary {
            manifests: self
                .manifests
                .iter()
                .map(|(k, t)| (k.clone(), t.timestamp()))
                .collect(),
            payloads: self
                .payloads
                .iter()
                .map(|((p, h), t)| (format!("{}/{}", p, h), t.timestamp()))
                .collect(),
            tracking_since: self.tracking_since.map(|t| t.timestamp()),
            gaps: self.gaps.clone(),
            recorders: self.recorders.clone(),
            folded_through: self.folded_through.map(|d| d.format("%Y%m%d").to_string()),
            malformed_lines: self.malformed_lines,
        }
    }

    fn note(&mut self, kind: AccessKind, publisher: Option<String>, key: String, t: DateTime<Utc>) {
        match kind {
            AccessKind::Manifest => {
                let e = self.manifests.entry(key).or_insert(t);
                *e = (*e).max(t);
            }
            AccessKind::Payload => {
                let e = self
                    .payloads
                    .entry((publisher.unwrap_or_default(), key))
                    .or_insert(t);
                *e = (*e).max(t);
            }
        }
    }

    fn gap(&mut self, recorder: &str, from: i64, until: i64, reason: impl Into<String>) {
        self.gaps.push(GapRecord {
            recorder: recorder.to_string(),
            from: from.min(until),
            until,
            reason: reason.into(),
        });
    }

    fn started(&mut self, t: i64) {
        let t = at_secs(t);
        self.tracking_since = Some(self.tracking_since.map_or(t, |s| s.min(t)));
    }

    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        t: i64,
        tag: &str,
        recorder: &str,
        run: &str,
        seq: u64,
        fence: &str,
        start: Option<RunStart>,
    ) {
        let first_start = self.tracking_since.map(|s| s.timestamp());
        self.started(t);
        let fence = (fence != "-").then(|| fence.to_string());
        let prev = self.recorders.get(recorder).cloned();
        if let Some(start) = start {
            if let Some(f) = start.failed_since {
                self.gap(
                    recorder,
                    f,
                    t,
                    "the depot could not write its access records",
                );
            }
            let chained = matches!(
                (&prev, &start.after),
                (Some(p), ChainRef::After { run, seq }) if p.run == *run && p.seq == *seq
            );
            match &prev {
                Some(p) if chained => {
                    let unended = if p.open {
                        Some((
                            p.last,
                            "the depot stopped without a clean shutdown (crash, or a failed write)",
                        ))
                    } else {
                        p.closed_at.map(|c| {
                            (
                                c,
                                "the depot was closed by an operator but was still running",
                            )
                        })
                    };
                    // Unless the failure gap, which starts no later, covers it.
                    if let Some((from, reason)) = unended
                        && start.failed_since.is_none_or(|f| f > from)
                    {
                        self.gap(recorder, from, t, reason);
                    }
                }
                None if start.after == ChainRef::First => {}
                Some(p) => self.gap(
                    recorder,
                    p.last,
                    t,
                    "runs of this depot are missing (a deleted access log)",
                ),
                None => self.gap(
                    recorder,
                    first_start.unwrap_or(t),
                    t,
                    "earlier runs of this depot are missing (a deleted access log or summary)",
                ),
            }
        } else {
            let chained = prev
                .as_ref()
                .is_some_and(|p| p.open && p.run == run && seq == p.seq + 1);
            if !chained {
                let from = prev.as_ref().map_or(t, |p| p.last);
                self.gap(
                    recorder,
                    from,
                    t,
                    "records of this depot are missing (a deleted or truncated access log)",
                );
            }
        }
        self.recorders.insert(
            recorder.to_string(),
            InstanceState {
                run: run.to_string(),
                seq,
                last: t,
                open: tag != "e",
                fence,
                closed_at: None,
            },
        );
    }

    fn close(&mut self, t: i64, recorder: &str, actor: &str) {
        if let Some(p) = self.recorders.get(recorder).cloned()
            && p.open
        {
            self.gap(
                recorder,
                p.last,
                t,
                format!("stopped reporting; closed by {}", actor),
            );
            if let Some(s) = self.recorders.get_mut(recorder) {
                s.open = false;
                s.closed_at = Some(t);
            }
        }
    }

    /// `on_control` sees the log after every control line of a recorder.
    fn read_day_log(
        &mut self,
        path: &Path,
        day: NaiveDate,
        on_control: &mut dyn FnMut(&Self, &str),
    ) -> Result<()> {
        let text = match fs::read(path) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            // Folded and removed by a concurrent compaction.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).at(path),
        };
        let mut malformed = 0usize;
        for line in text.split_terminator('\n') {
            let fields: Vec<&str> = line.split('\t').collect();
            let secs = fields.first().and_then(|s| s.parse::<i64>().ok());
            let ok = match (secs, fields.as_slice()) {
                (Some(t), [_, tag @ ("m" | "f"), publisher, key])
                    if usable(publisher) && usable(key) && ts(t).is_some() =>
                {
                    let kind = if *tag == "m" {
                        AccessKind::Manifest
                    } else {
                        AccessKind::Payload
                    };
                    self.note(
                        kind,
                        Some(publisher.to_string()),
                        key.to_string(),
                        at_secs(t),
                    );
                    true
                }
                (Some(t), [_, "s", recorder, run, seq, fence, after, failed])
                    if usable(recorder) && usable(run) && ts(t).is_some() =>
                {
                    let failed_since = match *failed {
                        "-" => Some(None),
                        f => f.parse::<i64>().ok().filter(|f| ts(*f).is_some()).map(Some),
                    };
                    match (seq.parse::<u64>(), ChainRef::parse(after), failed_since) {
                        (Ok(seq), Some(after), Some(failed_since)) => {
                            let start = RunStart {
                                after,
                                failed_since,
                            };
                            self.control(t, "s", recorder, run, seq, fence, Some(start));
                            on_control(self, recorder);
                            true
                        }
                        _ => false,
                    }
                }
                (Some(t), [_, tag @ ("h" | "e"), recorder, run, seq, fence])
                    if usable(recorder) && usable(run) && ts(t).is_some() =>
                {
                    match seq.parse::<u64>() {
                        Ok(seq) => {
                            self.control(t, tag, recorder, run, seq, fence, None);
                            on_control(self, recorder);
                            true
                        }
                        Err(_) => false,
                    }
                }
                (Some(t), [_, "x", recorder, actor]) if usable(recorder) && ts(t).is_some() => {
                    self.close(t, recorder, actor);
                    true
                }
                _ => false,
            };
            if !ok {
                malformed += 1;
            }
        }
        if malformed > 0 {
            // An unreadable line may have been any access of that day.
            self.malformed_lines += malformed;
            let start = day
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc().timestamp())
                .unwrap_or_default();
            self.gap(
                "-",
                start,
                start + 86_400,
                format!("{} unreadable line(s) in {}", malformed, path.display()),
            );
        }
        Ok(())
    }

    /// Last qualifying access of a package version's manifest.
    pub fn manifest_access(&self, fmri: &str) -> Option<DateTime<Utc>> {
        self.manifests.get(fmri).copied()
    }

    /// Intervals in which accesses may be missing, oldest first.
    pub fn gaps(&self) -> Vec<Gap> {
        let mut out: Vec<Gap> = self
            .gaps
            .iter()
            .map(|g| Gap {
                recorder: g.recorder.clone(),
                from: at_secs(g.from),
                until: at_secs(g.until),
                reason: g.reason.clone(),
            })
            .collect();
        out.sort_by_key(|g| (g.until, g.from));
        out
    }

    /// Recorder runs that have not ended.
    pub fn open_runs(&self) -> Vec<OpenRun> {
        self.recorders
            .iter()
            .filter(|(_, s)| s.open)
            .map(|(name, s)| OpenRun {
                instance: name.clone(),
                last_record: at_secs(s.last),
                confirmed_fence: s.fence.clone(),
            })
            .collect()
    }

    /// Since when access history is proven complete at `now`: the later of
    /// the first run start and the end of the latest gap, provided no run
    /// is stale.
    pub fn coverage(&self, now: DateTime<Utc>) -> Coverage {
        let Some(start) = self.tracking_since else {
            return Coverage::Unproven {
                reason: "no depot has recorded access yet".into(),
            };
        };
        let stale: Vec<String> = self
            .open_runs()
            .into_iter()
            .filter(|r| now - r.last_record > Duration::seconds(STALE_AFTER_SECS))
            .map(|r| {
                format!(
                    "{} (last record {})",
                    r.instance,
                    r.last_record.to_rfc3339()
                )
            })
            .collect();
        if !stale.is_empty() {
            return Coverage::Unproven {
                reason: format!(
                    "depot(s) stopped confirming access recording: {}. A depot that cannot write its records (full disk, permissions) may be losing accesses; restart it once it can write, or if it is gone run `pkg6repo retire close-depot`",
                    stale.join(", ")
                ),
            };
        }
        let since = self
            .gaps
            .iter()
            .map(|g| at_secs(g.until))
            .fold(start, DateTime::max);
        Coverage::Proven { since }
    }
}

fn day_logs(dir: &Path) -> Result<Vec<(NaiveDate, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).at(dir),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.at(dir)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".log")
            && let Ok(day) = NaiveDate::parse_from_str(stem, "%Y%m%d")
        {
            out.push((day, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Fold day logs older than yesterday (relative to now) into the summary
/// and remove them. The caller holds the repository writer lock, so only
/// one compaction runs at a time; depots only append to the current day's
/// log. Returns the number of logs removed.
pub fn compact(repo: &Path) -> Result<usize> {
    let dir = access_dir(repo);
    let cutoff = (clock::now() - Duration::days(1)).date_naive();
    let old: Vec<(NaiveDate, PathBuf)> = day_logs(&dir)?
        .into_iter()
        .filter(|(d, _)| *d < cutoff)
        .collect();
    if old.is_empty() {
        return Ok(0);
    }
    let mut log = AccessLog::load_summary(&dir)?;
    let mut folded = false;
    for (day, path) in &old {
        if log.folded_through.is_none_or(|f| *day > f) {
            log.read_day_log(path, *day, &mut |_, _| {})?;
            log.folded_through = Some(*day);
            folded = true;
        }
    }
    if folded {
        let json = serde_json::to_vec(&log.to_summary())
            .map_err(|e| RetentionError::Corrupt(format!("serialising access summary: {}", e)))?;
        let tmp = dir.join(format!(".{}.tmp", SUMMARY_FILE));
        {
            let mut f = fs::File::create(&tmp).at(&tmp)?;
            f.write_all(&json).at(&tmp)?;
            f.sync_all().at(&tmp)?;
        }
        let summary_path = dir.join(SUMMARY_FILE);
        fs::rename(&tmp, &summary_path).at(&summary_path)?;
        fs::File::open(&dir).and_then(|d| d.sync_all()).at(&dir)?;
    }
    for (_, path) in &old {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).at(path),
        }
    }
    Ok(old.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::generation::{CURRENT_LINK, GENERATIONS_DIR};

    thread_local! {
        /// Logs and heads synced on this thread, in order.
        pub(super) static SYNCED: std::cell::RefCell<Vec<PathBuf>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn take_synced() -> Vec<PathBuf> {
        SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn managed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let gens = dir.path().join(GENERATIONS_DIR);
        fs::create_dir_all(&gens).unwrap();
        std::os::unix::fs::symlink("x", gens.join(CURRENT_LINK)).unwrap();
        dir
    }

    fn at(d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, d, h, 0, 0).unwrap()
    }

    fn record(rec: &AccessRecorder, kind: AccessKind, key: &str, t: DateTime<Utc>) -> Recorded {
        clock::pin(Some(t));
        rec.record(kind, "oi", key)
    }

    fn proven(repo: &Path, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match AccessLog::load(repo).unwrap().coverage(now) {
            Coverage::Proven { since } => Some(since),
            Coverage::Unproven { .. } => None,
        }
    }

    #[test]
    fn records_once_per_day_and_reads_back() {
        let repo = managed_repo();
        let rec = AccessRecorder::new(repo.path(), "d1");
        let f = "pkg://oi/a@1:20200101T000000Z";
        let m = AccessKind::Manifest;
        assert_eq!(record(&rec, m, f, at(1, 1)), Recorded::Written);
        assert_eq!(record(&rec, m, f, at(1, 5)), Recorded::Skipped);
        assert_eq!(record(&rec, m, f, at(2, 5)), Recorded::Written);
        assert_eq!(
            record(&rec, AccessKind::Payload, "abcd", at(2, 6)),
            Recorded::Written
        );
        // Rejected keys never reach the log.
        assert_eq!(record(&rec, m, "a\tb", at(2, 6)), Recorded::Skipped);
        drop(rec);

        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.tracking_since, Some(at(1, 1)));
        assert_eq!(log.manifest_access(f), Some(at(2, 5)));
        assert_eq!(
            log.payloads.get(&("oi".to_string(), "abcd".to_string())),
            Some(&at(2, 6))
        );
        // A clean run over two days: no gaps, nothing open.
        assert!(log.gaps().is_empty(), "{:?}", log.gaps());
        assert!(log.open_runs().is_empty());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));
    }

    #[test]
    fn a_new_day_syncs_payload_lines_of_the_previous_day_first() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        let before_midnight = at(2, 0) - Duration::seconds(5);
        record(&rec, AccessKind::Manifest, "pkg://oi/a@1", before_midnight);
        take_synced();
        // Payload lines alone are not synced: the request does not wait.
        assert_eq!(
            record(&rec, AccessKind::Payload, "abcd", before_midnight),
            Recorded::Written
        );
        assert!(take_synced().is_empty());

        // The first control line after midnight vouches for that line but
        // lands in the next day log, so the previous one is synced first.
        clock::pin(Some(at(2, 0)));
        rec.tick();
        let synced = take_synced();
        let day1 = day_file(&dir, before_midnight.date_naive());
        let day2 = day_file(&dir, at(2, 0).date_naive());
        assert_eq!(synced.first(), Some(&day1), "{synced:?}");
        assert!(synced.contains(&day2), "{synced:?}");

        // Once flushed, later control lines do not sync it again.
        clock::pin(Some(at(2, 1)));
        rec.tick();
        assert!(!take_synced().contains(&day1));

        // The same holds for a payload line that itself starts a new day
        // and for a shutdown just after midnight.
        record(
            &rec,
            AccessKind::Payload,
            "ef01",
            at(3, 0) - Duration::seconds(1),
        );
        take_synced();
        clock::pin(Some(at(3, 0)));
        drop(rec);
        let synced = take_synced();
        assert_eq!(
            synced.first(),
            Some(&day_file(&dir, at(2, 0).date_naive())),
            "{synced:?}"
        );
        let log = AccessLog::load(repo.path()).unwrap();
        assert!(log.gaps().is_empty(), "{:?}", log.gaps());
    }

    #[test]
    fn unmanaged_repositories_are_not_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let rec = AccessRecorder::new(dir.path(), "d1");
        assert_eq!(
            record(&rec, AccessKind::Manifest, "k", at(1, 1)),
            Recorded::Skipped
        );
        rec.tick();
        drop(rec);
        assert!(!access_dir(dir.path()).exists());
    }

    #[test]
    fn a_crash_is_a_gap_until_the_restart() {
        let repo = managed_repo();
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        // A crash runs no destructor.
        std::mem::forget(rec);
        clock::pin(Some(at(1, 3)));
        // While the crashed run is recent, nothing says it failed.
        assert_eq!(proven(repo.path(), at(1, 1)), Some(at(1, 1)));
        // Once it is stale, nothing is proven.
        assert_eq!(proven(repo.path(), at(1, 3)), None);
        let rec = AccessRecorder::new(repo.path(), "d1");
        rec.tick();
        drop(rec);
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.gaps().len(), 1, "{:?}", log.gaps());
        assert_eq!(log.gaps()[0].until, at(1, 3));
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 3)));
    }

    #[test]
    fn a_failed_write_ends_the_run_and_shows_as_a_gap() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        // The next day's log cannot be created (a dangling link stands in
        // for a full disk, even for root).
        std::os::unix::fs::symlink("/nonexistent/pkg6/x", day_file(&dir, at(2, 0).date_naive()))
            .unwrap();
        assert_eq!(
            record(&rec, AccessKind::Manifest, "k2", at(2, 1)),
            Recorded::Failed
        );
        // The failing depot cannot write anything, not even a heartbeat.
        clock::pin(Some(at(2, 2)));
        rec.tick();
        assert_eq!(proven(repo.path(), at(2, 2)), None);
        // Writing works again: a new run starts, the failure is a gap.
        fs::remove_file(day_file(&dir, at(2, 0).date_naive())).unwrap();
        clock::pin(Some(at(2, 3)));
        rec.tick();
        drop(rec);
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.manifest_access("k2"), None);
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(2, 3)));
    }

    #[test]
    fn deleted_history_is_a_gap() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        for d in 1..=3 {
            record(&rec, AccessKind::Manifest, &format!("k{d}"), at(d, 1));
        }
        drop(rec);
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));
        fs::remove_file(day_file(&dir, at(2, 0).date_naive())).unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.gaps().len(), 1, "{:?}", log.gaps());
        assert!(log.gaps()[0].reason.contains("missing"));
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(3, 1)));

        // Losing everything leaves nothing proven.
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(proven(repo.path(), at(9, 0)), None);
    }

    #[test]
    fn deleting_whole_clean_runs_is_a_gap() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        // A depot restarted every day: each day log holds one clean run.
        for d in 1..=3 {
            let rec = AccessRecorder::new(repo.path(), "d1");
            record(&rec, AccessKind::Manifest, &format!("k{d}"), at(d, 1));
            drop(rec);
        }
        let log = AccessLog::load(repo.path()).unwrap();
        assert!(log.gaps().is_empty(), "{:?}", log.gaps());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));

        fs::remove_file(day_file(&dir, at(2, 0).date_naive())).unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.manifest_access("k2"), None);
        assert_eq!(log.gaps().len(), 1, "{:?}", log.gaps());
        assert_eq!(log.gaps()[0].from, at(1, 1));
        assert_eq!(log.gaps()[0].until, at(3, 1));
        assert!(log.gaps()[0].reason.contains("missing"));
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(3, 1)));
    }

    #[test]
    fn deleting_folded_runs_is_a_gap() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        for d in 1..=3 {
            let rec = AccessRecorder::new(repo.path(), "d1");
            record(&rec, AccessKind::Manifest, &format!("k{d}"), at(d, 1));
            drop(rec);
        }
        // Day 1 is folded into the summary; the chain continues across it.
        clock::pin(Some(at(3, 12)));
        assert_eq!(compact(repo.path()).unwrap(), 1);
        assert!(AccessLog::load(repo.path()).unwrap().gaps().is_empty());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));
        fs::remove_file(day_file(&dir, at(2, 0).date_naive())).unwrap();
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(3, 1)));
    }

    #[test]
    fn deleting_the_newest_clean_runs_is_a_gap_before_and_after_a_restart() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        // Run A on days 1-2, run B on days 3-4, both ended cleanly.
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        record(&rec, AccessKind::Manifest, "k2", at(2, 1));
        drop(rec);
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k3", at(3, 1));
        record(&rec, AccessKind::Manifest, "k4", at(4, 1));
        drop(rec);
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));

        // With every depot stopped, run B's logs disappear.
        for d in [3, 4] {
            fs::remove_file(day_file(&dir, at(d, 0).date_naive())).unwrap();
        }
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.manifest_access("k3"), None);
        assert!(
            log.gaps()
                .iter()
                .any(|g| g.from == at(2, 1) && g.until == at(4, 1)),
            "{:?}",
            log.gaps()
        );
        assert!(log.open_runs().is_empty());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(4, 1)));
        // No depot needs to confirm a fence, yet the gate sees the gap.
        drop(AccessFence::acquire(repo.path(), std::time::Duration::from_millis(100)).unwrap());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(4, 1)));

        // A restart chains to the head, not to the shortened records, so
        // the gap stays once the head moves on.
        clock::pin(Some(at(5, 1)));
        let rec = AccessRecorder::new(repo.path(), "d1");
        rec.tick();
        drop(rec);
        let log = AccessLog::load(repo.path()).unwrap();
        assert!(
            log.gaps()
                .iter()
                .any(|g| g.from == at(2, 1) && g.until == at(5, 1)),
            "{:?}",
            log.gaps()
        );
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(5, 1)));
    }

    #[test]
    fn deleting_the_newest_log_of_an_open_run_is_stale() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        record(&rec, AccessKind::Manifest, "k2", at(2, 1));
        // A crash, then the newest log is lost.
        std::mem::forget(rec);
        fs::remove_file(day_file(&dir, at(2, 0).date_naive())).unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        // The head keeps the run open, so it is stale rather than ended.
        assert_eq!(log.open_runs().len(), 1);
        assert_eq!(log.open_runs()[0].last_record, at(2, 1));
        assert_eq!(proven(repo.path(), at(3, 0)), None);
        clock::pin(Some(at(3, 0)));
        let rec = AccessRecorder::new(repo.path(), "d1");
        rec.tick();
        drop(rec);
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(3, 0)));
    }

    #[test]
    fn a_missing_or_unreadable_head_fails_closed() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        for d in 1..=2 {
            let rec = AccessRecorder::new(repo.path(), "d1");
            record(&rec, AccessKind::Manifest, &format!("k{d}"), at(d, 1));
            drop(rec);
        }
        let head = head_file(&dir, "d1");
        let bytes = fs::read(&head).unwrap();
        fs::remove_file(&head).unwrap();
        // Nothing before the last record read is proven.
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(2, 1)));
        fs::write(&head, b"d1\tR\tnot-a-number\te\t1\n").unwrap();
        assert!(matches!(
            AccessLog::load(repo.path()),
            Err(RetentionError::Corrupt(_))
        ));
        // A head naming another recorder than its file is refused too.
        fs::write(
            &head,
            String::from_utf8(bytes.clone())
                .unwrap()
                .replacen("d1", "d2", 1),
        )
        .unwrap();
        assert!(matches!(
            AccessLog::load(repo.path()),
            Err(RetentionError::Corrupt(_))
        ));
        fs::write(&head, &bytes).unwrap();
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 1)));
    }

    #[test]
    fn a_failed_head_update_ends_the_run() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        // The head cannot be replaced any more.
        let tmp = dir.join(format!(
            ".{}.tmp",
            head_file(&dir, "d1").file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink("/nonexistent/pkg6/x", &tmp).unwrap();
        assert_eq!(
            record(&rec, AccessKind::Manifest, "k2", at(2, 1)),
            Recorded::Failed
        );
        clock::pin(Some(at(2, 2)));
        rec.tick();
        // Retries are spaced out: each one may append a run start.
        let starts = |d: u32| {
            fs::read_to_string(day_file(&dir, at(d, 0).date_naive()))
                .unwrap()
                .lines()
                .filter(|l| l.split('\t').nth(1) == Some("s"))
                .count()
        };
        let before = starts(2);
        clock::pin(Some(at(2, 2) + Duration::seconds(30)));
        rec.tick();
        assert_eq!(starts(2), before);
        // The failed run stops confirming and goes stale.
        assert_eq!(proven(repo.path(), at(2, 3)), None);
        // Once the head can be written again, the failure is a gap.
        fs::remove_file(&tmp).unwrap();
        clock::pin(Some(at(2, 4)));
        rec.tick();
        drop(rec);
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.manifest_access("k2"), None);
        assert!(log.open_runs().is_empty());
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(2, 4)));
    }

    #[test]
    fn a_summary_folded_through_today_is_refused() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        fs::create_dir_all(&dir).unwrap();
        clock::pin(Some(at(5, 12)));
        for (day, ok) in [
            ("20260303", true),
            ("20260304", false),
            ("20260305", false),
            ("20260310", false),
        ] {
            fs::write(
                dir.join(SUMMARY_FILE),
                format!(r#"{{"folded_through":"{day}"}}"#),
            )
            .unwrap();
            let loaded = AccessLog::load(repo.path());
            assert_eq!(loaded.is_ok(), ok, "{day}");
            if !ok {
                assert!(matches!(loaded, Err(RetentionError::Corrupt(_))), "{day}");
            }
        }
    }

    #[test]
    fn failures_before_a_close_stay_a_gap_until_the_next_run() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        // Day 2: the depot keeps running but cannot write (full disk).
        let day2 = day_file(&dir, at(2, 0).date_naive());
        std::os::unix::fs::symlink("/nonexistent/pkg6/x", &day2).unwrap();
        assert_eq!(
            record(&rec, AccessKind::Manifest, "k2", at(2, 1)),
            Recorded::Failed
        );
        clock::pin(Some(at(2, 2)));
        rec.tick();
        // Day 3: an operator takes the stale depot for gone.
        clock::pin(Some(at(3, 0)));
        close_recorder(repo.path(), "d1", "op").unwrap();
        assert_eq!(proven(repo.path(), at(3, 0)), Some(at(3, 0)));
        // The same process writes again later: it was alive all along, so
        // nothing up to its new run is proven.
        clock::pin(Some(at(3, 5)));
        rec.tick();
        drop(rec);
        fs::remove_file(&day2).unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.manifest_access("k2"), None);
        assert!(
            log.gaps()
                .iter()
                .any(|g| g.from == at(1, 1) && g.until == at(3, 5)),
            "{:?}",
            log.gaps()
        );
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(3, 5)));
    }

    #[test]
    fn a_closed_depot_that_comes_back_is_a_gap_from_the_close() {
        // A failing depot closed by an operator, then restarted: the new
        // process knows nothing of the failure.
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        fs::create_dir_all(&dir).unwrap();
        let t = |h: u32| at(1, h).timestamp();
        fs::write(
            day_file(&dir, at(1, 0).date_naive()),
            format!(
                "{}\ts\td1\tR0\t0\t-\t-\t-\n{}\tm\toi\tk1\n{}\tx\td1\top\n{}\ts\td1\tR1\t0\t-\tR0:0\t-\n{}\te\td1\tR1\t1\t-\n",
                t(1),
                t(1),
                t(3),
                t(5),
                t(6)
            ),
        )
        .unwrap();
        Head {
            instance: "d1".into(),
            run: "R1".into(),
            seq: 1,
            ended: true,
            at: t(6),
        }
        .write(&dir)
        .unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        assert_eq!(log.malformed_lines, 0);
        assert!(log.open_runs().is_empty());
        assert!(
            log.gaps()
                .iter()
                .any(|g| g.from == at(1, 3) && g.until == at(1, 5)),
            "{:?}",
            log.gaps()
        );
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(1, 5)));
    }

    #[test]
    fn unreadable_summary_entries_are_refused() {
        let repo = managed_repo();
        let dir = access_dir(repo.path());
        fs::create_dir_all(&dir).unwrap();
        for bad in [
            r#"{"manifests":{"pkg://oi/a@1":9223372036854775807}}"#,
            r#"{"payloads":{"no-publisher":1}}"#,
            r#"{"payloads":{"/abcd":1}}"#,
        ] {
            fs::write(dir.join(SUMMARY_FILE), bad).unwrap();
            assert!(
                matches!(
                    AccessLog::load(repo.path()),
                    Err(RetentionError::Corrupt(_))
                ),
                "{bad}"
            );
        }
        fs::write(
            dir.join(SUMMARY_FILE),
            r#"{"manifests":{"pkg://oi/a@1":1},"payloads":{"oi/abcd":1}}"#,
        )
        .unwrap();
        assert!(AccessLog::load(repo.path()).is_ok());
    }

    #[test]
    fn a_stale_recorder_can_be_closed_by_the_operator() {
        let repo = managed_repo();
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        std::mem::forget(rec);
        clock::pin(Some(at(1, 1)));
        assert!(matches!(
            close_recorder(repo.path(), "d1", "op"),
            Err(RetentionError::DepotStillReporting { .. })
        ));
        assert!(matches!(
            close_recorder(repo.path(), "d2", "op"),
            Err(RetentionError::UnknownDepot { .. })
        ));
        clock::pin(Some(at(2, 0)));
        close_recorder(repo.path(), "d1", "op").unwrap();
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(2, 0)));
    }

    #[test]
    fn fence_queues_records_and_waits_for_confirmation() {
        let repo = managed_repo();
        let rec = Arc::new(AccessRecorder::new(repo.path(), "d1"));
        clock::pin(None);
        assert_eq!(
            rec.record(AccessKind::Manifest, "oi", "k1"),
            Recorded::Written
        );
        // An open run that never confirms blocks the fence.
        assert!(matches!(
            AccessFence::acquire(repo.path(), std::time::Duration::from_millis(300)),
            Err(RetentionError::AccessUnconfirmed { .. })
        ));
        let ticker = {
            let rec = rec.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    rec.tick();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        };
        let fence = AccessFence::acquire(repo.path(), std::time::Duration::from_secs(10)).unwrap();
        // Recording neither blocks nor gets lost while the fence is held.
        assert_eq!(
            rec.record(AccessKind::Manifest, "oi", "k2"),
            Recorded::Queued
        );
        assert_eq!(
            AccessLog::load(repo.path()).unwrap().manifest_access("k2"),
            None
        );
        drop(fence);
        ticker.join().unwrap();
        let log = AccessLog::load(repo.path()).unwrap();
        assert!(log.manifest_access("k2").is_some());
        assert!(log.gaps().is_empty(), "{:?}", log.gaps());
    }

    #[test]
    fn torn_lines_are_gaps_and_compaction_is_idempotent() {
        let repo = managed_repo();
        let rec = AccessRecorder::new(repo.path(), "d1");
        record(&rec, AccessKind::Manifest, "k1", at(1, 1));
        record(&rec, AccessKind::Manifest, "k2", at(5, 1));
        drop(rec);
        let dir = access_dir(repo.path());
        let old_log = day_file(&dir, at(1, 1).date_naive());
        let mut f = OpenOptions::new().append(true).open(&old_log).unwrap();
        f.write_all(b"123\tm\too").unwrap();
        drop(f);
        let before = AccessLog::load(repo.path()).unwrap();
        assert_eq!(before.malformed_lines, 1);
        assert_eq!(proven(repo.path(), at(9, 0)), Some(at(2, 0)));

        let old_bytes = fs::read(&old_log).unwrap();
        clock::pin(Some(at(5, 12)));
        assert_eq!(compact(repo.path()).unwrap(), 1);
        // A crash after the summary was replaced but before the logs were
        // removed leaves logs that are not read or folded again.
        fs::write(&old_log, &old_bytes).unwrap();
        let after = AccessLog::load(repo.path()).unwrap();
        assert_eq!(after.manifests, before.manifests);
        assert_eq!(after.gaps(), before.gaps());
        assert_eq!(after.malformed_lines, 1);
        assert_eq!(compact(repo.path()).unwrap(), 1);
        assert_eq!(compact(repo.path()).unwrap(), 0);
        let after = AccessLog::load(repo.path()).unwrap();
        assert_eq!(after.manifests, before.manifests);
        assert_eq!(after.gaps(), before.gaps());
        assert_eq!(after.tracking_since, before.tracking_since);
        assert!(!old_log.exists());
        assert!(day_file(&dir, at(5, 1).date_naive()).exists());
    }
}
