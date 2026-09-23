//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Plan, apply and restore retirement; report its status.
//!
//! A plan is computed without the writer lock against the published
//! generation and saved by the operator for review. Apply takes the writer
//! lock and checks the plan again against whatever generation is published
//! then, with fresh access records: every planned version must still exist,
//! be superseded, not be kept and (for policy candidates) still be inactive,
//! and hiding the set must keep protected snapshots and the dependencies of
//! visible packages installable. Only then is a generation staged with the
//! new hidden set, rebuilt catalogs for the affected publishers and an audit
//! record. Right before the atomic switch, apply takes the access fence,
//! waits for every running depot to confirm its recording, reads the access
//! records once more and checks every policy candidate again; the switch
//! happens while the fence is held, so no access recorded before it goes
//! unseen. Readers see the old catalogs until that switch; hidden manifests
//! and payloads are never moved, so exact fetches work throughout.
//!
//! Time is the wall clock ([`super::clock`]) throughout: eligibility, the
//! `hidden_at` of hidden versions and audit records.

use super::access::{self, AccessFence, AccessLog, Coverage};
use super::clock;
use super::policy::{FmriRule, Interval, Policy};
use super::state::{
    AuditKind, AuditRecord, HiddenEntry, HideReason, RetirementState, current_actor,
};
use super::universe::{
    Blockers, DepKind, PayloadRefs, SnapshotClosure, Universe, check_hiding, snapshot_closure,
};
use super::{IoContext, Result, RetentionError};
use crate::fmri::Fmri;
use crate::repository::FileBackend;
use crate::repository::generation::{
    self, FailPoint, GENERATIONS_DIR, GenerationInfo, GenerationReason, GenerationStore,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Version of the plan file format.
pub const PLAN_FORMAT: u32 = 1;

/// A version a plan hides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedHide {
    pub fmri: String,
    pub publisher: String,
    pub reason: HideReason,
    pub superseded_at: Option<String>,
    pub last_access: Option<String>,
    /// For policy candidates: when the inactivity interval ran out.
    pub eligible_since: Option<String>,
}

/// A selected or eligible version the plan leaves visible, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSkip {
    pub fmri: String,
    pub reason: String,
}

/// A reviewed retirement plan (`pkg6repo retire plan -o <file>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementPlan {
    pub format: u32,
    /// SHA-256 of the plan content; identifies the plan in audit records.
    pub id: String,
    pub created: String,
    pub repository: PathBuf,
    /// Generation the plan was computed against.
    pub base_generation: String,
    pub policy_digest: String,
    pub policy_driven: bool,
    /// Explicit operator selections (exact FMRIs or `regex:` rules).
    pub selections: Vec<String>,
    pub hide: Vec<PlannedHide>,
    pub skipped: Vec<PlanSkip>,
    pub warnings: Vec<String>,
}

impl RetirementPlan {
    fn compute_id(&self) -> String {
        let mut copy = self.clone();
        copy.id = String::new();
        let bytes = serde_json::to_vec(&copy).unwrap_or_default();
        format!("{:x}", sha2::Sha256::digest(&bytes))
    }

    /// Whether the content still matches its id (it was not edited).
    pub fn verify_id(&self) -> bool {
        self.id == self.compute_id()
    }
}

/// What to plan.
#[derive(Debug, Clone, Default)]
pub struct PlanRequest {
    /// Include versions the policy makes eligible (needs `enabled true`).
    pub use_policy: bool,
    /// Explicitly selected versions; each must match a package version.
    /// Selected versions must be superseded; the inactivity interval does
    /// not apply to them.
    pub select: Vec<FmriRule>,
}

/// Points of an apply at which tests can act (`ApplyOptions::hook`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyStage {
    /// The generation is built; the final access check has not started.
    BeforeFinalCheck,
    /// The access fence is held and confirmed; the records are read next.
    Fenced,
}

/// A test callback run at an [`ApplyStage`].
#[cfg(any(test, feature = "test-clock"))]
#[derive(Clone)]
pub struct ApplyHook(pub std::sync::Arc<dyn Fn(ApplyStage) + Send + Sync>);

#[cfg(any(test, feature = "test-clock"))]
impl std::fmt::Debug for ApplyHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApplyHook")
    }
}

#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub wait_for_lock: bool,
    /// How long the final check waits for running depots to confirm their
    /// access recording (policy candidates only).
    pub fence_timeout: std::time::Duration,
    /// Simulate a crash (failure-injection tests).
    pub fail_point: Option<FailPoint>,
    #[cfg(any(test, feature = "test-clock"))]
    pub hook: Option<ApplyHook>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            wait_for_lock: true,
            fence_timeout: std::time::Duration::from_secs(30),
            fail_point: None,
            #[cfg(any(test, feature = "test-clock"))]
            hook: None,
        }
    }
}

impl ApplyOptions {
    #[cfg_attr(not(any(test, feature = "test-clock")), allow(unused_variables))]
    fn at(&self, stage: ApplyStage) {
        #[cfg(any(test, feature = "test-clock"))]
        if let Some(h) = &self.hook {
            (h.0)(stage);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// A new generation hides the versions.
    Applied {
        generation: String,
        hidden: Vec<String>,
        /// The plan was checked against a newer generation than its own.
        rechecked: bool,
    },
    /// The audit trail already records this plan.
    AlreadyApplied { operation: String },
    /// Every planned version is already hidden.
    NothingToDo,
}

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub wait_for_lock: bool,
    pub dry_run: bool,
    pub fail_point: Option<FailPoint>,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            wait_for_lock: true,
            dry_run: false,
            fail_point: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored {
        generation: String,
        restored: Vec<String>,
    },
    DryRun {
        would_restore: Vec<String>,
    },
    /// The selections match only versions that are already visible.
    AlreadyVisible {
        fmris: Vec<String>,
    },
}

/// A hidden version in [`RetirementStatus`].
#[derive(Debug, Clone, Serialize)]
pub struct HiddenStatus {
    pub fmri: String,
    #[serde(flatten)]
    pub entry: HiddenEntry,
    /// Latest qualifying access, including accesses after hiding.
    pub last_access: Option<String>,
}

/// An interval of [`RetirementStatus`] in which accesses may be missing.
#[derive(Debug, Clone, Serialize)]
pub struct GapStatus {
    pub recorder: String,
    pub from: String,
    pub until: String,
    pub reason: String,
}

/// A depot whose access recording run is open.
#[derive(Debug, Clone, Serialize)]
pub struct RecorderStatus {
    pub name: String,
    pub last_record: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetirementStatus {
    pub generation: String,
    pub enabled: bool,
    pub inactivity: String,
    pub purge_grace: String,
    pub tracking_since: Option<String>,
    /// Access history is complete since then (policy candidates count
    /// inactivity from no earlier).
    pub access_proven_since: Option<String>,
    /// Why no access history is proven, if so.
    pub access_unproven: Option<String>,
    pub access_gaps: Vec<GapStatus>,
    pub recorders: Vec<RecorderStatus>,
    pub hidden: Vec<HiddenStatus>,
    pub audit: Vec<AuditRecord>,
    /// Hidden snapshot versions, unavailable snapshot members, unsatisfied
    /// snapshot requirements and content whose dependencies cannot be
    /// checked.
    pub problems: Vec<String>,
    pub malformed_access_lines: usize,
}

fn rfc(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

/// Everything the checks read, from one generation.
struct Context {
    u: Universe,
    refs: PayloadRefs,
    state: RetirementState,
    access: AccessLog,
    /// Latest access of each version, from its manifest or from payloads
    /// only it references.
    last_access: HashMap<usize, DateTime<Utc>>,
    /// Latest restore of each version; its inactivity counts from there.
    restored_at: HashMap<usize, DateTime<Utc>>,
    hidden: HashSet<usize>,
    closures: Vec<SnapshotClosure>,
    /// Problems of the repository and policy, also reported by status.
    problems: Vec<String>,
    warnings: Vec<String>,
}

impl Context {
    fn set_access(&mut self, access: AccessLog) {
        let mut last_access: HashMap<usize, DateTime<Utc>> = HashMap::new();
        for (i, p) in self.u.packages.iter().enumerate() {
            if let Some(t) = access.manifest_access(&p.fmri) {
                last_access.insert(i, t);
            }
        }
        // A payload shared by several versions cannot tell which one was
        // used, so it refreshes none of them.
        for (key, users) in &self.refs {
            if let ([only], Some(&t)) = (users.as_slice(), access.payloads.get(key)) {
                let e = last_access.entry(*only).or_insert(t);
                *e = (*e).max(t);
            }
        }
        self.last_access = last_access;
        self.access = access;
    }
}

/// Payloads of the versions a plan hides, so that the final check of an
/// apply sees payload accesses that arrive while it runs.
fn planned_payloads(gen_dir: &Path, plan: &RetirementPlan) -> Result<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    for h in &plan.hide {
        let Ok(fmri) = Fmri::parse(&h.fmri) else {
            continue;
        };
        let path = FileBackend::construct_manifest_path(
            gen_dir,
            &h.publisher,
            fmri.stem(),
            &fmri.version(),
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // Reported as "no longer in the repository" by the recheck.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).at(&path),
        };
        for p in super::universe::scan_manifest(&String::from_utf8_lossy(&bytes)).payloads {
            out.insert((h.publisher.clone(), p));
        }
    }
    Ok(out)
}

fn load_context(
    repo: &Path,
    gen_dir: &Path,
    info: &GenerationInfo,
    policy: &Policy,
    watch_payloads: &HashSet<(String, String)>,
) -> Result<Context> {
    let mut first_seen: HashMap<String, DateTime<Utc>> = HashMap::new();
    for r in generation::read_receipts(gen_dir)? {
        // Skipping it would date its versions by their FMRI timestamps,
        // which can make them look superseded earlier than they were.
        let t = DateTime::parse_from_rfc3339(&r.committed)
            .map_err(|e| {
                RetentionError::Corrupt(format!(
                    "import receipt of generation {}: committed: {}",
                    r.generation, e
                ))
            })?
            .with_timezone(&Utc);
        for f in r.added {
            let e = first_seen.entry(f).or_insert(t);
            *e = (*e).min(t);
        }
    }
    let access = AccessLog::load(repo)?;
    let accessed: HashSet<(String, String)> = access.payloads.keys().cloned().collect();
    let wanted = |p: &str, h: &str| {
        let key = (p.to_string(), h.to_string());
        accessed.contains(&key) || watch_payloads.contains(&key)
    };
    let (u, refs): (Universe, PayloadRefs) =
        Universe::load(gen_dir, &info.publishers, &first_seen, &wanted)?;

    let mut restored_at: HashMap<usize, DateTime<Utc>> = HashMap::new();
    for r in generation::read_audit_records(gen_dir)? {
        if r.kind != AuditKind::Restore {
            continue;
        }
        let t = DateTime::parse_from_rfc3339(&r.at)
            .map_err(|e| {
                RetentionError::Corrupt(format!("audit record {}: at: {}", r.operation, e))
            })?
            .with_timezone(&Utc);
        for f in &r.fmris {
            if let Some(i) = u.get(f) {
                let e = restored_at.entry(i).or_insert(t);
                *e = (*e).max(t);
            }
        }
    }

    let state = generation::read_retirement_state(gen_dir)?;
    let mut problems = Vec::new();
    let mut hidden = HashSet::new();
    for fmri in state.hidden.keys() {
        match u.get(fmri) {
            Some(i) => {
                hidden.insert(i);
            }
            None => problems.push(format!(
                "{}: recorded as hidden but has no manifest in generation {}",
                fmri, info.id
            )),
        }
    }
    for uv in &u.unverifiable {
        problems.push(format!(
            "{}; retiring or excluding versions it may need is refused",
            uv.what
        ));
    }
    let closures: Vec<SnapshotClosure> = policy
        .snapshots
        .iter()
        .map(|s| snapshot_closure(&u, s))
        .collect();
    for c in &closures {
        if !c.missing.is_empty() {
            problems.push(format!(
                "snapshot {}: {} member(s) are not in the repository (unavailable historical content): {}",
                c.name,
                c.missing.len(),
                c.missing.join(", ")
            ));
        }
        for m in &c.unsatisfied {
            problems.push(format!("snapshot {}: not installable: {}", c.name, m));
        }
    }
    let mut warnings = problems.clone();
    if access.malformed_lines > 0 {
        warnings.push(format!(
            "{} unreadable access record line(s); access history is proven only after the days that hold them",
            access.malformed_lines
        ));
    }
    let mut ctx = Context {
        u,
        refs,
        state,
        access: AccessLog::default(),
        last_access: HashMap::new(),
        restored_at,
        hidden,
        closures,
        problems,
        warnings,
    };
    ctx.set_access(access);
    Ok(ctx)
}

enum Eligibility {
    Eligible { since: DateTime<Utc> },
    NotYet { until: DateTime<Utc> },
    Unknown(String),
}

fn eligibility(ctx: &Context, i: usize, interval: Interval, now: DateTime<Utc>) -> Eligibility {
    let proven = match ctx.access.coverage(now) {
        Coverage::Proven { since } => since,
        Coverage::Unproven { reason } => {
            return Eligibility::Unknown(format!(
                "access history is not proven complete: {}",
                reason
            ));
        }
    };
    let Some(sup) = ctx.u.superseded_at(i) else {
        return Eligibility::Unknown(if ctx.u.is_superseded(i) {
            "the publication time of its successors is unknown".into()
        } else {
            "it is the newest version".into()
        });
    };
    let mut base = sup.max(proven);
    if let Some(&t) = ctx.last_access.get(&i) {
        base = base.max(t);
    }
    if let Some(&t) = ctx.restored_at.get(&i) {
        base = base.max(t);
    }
    let since = interval.after(base);
    if now >= since {
        Eligibility::Eligible { since }
    } else {
        Eligibility::NotYet { until: since }
    }
}

fn check_managed(repo: &Path) -> Result<GenerationInfo> {
    generation::published_generation(repo)?.ok_or_else(|| RetentionError::Unmanaged {
        path: repo.to_path_buf(),
    })
}

fn planned(
    ctx: &Context,
    i: usize,
    reason: HideReason,
    since: Option<DateTime<Utc>>,
) -> PlannedHide {
    let p = &ctx.u.packages[i];
    PlannedHide {
        fmri: p.fmri.clone(),
        publisher: p.publisher.clone(),
        reason,
        superseded_at: ctx.u.superseded_at(i).map(rfc),
        last_access: ctx.last_access.get(&i).copied().map(rfc),
        eligible_since: since.map(rfc),
    }
}

fn blocked(b: Blockers) -> RetentionError {
    RetentionError::Blocked {
        blockers: b.blockers,
        suggested_keeps: b.keeps.into_iter().collect(),
        suggested_restores: b.restores.into_iter().collect(),
    }
}

/// Compute a retirement plan against the published generation. Fails with
/// [`RetentionError::Blocked`] (listing exact keep entries or restores that
/// resolve it) if hiding the candidates would make a protected snapshot or
/// a hard dependency of a visible package unsatisfiable, or if a snapshot
/// version is already hidden.
pub fn plan_retirement(
    repo: &Path,
    policy: &Policy,
    request: &PlanRequest,
) -> Result<RetirementPlan> {
    let now = clock::now();
    let info = check_managed(repo)?;
    let gen_dir = repo.join(GENERATIONS_DIR).join(&info.id);
    let ctx = load_context(repo, &gen_dir, &info, policy, &HashSet::new())?;
    let mut warnings = ctx.warnings.clone();
    let mut hide: Vec<PlannedHide> = Vec::new();
    let mut skipped: Vec<PlanSkip> = Vec::new();
    let mut chosen: HashSet<usize> = HashSet::new();

    if request.use_policy {
        if !policy.retirement.enabled {
            warnings.push(
                "retirement is disabled in the policy; policy-driven retirement is paused".into(),
            );
        } else if let Coverage::Unproven { reason } = ctx.access.coverage(now) {
            warnings.push(format!(
                "no qualifying access history is proven complete ({}); policy-driven retirement waits until access has been recorded without gaps for {}",
                reason, policy.retirement.inactivity
            ));
        } else {
            if let (Coverage::Proven { since }, Some(start)) =
                (ctx.access.coverage(now), ctx.access.tracking_since)
                && since > start
            {
                warnings.push(format!(
                    "access history has gaps; inactivity counts from {} (see `pkg6repo retire status`)",
                    rfc(since)
                ));
            }
            let mut unknown = 0usize;
            for i in 0..ctx.u.len() {
                if ctx.hidden.contains(&i) || !ctx.u.is_superseded(i) {
                    continue;
                }
                match eligibility(&ctx, i, policy.retirement.inactivity, now) {
                    Eligibility::Eligible { since } => {
                        if let Some(rule) = policy.keep.find(&ctx.u.packages[i].fmri) {
                            skipped.push(PlanSkip {
                                fmri: ctx.u.packages[i].fmri.clone(),
                                reason: format!("eligible but kept by {}", rule),
                            });
                        } else if chosen.insert(i) {
                            hide.push(planned(&ctx, i, HideReason::Inactive, Some(since)));
                        }
                    }
                    Eligibility::NotYet { .. } => {}
                    Eligibility::Unknown(_) => unknown += 1,
                }
            }
            if unknown > 0 {
                warnings.push(format!(
                    "{} superseded version(s) have successors without a known publication time and are never policy candidates",
                    unknown
                ));
            }
        }
    }

    for rule in &request.select {
        let matched: Vec<usize> = (0..ctx.u.len())
            .filter(|&i| rule.matches(&ctx.u.packages[i].fmri))
            .collect();
        if matched.is_empty() {
            return Err(RetentionError::InvalidRule {
                rule: rule.to_string(),
                detail: format!("matches no package version in generation {}", info.id),
            });
        }
        for i in matched {
            let fmri = ctx.u.packages[i].fmri.clone();
            if ctx.hidden.contains(&i) {
                skipped.push(PlanSkip {
                    fmri,
                    reason: "already hidden".into(),
                });
            } else if !ctx.u.is_superseded(i) {
                skipped.push(PlanSkip {
                    fmri,
                    reason: "newest version of its package; only superseded versions are retired"
                        .into(),
                });
            } else if let Some(k) = policy.keep.find(&fmri) {
                skipped.push(PlanSkip {
                    fmri,
                    reason: format!("kept by {}", k),
                });
            } else if chosen.insert(i) {
                hide.push(planned(&ctx, i, HideReason::Operator, None));
            }
        }
    }

    let b = check_hiding(&ctx.u, &ctx.hidden, &chosen, &ctx.closures, &|_| true, true);
    if !b.is_empty() {
        return Err(blocked(b));
    }

    hide.sort_by(|a, b| a.fmri.cmp(&b.fmri));
    skipped.sort_by(|a, b| a.fmri.cmp(&b.fmri));
    skipped.dedup();
    let mut plan = RetirementPlan {
        format: PLAN_FORMAT,
        id: String::new(),
        created: rfc(now),
        repository: repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        base_generation: info.id.clone(),
        policy_digest: policy.digest.clone(),
        policy_driven: request.use_policy,
        selections: request.select.iter().map(|r| r.to_string()).collect(),
        hide,
        skipped,
        warnings,
    };
    plan.id = plan.compute_id();
    Ok(plan)
}

/// Apply a plan: recheck it under the writer lock against the published
/// generation and current access records, then publish a generation that
/// hides its versions. Idempotent: a plan already in the audit trail, or
/// whose versions are all hidden, changes nothing.
pub fn apply_retirement(
    repo: &Path,
    plan: &RetirementPlan,
    policy: &Policy,
    options: &ApplyOptions,
) -> Result<ApplyOutcome> {
    if plan.format != PLAN_FORMAT {
        return Err(RetentionError::PlanFormat(plan.format));
    }
    if !plan.verify_id() {
        return Err(RetentionError::StalePlan {
            problems: vec!["the plan file was modified after it was created".into()],
        });
    }
    let actual = repo.canonicalize().at(repo)?;
    if plan.repository != actual {
        return Err(RetentionError::WrongRepository {
            planned: plan.repository.clone(),
            actual,
        });
    }
    if plan.policy_digest != policy.digest {
        return Err(RetentionError::PolicyChanged {
            planned: plan.policy_digest.clone(),
            current: policy.digest.clone(),
        });
    }
    check_managed(repo)?;
    let mut store = GenerationStore::open_with(repo, options.wait_for_lock, false)?;
    let info = store.current().clone();
    let gen_dir = store.current_dir();
    if let Some(done) = generation::read_audit_records(&gen_dir)?
        .into_iter()
        .find(|r| r.plan_id.as_deref() == Some(plan.id.as_str()))
    {
        return Ok(ApplyOutcome::AlreadyApplied {
            operation: done.operation,
        });
    }
    let now = clock::now();
    let watch = planned_payloads(&gen_dir, plan)?;
    let mut ctx = load_context(repo, &gen_dir, &info, policy, &watch)?;

    let mut problems = Vec::new();
    let mut hide: HashSet<usize> = HashSet::new();
    let mut entries: Vec<PlannedHide> = Vec::new();
    for p in &plan.hide {
        let Some(i) = ctx.u.get(&p.fmri) else {
            problems.push(format!("{}: no longer in the repository", p.fmri));
            continue;
        };
        if ctx.hidden.contains(&i) {
            continue;
        }
        if !ctx.u.is_superseded(i) {
            problems.push(format!("{}: no longer superseded", p.fmri));
            continue;
        }
        if let Some(rule) = policy.keep.find(&p.fmri) {
            problems.push(format!("{}: kept by {}", p.fmri, rule));
            continue;
        }
        if p.reason == HideReason::Inactive {
            if !policy.retirement.enabled {
                problems.push(format!("{}: policy-driven retirement is disabled", p.fmri));
                continue;
            }
            if let Some(problem) = still_inactive(&ctx, i, policy.retirement.inactivity, now) {
                problems.push(problem);
                continue;
            }
        }
        hide.insert(i);
        entries.push(planned(&ctx, i, p.reason, None));
    }
    if !problems.is_empty() {
        return Err(RetentionError::StalePlan { problems });
    }
    if hide.is_empty() {
        return Ok(ApplyOutcome::NothingToDo);
    }
    let b = check_hiding(&ctx.u, &ctx.hidden, &hide, &ctx.closures, &|_| true, true);
    if !b.is_empty() {
        return Err(blocked(b));
    }

    let mut staged = store.stage(GenerationReason::Retire { fmris: hide.len() }, &[])?;
    let operation = staged.id().to_string();
    let mut state = ctx.state.clone();
    let mut publishers = BTreeSet::new();
    for e in &entries {
        publishers.insert(e.publisher.clone());
        state.hidden.insert(
            e.fmri.clone(),
            HiddenEntry {
                publisher: e.publisher.clone(),
                hidden_at: rfc(now),
                operation: operation.clone(),
                reason: e.reason,
                superseded_at: e.superseded_at.clone(),
                last_access: e.last_access.clone(),
            },
        );
    }
    for p in &publishers {
        staged.mark_rebuild(p)?;
    }
    staged.put_retirement_state(&state)?;
    let mut fmris: Vec<String> = entries.iter().map(|e| e.fmri.clone()).collect();
    fmris.sort();
    staged.put_audit(&AuditRecord {
        operation: operation.clone(),
        kind: AuditKind::Retire,
        at: rfc(now),
        actor: current_actor(),
        parent_generation: Some(info.id.clone()),
        plan_id: Some(plan.id.clone()),
        planned_generation: Some(plan.base_generation.clone()),
        policy_digest: policy.digest.clone(),
        fmris: fmris.clone(),
    })?;
    // Final check, synchronised with the switch: under the access fence
    // (held until the switch is done) every policy candidate must still be
    // inactive with the access records as they are now.
    let candidates: Vec<(usize, String)> = entries
        .iter()
        .filter(|e| e.reason == HideReason::Inactive)
        .filter_map(|e| ctx.u.get(&e.fmri).map(|i| (i, e.fmri.clone())))
        .collect();
    let interval = policy.retirement.inactivity;
    let committed = store.commit_gated(staged, options.fail_point, || {
        options.at(ApplyStage::BeforeFinalCheck);
        if candidates.is_empty() {
            return Ok(None);
        }
        let fence = AccessFence::acquire(repo, options.fence_timeout)?;
        options.at(ApplyStage::Fenced);
        ctx.set_access(AccessLog::load(repo)?);
        let now = clock::now();
        let problems: Vec<String> = candidates
            .iter()
            .filter_map(|(i, _)| still_inactive(&ctx, *i, interval, now))
            .collect();
        if !problems.is_empty() {
            return Err(RetentionError::StalePlan { problems });
        }
        Ok(Some(fence))
    })?;
    info!(generation = %committed.id, hidden = fmris.len(), "retirement applied");
    fold_access(repo);
    Ok(ApplyOutcome::Applied {
        generation: committed.id,
        hidden: fmris,
        rechecked: info.id != plan.base_generation,
    })
}

/// `None` if policy candidate `i` is still eligible at `now`, else why not.
fn still_inactive(
    ctx: &Context,
    i: usize,
    interval: Interval,
    now: DateTime<Utc>,
) -> Option<String> {
    let fmri = &ctx.u.packages[i].fmri;
    match eligibility(ctx, i, interval, now) {
        Eligibility::Eligible { .. } => None,
        Eligibility::NotYet { until } => {
            let accessed = ctx
                .last_access
                .get(&i)
                .map(|t| format!(" (last accessed {})", rfc(*t)))
                .unwrap_or_default();
            Some(format!(
                "{}: not inactive until {}{}",
                fmri,
                rfc(until),
                accessed
            ))
        }
        Eligibility::Unknown(why) => Some(format!("{}: no longer eligible: {}", fmri, why)),
    }
}

fn fold_access(repo: &Path) {
    if let Err(e) = access::compact(repo) {
        warn!(error = %e, "failed to fold access records; they are kept and folded later");
    }
}

/// Make hidden versions visible again in a new generation. `select` rules
/// are matched against the hidden set; a rule matching no version at all is
/// an error. Restoring a version whose hard dependencies are satisfiable
/// only by other hidden versions is blocked, naming them.
pub fn restore_retired(
    repo: &Path,
    select: &[FmriRule],
    policy: &Policy,
    options: &RestoreOptions,
) -> Result<RestoreOutcome> {
    check_managed(repo)?;
    let mut store = GenerationStore::open_with(repo, options.wait_for_lock, false)?;
    let info = store.current().clone();
    let gen_dir = store.current_dir();
    let ctx = load_context(repo, &gen_dir, &info, policy, &HashSet::new())?;
    let now = clock::now();

    let mut restore: BTreeSet<usize> = BTreeSet::new();
    let mut visible: Vec<String> = Vec::new();
    let mut unmatched: Vec<String> = Vec::new();
    for rule in select {
        let matched: Vec<usize> = (0..ctx.u.len())
            .filter(|&i| rule.matches(&ctx.u.packages[i].fmri))
            .collect();
        if matched.is_empty() {
            unmatched.push(rule.to_string());
        }
        for i in matched {
            if ctx.hidden.contains(&i) {
                restore.insert(i);
            } else {
                visible.push(ctx.u.packages[i].fmri.clone());
            }
        }
    }
    if !unmatched.is_empty() {
        return Err(RetentionError::NothingMatched {
            patterns: unmatched,
        });
    }
    if restore.is_empty() {
        visible.sort();
        visible.dedup();
        return Ok(RestoreOutcome::AlreadyVisible { fmris: visible });
    }

    // Every hard dependency of a restored version must be satisfiable by
    // versions visible after the restore.
    let mut b = Blockers::default();
    let after = |j: &usize| !ctx.hidden.contains(j) || restore.contains(j);
    for &i in &restore {
        for dep in &ctx.u.packages[i].deps {
            if !matches!(
                dep.kind,
                DepKind::Require | DepKind::RequireAny | DepKind::Incorporate
            ) {
                continue;
            }
            let sats: Vec<Vec<usize>> = dep
                .targets
                .iter()
                .map(|t| ctx.u.satisfiers(i, dep.kind, t))
                .collect();
            let exists = sats.iter().any(|s| !s.is_empty());
            if exists && !sats.iter().any(|s| s.iter().any(after)) {
                let texts: Vec<&str> = dep.targets.iter().map(|t| t.text.as_str()).collect();
                b.blockers.push(format!(
                    "{} depends on {} ({:?}), which only hidden versions satisfy",
                    ctx.u.packages[i].fmri,
                    texts.join(", "),
                    dep.kind
                ));
                if let Some(j) = sats.iter().flat_map(|s| s.iter().rev()).next() {
                    b.restores.insert(ctx.u.packages[*j].fmri.clone());
                }
            }
        }
    }
    if !b.is_empty() {
        return Err(blocked(b));
    }

    let fmris: Vec<String> = restore
        .iter()
        .map(|&i| ctx.u.packages[i].fmri.clone())
        .collect();
    if options.dry_run {
        return Ok(RestoreOutcome::DryRun {
            would_restore: fmris,
        });
    }
    let mut staged = store.stage(GenerationReason::Restore { fmris: fmris.len() }, &[])?;
    let operation = staged.id().to_string();
    let mut state = ctx.state.clone();
    let mut publishers = BTreeSet::new();
    for f in &fmris {
        if let Some(e) = state.hidden.remove(f) {
            publishers.insert(e.publisher);
        }
    }
    for p in &publishers {
        staged.mark_rebuild(p)?;
    }
    staged.put_retirement_state(&state)?;
    staged.put_audit(&AuditRecord {
        operation,
        kind: AuditKind::Restore,
        at: rfc(now),
        actor: current_actor(),
        parent_generation: Some(info.id.clone()),
        plan_id: None,
        planned_generation: None,
        policy_digest: policy.digest.clone(),
        fmris: fmris.clone(),
    })?;
    let committed = store.commit(staged, options.fail_point)?;
    info!(generation = %committed.id, restored = fmris.len(), "hidden versions restored");
    Ok(RestoreOutcome::Restored {
        generation: committed.id,
        restored: fmris,
    })
}

/// Policy settings, hidden versions with their latest access, the audit
/// trail, and snapshot problems of the published generation.
pub fn retirement_status(repo: &Path, policy: &Policy) -> Result<RetirementStatus> {
    let info = check_managed(repo)?;
    let gen_dir = repo.join(GENERATIONS_DIR).join(&info.id);
    let ctx = load_context(repo, &gen_dir, &info, policy, &HashSet::new())?;
    let mut problems = ctx.problems.clone();
    let b = check_hiding(
        &ctx.u,
        &ctx.hidden,
        &HashSet::new(),
        &ctx.closures,
        &|_| true,
        true,
    );
    problems.extend(b.blockers);
    let coverage = ctx.access.coverage(clock::now());
    let hidden = ctx
        .state
        .hidden
        .iter()
        .map(|(fmri, entry)| HiddenStatus {
            fmri: fmri.clone(),
            entry: entry.clone(),
            last_access: ctx
                .u
                .get(fmri)
                .and_then(|i| ctx.last_access.get(&i))
                .copied()
                .map(rfc),
        })
        .collect();
    Ok(RetirementStatus {
        generation: info.id.clone(),
        enabled: policy.retirement.enabled,
        inactivity: policy.retirement.inactivity.to_string(),
        purge_grace: policy.retirement.purge_grace.to_string(),
        tracking_since: ctx.access.tracking_since.map(rfc),
        access_proven_since: match &coverage {
            Coverage::Proven { since } => Some(rfc(*since)),
            Coverage::Unproven { .. } => None,
        },
        access_unproven: match coverage {
            Coverage::Proven { .. } => None,
            Coverage::Unproven { reason } => Some(reason),
        },
        access_gaps: ctx
            .access
            .gaps()
            .into_iter()
            .map(|g| GapStatus {
                recorder: g.recorder,
                from: rfc(g.from),
                until: rfc(g.until),
                reason: g.reason,
            })
            .collect(),
        recorders: ctx
            .access
            .open_runs()
            .into_iter()
            .map(|r| RecorderStatus {
                name: r.instance,
                last_record: rfc(r.last_record),
            })
            .collect(),
        hidden,
        audit: generation::read_audit_records(&gen_dir)?,
        problems,
        malformed_access_lines: ctx.access.malformed_lines,
    })
}
