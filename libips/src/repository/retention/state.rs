//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Retirement state and audit records, stored inside each generation:
//!
//! ```text
//! generations/<id>/retirement/state.json            hidden versions
//! generations/<id>/retirement/audit/<op>.json       one record per change
//! ```
//!
//! Both are carried into every later generation (hard links, never
//! rewritten in place), so the hidden set switches atomically with the
//! catalogs that leave those versions out, and the audit trail survives
//! generation cleanup.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Directory inside a generation.
pub const RETIREMENT_DIR: &str = "retirement";
/// Hidden-version state file inside [`RETIREMENT_DIR`].
pub const STATE_FILE: &str = "state.json";
/// Audit record directory inside [`RETIREMENT_DIR`].
pub const AUDIT_DIR: &str = "audit";

/// Why a version was hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HideReason {
    /// Superseded and inactive for the policy interval.
    Inactive,
    /// Selected explicitly by the operator.
    Operator,
}

/// A hidden version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenEntry {
    pub publisher: String,
    /// RFC 3339 time of the retirement.
    pub hidden_at: String,
    /// Operation (generation id) that hid it.
    pub operation: String,
    pub reason: HideReason,
    pub superseded_at: Option<String>,
    /// Last qualifying access known when it was hidden.
    pub last_access: Option<String>,
}

/// Hidden versions of a generation, keyed by canonical full FMRI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementState {
    #[serde(default)]
    pub hidden: BTreeMap<String, HiddenEntry>,
}

impl RetirementState {
    pub fn is_hidden(&self, fmri: &str) -> bool {
        self.hidden.contains_key(fmri)
    }

    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditKind {
    Retire,
    Restore,
}

/// One committed retirement or restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Operation id: the generation that published the change.
    pub operation: String,
    pub kind: AuditKind,
    /// RFC 3339 commit time.
    pub at: String,
    /// Login name of the operator (`SUDO_USER`, `LOGNAME` or `USER`).
    pub actor: String,
    /// Generation the change was checked against and applied on.
    pub parent_generation: Option<String>,
    /// Plan identity (retire only).
    pub plan_id: Option<String>,
    /// Generation the plan was made for; differs from `parent_generation`
    /// if the plan was rechecked against a newer generation.
    pub planned_generation: Option<String>,
    pub policy_digest: String,
    pub fmris: Vec<String>,
}

/// Who runs the command, for audit records.
pub fn current_actor() -> String {
    ["SUDO_USER", "LOGNAME", "USER"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}
