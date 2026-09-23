//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Repository policy: retirement settings, keep rules, import exclusions and
//! protected snapshot sets.
//!
//! The policy is a KDL file, `<repo>/policy.kdl` unless another path is
//! given:
//!
//! ```kdl
//! retirement {
//!     enabled true              // policy-driven retirement; default false
//!     inactivity "12 months"    // since supersession and last access
//!     purge-grace "12 months"   // recorded for the purge stage
//! }
//! keep "pkg://openindiana.org/web/server/nginx@1.20,5.11-2021.0.0.1:20210601T101010Z"
//! keep regex="^pkg://openindiana.org/consolidation/.*"
//! keep-file "keep.txt"          // text, KDL or JSON, relative to this file
//! import {
//!     exclude regex="^pkg://openindiana.org/developer/.*"
//!     exclude "pkg://openindiana.org/library/foo@1.0,5.11-1:20200101T000000Z"
//! }
//! snapshot "hipster-2020.10" as-of="20201031T235959Z" {
//!     source iso="OI-hipster-gui-20201031.iso" sha256="..."
//!     fmri "pkg://openindiana.org/entire@2020.10,5.11-2020.0.1.0:20201031T010203Z"
//! }
//! ```
//!
//! A rule is exact (a full FMRI with publisher and timestamped version) or a
//! regular expression matched against the full FMRI
//! `pkg://<publisher>/<name>@<version>`. `keep` rules stop both retirement
//! and import exclusion of the versions they match. Every file is validated
//! completely before any command changes anything.

use super::{IoContext, Result, RetentionError};
use crate::fmri::Fmri;
use chrono::{DateTime, Months, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde::Deserialize;
use sha2::Digest as _;
use std::fmt;
use std::path::{Path, PathBuf};

/// Default policy file below the repository root.
pub const POLICY_FILE: &str = "policy.kdl";

/// Length of a policy interval. Months are calendar months.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    Days(u32),
    Months(u32),
}

/// Longest interval accepted (100 years).
const MAX_INTERVAL_MONTHS: u32 = 1200;

impl Interval {
    pub const TWELVE_MONTHS: Interval = Interval::Months(12);

    /// Parse `"12 months"`, `"1 year"`, `"90 days"`, `"8 weeks"` (also
    /// `12mo`, `90d`, `8w`, `1y`).
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let t = s.trim();
        let split = t
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("{:?} has no unit (days, weeks, months, years)", s))?;
        let (num, unit) = t.split_at(split);
        let n: u32 = num
            .parse()
            .map_err(|_| format!("{:?} does not start with a whole number", s))?;
        if n == 0 {
            return Err(format!("{:?} must be longer than zero", s));
        }
        let interval = match unit.trim() {
            "d" | "day" | "days" => Interval::Days(n),
            "w" | "week" | "weeks" => Interval::Days(
                n.checked_mul(7)
                    .ok_or_else(|| format!("{:?} is too long", s))?,
            ),
            "mo" | "month" | "months" => Interval::Months(n),
            "y" | "year" | "years" => Interval::Months(
                n.checked_mul(12)
                    .ok_or_else(|| format!("{:?} is too long", s))?,
            ),
            other => {
                return Err(format!(
                    "unknown unit {:?} in {:?} (use days, weeks, months or years)",
                    other, s
                ));
            }
        };
        let months = match interval {
            Interval::Days(d) => d / 28,
            Interval::Months(m) => m,
        };
        if months > MAX_INTERVAL_MONTHS {
            return Err(format!("{:?} is longer than 100 years", s));
        }
        Ok(interval)
    }

    /// `t` plus this interval (calendar months clamp to the month's end).
    pub fn after(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Interval::Days(d) => t + chrono::Duration::days(i64::from(*d)),
            Interval::Months(m) => t
                .checked_add_months(Months::new(*m))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Interval::Days(1) => write!(f, "1 day"),
            Interval::Days(d) => write!(f, "{} days", d),
            Interval::Months(1) => write!(f, "1 month"),
            Interval::Months(m) => write!(f, "{} months", m),
        }
    }
}

/// Settings of policy-driven retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementPolicy {
    /// Disabled by default. Disabling pauses policy-driven retirement; it
    /// neither restores hidden versions nor purges anything.
    pub enabled: bool,
    /// A superseded version becomes a candidate once this interval has
    /// passed since the later of its supersession, its last qualifying
    /// access and the start of access tracking.
    pub inactivity: Interval,
    /// Minimum time a version stays hidden (after hiding or its latest
    /// access) before the purge stage may remove it. Validated and
    /// reported; there is no purge in this release.
    pub purge_grace: Interval,
}

impl Default for RetirementPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            inactivity: Interval::TWELVE_MONTHS,
            purge_grace: Interval::TWELVE_MONTHS,
        }
    }
}

/// An FMRI matching rule.
#[derive(Debug, Clone)]
pub enum FmriRule {
    /// A canonical full FMRI (`pkg://pub/name@version`).
    Exact(String),
    /// A regular expression matched against the canonical full FMRI.
    Regex(Regex),
}

impl FmriRule {
    /// An exact rule. The FMRI needs a publisher and a timestamped version.
    pub fn exact(fmri: &str) -> Result<Self> {
        canonical_exact_fmri(fmri)
            .map(FmriRule::Exact)
            .map_err(|detail| RetentionError::InvalidRule {
                rule: fmri.to_string(),
                detail,
            })
    }

    pub fn regex(pattern: &str) -> Result<Self> {
        Regex::new(pattern)
            .map(FmriRule::Regex)
            .map_err(|e| RetentionError::InvalidRule {
                rule: pattern.to_string(),
                detail: e.to_string(),
            })
    }

    /// Parse the command-line and text form: `regex:<re>`, `exact:<fmri>`
    /// or a bare exact FMRI.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some(re) = s.strip_prefix("regex:") {
            Self::regex(re)
        } else if let Some(f) = s.strip_prefix("exact:") {
            Self::exact(f.trim())
        } else {
            Self::exact(s)
        }
    }

    pub fn matches(&self, fmri: &str) -> bool {
        match self {
            FmriRule::Exact(e) => e == fmri,
            FmriRule::Regex(r) => r.is_match(fmri),
        }
    }
}

impl fmt::Display for FmriRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FmriRule::Exact(e) => f.write_str(e),
            FmriRule::Regex(r) => write!(f, "regex:{}", r.as_str()),
        }
    }
}

/// The canonical full form of an exact FMRI, as the repository names its
/// versions: `pkg://<publisher>/<name>@<version>`.
pub fn canonical_exact_fmri(s: &str) -> std::result::Result<String, String> {
    let fmri = Fmri::parse(s.trim()).map_err(|e| e.to_string())?;
    let publisher = fmri
        .publisher
        .clone()
        .ok_or_else(|| "an exact FMRI needs a publisher (pkg://<publisher>/...)".to_string())?;
    let version = fmri
        .version
        .clone()
        .ok_or_else(|| "an exact FMRI needs a version".to_string())?;
    if version.timestamp.is_none() {
        return Err("an exact FMRI needs the full version including its :timestamp".to_string());
    }
    Ok(Fmri::with_publisher(&publisher, &fmri.name, Some(version)).to_string())
}

/// Ordered rules with the file each came from.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<(FmriRule, String)>,
}

impl RuleSet {
    pub fn push(&mut self, rule: FmriRule, origin: impl Into<String>) {
        self.rules.push((rule, origin.into()));
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The first rule matching the full FMRI.
    pub fn find(&self, fmri: &str) -> Option<&FmriRule> {
        self.rules
            .iter()
            .find(|(r, _)| r.matches(fmri))
            .map(|(r, _)| r)
    }

    pub fn matches(&self, fmri: &str) -> bool {
        self.find(fmri).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FmriRule, &str)> {
        self.rules.iter().map(|(r, o)| (r, o.as_str()))
    }
}

/// Where the content of a protected snapshot came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSource {
    pub iso: Option<String>,
    pub sha256: Option<String>,
    pub note: Option<String>,
}

/// A protected historical snapshot: exact package versions (typically the
/// incorporations and packages of an installation ISO) whose dependency
/// closure must stay visible to pkg5 clients.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub name: String,
    /// Dependencies not pinned by the snapshot's incorporations resolve to
    /// the newest version published at or before this time. Defaults to the
    /// newest timestamp among the members.
    pub as_of: Option<DateTime<Utc>>,
    pub sources: Vec<SnapshotSource>,
    /// Canonical exact FMRIs.
    pub members: Vec<String>,
}

/// The effective repository policy.
#[derive(Debug, Clone)]
pub struct Policy {
    pub retirement: RetirementPolicy,
    /// Versions that are never retired or excluded from import.
    pub keep: RuleSet,
    /// Versions an import leaves out (unless kept).
    pub import_exclude: RuleSet,
    pub snapshots: Vec<Snapshot>,
    /// Files read, in order.
    pub sources: Vec<PathBuf>,
    /// SHA-256 over every source path and content; plans are bound to it.
    pub digest: String,
}

impl Default for Policy {
    fn default() -> Self {
        let mut p = Self {
            retirement: RetirementPolicy::default(),
            keep: RuleSet::default(),
            import_exclude: RuleSet::default(),
            snapshots: vec![],
            sources: vec![],
            digest: String::new(),
        };
        p.digest = PolicyDigest::default().finish();
        p
    }
}

#[derive(Default)]
struct PolicyDigest(sha2::Sha256);

impl PolicyDigest {
    fn add(&mut self, path: &Path, bytes: &[u8]) {
        self.0.update(path.to_string_lossy().as_bytes());
        self.0.update([0]);
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    fn finish(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}

/// KDL document shapes. Kept apart because the knuffel derive expands to
/// code naming `Result`, which this module aliases.
mod doc {
    #[derive(knuffel::Decode, Debug, Default)]
    pub(super) struct PolicyDoc {
        #[knuffel(child)]
        pub(super) retirement: Option<RetirementNode>,
        #[knuffel(child)]
        pub(super) import: Option<ImportNode>,
        #[knuffel(children(name = "keep"))]
        pub(super) keep: Vec<RuleNode>,
        #[knuffel(children(name = "keep-file"))]
        pub(super) keep_files: Vec<ArgNode>,
        #[knuffel(children(name = "snapshot"))]
        pub(super) snapshots: Vec<SnapshotNode>,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct RetirementNode {
        #[knuffel(child, unwrap(argument))]
        pub(super) enabled: Option<bool>,
        #[knuffel(child, unwrap(argument))]
        pub(super) inactivity: Option<String>,
        #[knuffel(child, unwrap(argument))]
        pub(super) purge_grace: Option<String>,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct ImportNode {
        #[knuffel(children(name = "exclude"))]
        pub(super) exclude: Vec<RuleNode>,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct RuleNode {
        #[knuffel(argument)]
        pub(super) exact: Option<String>,
        #[knuffel(property)]
        pub(super) regex: Option<String>,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct ArgNode {
        #[knuffel(argument)]
        pub(super) value: String,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct SnapshotNode {
        #[knuffel(argument)]
        pub(super) name: String,
        #[knuffel(property(name = "as-of"))]
        pub(super) as_of: Option<String>,
        #[knuffel(children(name = "source"))]
        pub(super) sources: Vec<SourceNode>,
        #[knuffel(children(name = "fmri"))]
        pub(super) fmris: Vec<ArgNode>,
    }

    #[derive(knuffel::Decode, Debug)]
    pub(super) struct SourceNode {
        #[knuffel(property)]
        pub(super) iso: Option<String>,
        #[knuffel(property)]
        pub(super) sha256: Option<String>,
        #[knuffel(property)]
        pub(super) note: Option<String>,
    }

    #[derive(knuffel::Decode, Debug, Default)]
    pub(super) struct KeepFileDoc {
        #[knuffel(children(name = "keep"))]
        pub(super) keep: Vec<RuleNode>,
    }
}
use doc::{KeepFileDoc, PolicyDoc, RuleNode};

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonKeepFile {
    List(Vec<JsonRule>),
    Object { keep: Vec<JsonRule> },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonRule {
    Exact(String),
    Tagged {
        exact: Option<String>,
        regex: Option<String>,
    },
}

fn policy_err(path: &Path, detail: impl Into<String>) -> RetentionError {
    RetentionError::Policy {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

fn kdl_err(path: &Path, err: knuffel::Error) -> RetentionError {
    // knuffel reports through an older miette; its Debug form carries the
    // located messages.
    policy_err(path, format!("{:?}", err))
}

fn rule_from_node(path: &Path, node: &RuleNode, what: &str) -> Result<FmriRule> {
    let rule = match (&node.exact, &node.regex) {
        (Some(e), None) => FmriRule::exact(e),
        (None, Some(r)) => FmriRule::regex(r),
        _ => {
            return Err(policy_err(
                path,
                format!(
                    "`{}` takes either an exact FMRI argument or a regex= property",
                    what
                ),
            ));
        }
    };
    rule.map_err(|e| policy_err(path, format!("`{}`: {}", what, e)))
}

fn parse_as_of(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .map(|n| Utc.from_utc_datetime(&n))
        .or_else(|_| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .map_err(|_| format!("as-of {:?} is neither 20201031T235959Z nor RFC 3339", s))
}

impl Policy {
    /// Load the policy of `repo`: `policy_path` if given (it must exist),
    /// else `<repo>/policy.kdl` if present, else the defaults (retirement
    /// disabled, no rules). `keep_files` add keep rules.
    pub fn load(
        repo: Option<&Path>,
        policy_path: Option<&Path>,
        keep_files: &[PathBuf],
    ) -> Result<Self> {
        let path = match (policy_path, repo) {
            (Some(p), _) => Some(p.to_path_buf()),
            (None, Some(r)) if r.join(POLICY_FILE).exists() => Some(r.join(POLICY_FILE)),
            _ => None,
        };
        let mut policy = Policy::default();
        let mut digest = PolicyDigest::default();
        if let Some(path) = &path {
            let bytes = std::fs::read(path).at(path)?;
            digest.add(path, &bytes);
            policy.sources.push(path.clone());
            let text = String::from_utf8(bytes).map_err(|_| policy_err(path, "not UTF-8"))?;
            let doc: PolicyDoc =
                knuffel::parse(&path.to_string_lossy(), &text).map_err(|e| kdl_err(path, e))?;
            policy.apply_doc(path, doc, &mut digest)?;
        }
        for file in keep_files {
            policy.load_keep_file(file, &mut digest)?;
        }
        policy.digest = digest.finish();
        Ok(policy)
    }

    /// Parse a policy from text (tests and validation).
    pub fn from_kdl(name: &str, text: &str) -> Result<Self> {
        let path = PathBuf::from(name);
        let doc: PolicyDoc = knuffel::parse(name, text).map_err(|e| kdl_err(&path, e))?;
        let mut policy = Policy::default();
        let mut digest = PolicyDigest::default();
        digest.add(&path, text.as_bytes());
        policy.apply_doc(&path, doc, &mut digest)?;
        policy.digest = digest.finish();
        Ok(policy)
    }

    fn apply_doc(&mut self, path: &Path, doc: PolicyDoc, digest: &mut PolicyDigest) -> Result<()> {
        let origin = path.display().to_string();
        if let Some(r) = doc.retirement {
            if let Some(e) = r.enabled {
                self.retirement.enabled = e;
            }
            if let Some(i) = r.inactivity {
                self.retirement.inactivity = Interval::parse(&i)
                    .map_err(|d| policy_err(path, format!("retirement inactivity: {}", d)))?;
            }
            if let Some(g) = r.purge_grace {
                self.retirement.purge_grace = Interval::parse(&g)
                    .map_err(|d| policy_err(path, format!("retirement purge-grace: {}", d)))?;
            }
        }
        for node in &doc.keep {
            self.keep
                .push(rule_from_node(path, node, "keep")?, origin.clone());
        }
        if let Some(import) = doc.import {
            for node in &import.exclude {
                self.import_exclude
                    .push(rule_from_node(path, node, "exclude")?, origin.clone());
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for s in doc.snapshots {
            if !names.insert(s.name.clone()) {
                return Err(policy_err(
                    path,
                    format!("snapshot {:?} is defined twice", s.name),
                ));
            }
            let mut members = Vec::new();
            for f in &s.fmris {
                members.push(canonical_exact_fmri(&f.value).map_err(|d| {
                    policy_err(path, format!("snapshot {:?}: {:?}: {}", s.name, f.value, d))
                })?);
            }
            if members.is_empty() {
                return Err(policy_err(
                    path,
                    format!("snapshot {:?} lists no fmri", s.name),
                ));
            }
            let as_of = s
                .as_of
                .as_deref()
                .map(parse_as_of)
                .transpose()
                .map_err(|d| policy_err(path, format!("snapshot {:?}: {}", s.name, d)))?;
            self.snapshots.push(Snapshot {
                name: s.name,
                as_of,
                sources: s
                    .sources
                    .into_iter()
                    .map(|n| SnapshotSource {
                        iso: n.iso,
                        sha256: n.sha256,
                        note: n.note,
                    })
                    .collect(),
                members,
            });
        }
        let base = path.parent().unwrap_or(Path::new("."));
        for f in doc.keep_files {
            let file = base.join(&f.value);
            self.load_keep_file(&file, digest)?;
        }
        Ok(())
    }

    fn load_keep_file(&mut self, path: &Path, digest: &mut PolicyDigest) -> Result<()> {
        let bytes = std::fs::read(path).at(path)?;
        digest.add(path, &bytes);
        self.sources.push(path.to_path_buf());
        let text =
            String::from_utf8(bytes).map_err(|_| policy_err(path, "keep file is not UTF-8"))?;
        let origin = path.display().to_string();
        match path.extension().and_then(|e| e.to_str()) {
            Some("kdl") => {
                let doc: KeepFileDoc =
                    knuffel::parse(&path.to_string_lossy(), &text).map_err(|e| kdl_err(path, e))?;
                for node in &doc.keep {
                    self.keep
                        .push(rule_from_node(path, node, "keep")?, origin.clone());
                }
            }
            Some("json") => {
                let doc: JsonKeepFile = serde_json::from_str(&text).map_err(|e| {
                    policy_err(
                        path,
                        format!(
                            "{} (expected [\"<fmri>\", {{\"regex\": \"...\"}}] or {{\"keep\": [...]}})",
                            e
                        ),
                    )
                })?;
                let rules = match doc {
                    JsonKeepFile::List(r) | JsonKeepFile::Object { keep: r } => r,
                };
                for r in rules {
                    let rule = match r {
                        JsonRule::Exact(e)
                        | JsonRule::Tagged {
                            exact: Some(e),
                            regex: None,
                        } => FmriRule::exact(&e),
                        JsonRule::Tagged {
                            exact: None,
                            regex: Some(re),
                        } => FmriRule::regex(&re),
                        _ => {
                            return Err(policy_err(
                                path,
                                "each entry is a string or has exactly one of \"exact\" and \"regex\"",
                            ));
                        }
                    }
                    .map_err(|e| policy_err(path, e.to_string()))?;
                    self.keep.push(rule, origin.clone());
                }
            }
            _ => {
                for (n, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let rule = FmriRule::parse(line)
                        .map_err(|e| policy_err(path, format!("line {}: {}", n + 1, e)))?;
                    self.keep.push(rule, format!("{}:{}", origin, n + 1));
                }
            }
        }
        Ok(())
    }

    pub fn is_kept(&self, fmri: &str) -> bool {
        self.keep.matches(fmri)
    }

    /// Whether an import leaves `fmri` out: an exclusion matches and no keep
    /// rule overrides it.
    pub fn import_excludes(&self, fmri: &str) -> bool {
        self.import_exclude.matches(fmri) && !self.keep.matches(fmri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F1: &str = "pkg://oi/library/foo@1.0,5.11-2020.0.1.1:20200101T000000Z";

    #[test]
    fn parses_full_policy() {
        let p = Policy::from_kdl(
            "policy.kdl",
            &format!(
                r#"
retirement {{
    enabled true
    inactivity "90 days"
    purge-grace "1 year"
}}
keep "{F1}"
keep regex="^pkg://oi/consolidation/"
import {{
    exclude regex="^pkg://oi/developer/"
}}
snapshot "hipster-2020.10" as-of="20201031T235959Z" {{
    source iso="OI-hipster-gui-20201031.iso" sha256="abc"
    fmri "{F1}"
}}
"#
            ),
        )
        .unwrap();
        assert!(p.retirement.enabled);
        assert_eq!(p.retirement.inactivity, Interval::Days(90));
        assert_eq!(p.retirement.purge_grace, Interval::Months(12));
        assert!(p.is_kept(F1));
        assert!(p.is_kept("pkg://oi/consolidation/x@1:20200101T000000Z"));
        assert!(p.import_excludes("pkg://oi/developer/gcc@1:20200101T000000Z"));
        assert_eq!(p.snapshots[0].members, vec![F1.to_string()]);
        assert_eq!(
            p.snapshots[0].sources[0].iso.as_deref(),
            Some("OI-hipster-gui-20201031.iso")
        );
    }

    #[test]
    fn defaults_are_disabled_twelve_months() {
        let p = Policy::from_kdl("p.kdl", "").unwrap();
        assert!(!p.retirement.enabled);
        assert_eq!(p.retirement.inactivity, Interval::Months(12));
        assert_eq!(p.retirement.purge_grace, Interval::Months(12));
    }

    #[test]
    fn rejects_invalid_rules_and_durations() {
        for bad in [
            "retirement { inactivity \"0 days\" }",
            "retirement { inactivity \"12 fortnights\" }",
            "retirement { inactivity \"months\" }",
            "keep regex=\"(unclosed\"",
            "keep \"pkg:/foo@1.0\"",
            "keep \"pkg://oi/foo@1.0\"",
            "keep",
            "keep \"pkg://oi/a@1:20200101T000000Z\" regex=\"x\"",
            "snapshot \"s\" { }",
            "snapshot \"s\" as-of=\"yesterday\" { fmri \"pkg://oi/a@1:20200101T000000Z\" }",
            "unknown-node 1",
        ] {
            assert!(Policy::from_kdl("p.kdl", bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn exact_rules_are_canonical() {
        let r =
            FmriRule::parse("pkg://oi/library/foo@1.0,5.11-2020.0.1.1:20200101T000000Z").unwrap();
        assert!(r.matches(F1));
        assert!(
            FmriRule::parse("regex:^pkg://oi/library/")
                .unwrap()
                .matches(F1)
        );
    }

    #[test]
    fn calendar_months() {
        let t = Utc.with_ymd_and_hms(2025, 1, 31, 0, 0, 0).unwrap();
        assert_eq!(
            Interval::Months(1).after(t),
            Utc.with_ymd_and_hms(2025, 2, 28, 0, 0, 0).unwrap()
        );
        assert_eq!(
            Interval::parse("1y").unwrap().after(t),
            Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn keep_files_in_every_format() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("keep.txt");
        std::fs::write(&txt, format!("# comment\n{F1}\nregex:^pkg://oi/a/\n")).unwrap();
        let json = dir.path().join("keep.json");
        std::fs::write(&json, r#"{"keep": [{"regex": "^pkg://oi/b/"}]}"#).unwrap();
        let kdl = dir.path().join("keep.kdl");
        std::fs::write(&kdl, "keep regex=\"^pkg://oi/c/\"\n").unwrap();
        let p = Policy::load(None, None, &[txt, json, kdl.clone()]).unwrap();
        assert!(p.is_kept(F1));
        for s in ["a", "b", "c"] {
            assert!(p.is_kept(&format!("pkg://oi/{s}/x@1:20200101T000000Z")));
        }
        let before = p.digest.clone();
        std::fs::write(&kdl, "keep regex=\"^pkg://oi/d/\"\n").unwrap();
        let p2 = Policy::load(None, None, &[kdl]).unwrap();
        assert_ne!(before, p2.digest);

        let bad = dir.path().join("bad.txt");
        std::fs::write(&bad, "pkg:/no-publisher@1.0\n").unwrap();
        assert!(Policy::load(None, None, &[bad]).is_err());
    }
}
