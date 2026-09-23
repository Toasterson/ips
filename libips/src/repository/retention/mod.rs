//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Repository retirement (first stage), protected snapshots and import
//! exclusions.
//!
//! Retirement hides superseded package versions from the catalogs and search
//! of a generation-managed repository. Hidden manifests and payloads stay in
//! place, so exact `manifest/0` and `file/0` requests keep working. Nothing
//! is ever deleted here: purge is a separate, later stage.
//!
//! - [`policy`]: the KDL repository policy (`<repo>/policy.kdl`): whether
//!   policy-driven retirement is enabled and its intervals, `keep` rules,
//!   import exclusions and protected snapshot sets, plus keep files in
//!   text, KDL or JSON.
//! - [`access`]: durable accounting of qualifying (manifest and payload)
//!   requests, written by the depot, with the heartbeats that prove it
//!   complete; retirement for inactivity fails closed where it is not.
//! - [`clock`]: wall-clock time; only tests can pin it.
//! - [`universe`]: pkg5 dependency and snapshot closure over a generation.
//! - [`plan`]: plan, apply, restore and status. Apply and restore rerun
//!   every check under the writer lock and publish through one generation
//!   switch; apply checks access once more under the access fence right
//!   before that switch. The hidden set and the audit trail live in the
//!   generation.
//!
//! pkg5 obsoletion (`pkg.obsolete=true`) is unrelated: obsolete packages are
//! published and stay in the catalog as in pkg5.

pub mod access;
pub mod clock;
pub mod plan;
pub mod policy;
pub mod state;
pub mod universe;
pub mod version;

pub use access::{
    AccessFence, AccessKind, AccessLog, AccessRecorder, Coverage, Gap, OpenRun, Recorded,
    close_recorder,
};
#[cfg(any(test, feature = "test-clock"))]
pub use plan::ApplyHook;
pub use plan::{
    ApplyOptions, ApplyOutcome, ApplyStage, PlanRequest, PlannedHide, RestoreOptions,
    RestoreOutcome, RetirementPlan, RetirementStatus, apply_retirement, plan_retirement,
    restore_retired, retirement_status,
};
pub use policy::{FmriRule, Interval, Policy, RetirementPolicy, RuleSet, Snapshot};
pub use state::{AuditKind, AuditRecord, HiddenEntry, HideReason, RetirementState};

use super::RepositoryError;
use super::generation::GenerationError;
use miette::Diagnostic;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, RetentionError>;

fn format_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("  - {}", i))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_suggestions(keeps: &[String], restores: &[String]) -> String {
    let mut out = String::new();
    if !keeps.is_empty() {
        out.push_str("\nSuggested keep entries (add to the policy or a keep file):\n");
        for k in keeps {
            out.push_str(&format!("  keep \"{}\"\n", k));
        }
    }
    if !restores.is_empty() {
        out.push_str("\nHidden versions to restore first:\n");
        for r in restores {
            out.push_str(&format!("  pkg6repo retire restore -s <repo> '{}'\n", r));
        }
    }
    out
}

#[derive(Debug, Error, Diagnostic)]
pub enum RetentionError {
    #[error("invalid policy {path}: {detail}")]
    #[diagnostic(
        code(ips::retention::policy),
        help(
            "Fix the file and rerun; nothing was changed. See pkg6-policy(5) for the syntax of policy and keep files"
        )
    )]
    Policy { path: PathBuf, detail: String },

    #[error("invalid FMRI rule {rule:?}: {detail}")]
    #[diagnostic(
        code(ips::retention::rule),
        help(
            "An exact rule is a full FMRI with publisher and timestamped version (pkg://pub/name@1.0,5.11-1:20240101T000000Z); a regex rule is matched against that form"
        )
    )]
    InvalidRule { rule: String, detail: String },

    #[error(
        "the operation is blocked; nothing was changed:\n{}{}",
        format_list(.blockers),
        format_suggestions(.suggested_keeps, .suggested_restores)
    )]
    #[diagnostic(
        code(ips::retention::blocked),
        help(
            "Protected snapshots and the dependencies of visible packages must stay installable. Add the suggested exact keep entries (or restore the listed versions) after reviewing them, then plan again"
        )
    )]
    Blocked {
        blockers: Vec<String>,
        suggested_keeps: Vec<String>,
        suggested_restores: Vec<String>,
    },

    #[error("the retirement plan no longer holds; nothing was changed:\n{}", format_list(.problems))]
    #[diagnostic(
        code(ips::retention::stale_plan),
        help(
            "The repository or its access records changed since the plan was made. Create a new plan with `pkg6repo retire plan` and review it"
        )
    )]
    StalePlan { problems: Vec<String> },

    #[error("the policy changed since the plan was made (plan {planned}, now {current})")]
    #[diagnostic(
        code(ips::retention::policy_changed),
        help(
            "Plans are bound to the policy and keep files they were made with. Plan again with the current policy"
        )
    )]
    PolicyChanged { planned: String, current: String },

    #[error("the plan was made for repository {planned}, not {actual}")]
    #[diagnostic(
        code(ips::retention::wrong_repository),
        help("Apply the plan to the repository it was made for, or plan again")
    )]
    WrongRepository { planned: PathBuf, actual: PathBuf },

    #[error("no hidden version matches {}", .patterns.join(", "))]
    #[diagnostic(
        code(ips::retention::nothing_matched),
        help("List hidden versions with `pkg6repo retire status -v`; nothing was changed")
    )]
    NothingMatched { patterns: Vec<String> },

    #[error("access recording is not confirmed by: {}", .depots.join(", "))]
    #[diagnostic(
        code(ips::retention::access_unconfirmed),
        help(
            "Retirement for inactivity is applied only after every running depot confirms that it recorded every access. Check that these depots run and can write <repo>/retention/access/ (free space, permissions). A depot that stopped without a clean shutdown stays unconfirmed until it restarts; if it is gone for good, run `pkg6repo retire close-depot`. Nothing was changed"
        )
    )]
    AccessUnconfirmed { depots: Vec<String> },

    #[error("cannot give {path} to the repository owner (uid {uid}): {source}")]
    #[diagnostic(
        code(ips::retention::access_owner),
        help(
            "The depot must be able to write <repo>/retention/access/, so entries created there belong to the owner of the repository. Run pkg6repo as that user (for example `su pkg6srv -c 'pkg6repo retire ...'`) or as root. What this command had just created there was removed again"
        )
    )]
    AccessOwner {
        path: PathBuf,
        uid: u32,
        #[source]
        source: io::Error,
    },

    #[error("no running depot named {instance:?} has recorded access (open: {})", .known.join(", "))]
    #[diagnostic(
        code(ips::retention::unknown_depot),
        help("`pkg6repo retire status` lists the depots that record access")
    )]
    UnknownDepot {
        instance: String,
        known: Vec<String>,
    },

    #[error("depot {instance:?} is still recording access (last record {last})")]
    #[diagnostic(
        code(ips::retention::depot_active),
        help(
            "Only a depot that has stopped reporting for ten minutes can be closed. Stop it first, or let it shut down cleanly"
        )
    )]
    DepotStillReporting { instance: String, last: String },

    #[error("{path} does not publish through repository generations")]
    #[diagnostic(
        code(ips::retention::unmanaged),
        help(
            "Retirement changes catalogs through an atomic generation switch. Import packages with `pkg6repo import-pkg5` (adopting an existing repository with --adopt-existing) first"
        )
    )]
    Unmanaged { path: PathBuf },

    #[error("unsupported plan format {0}")]
    #[diagnostic(
        code(ips::retention::plan_format),
        help("Create the plan with this version of pkg6repo")
    )]
    PlanFormat(u32),

    #[error("I/O error at {path}: {source}")]
    #[diagnostic(
        code(ips::retention::io),
        help("Check that the path exists and is readable (and writable for apply/restore)")
    )]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("retention state is inconsistent: {0}")]
    #[diagnostic(
        code(ips::retention::corrupt),
        help(
            "Inspect generations/current/retirement/ and <repo>/retention/; the generation files are never rewritten in place"
        )
    )]
    Corrupt(String),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Generation(Box<GenerationError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<GenerationError> for RetentionError {
    fn from(err: GenerationError) -> Self {
        RetentionError::Generation(Box::new(err))
    }
}

impl From<RepositoryError> for RetentionError {
    fn from(err: RepositoryError) -> Self {
        RetentionError::Repository(Box::new(err))
    }
}

trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| RetentionError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
