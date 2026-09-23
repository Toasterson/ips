//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! pkg5 version ordering and dependency matching.
//!
//! Follows OpenIndiana pkg5 `src/modules/version.py`: versions order by
//! release, then branch (the part after `-`), then timestamp; a missing
//! component sorts first. The build release (the part after `,`) does not
//! take part in comparisons. libips calls the part after `,` "branch" and
//! the part after `-` "build", so pkg5's branch is [`Version::build`].

use crate::fmri::Version;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use std::cmp::Ordering;
use std::fmt;

/// A package version as pkg5 compares it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pkg5Version {
    release: Vec<u64>,
    branch: Option<Vec<u64>>,
    timestamp: Option<String>,
    text: String,
}

fn dot_sequence(s: &str) -> Option<Vec<u64>> {
    s.split('.').map(|c| c.parse::<u64>().ok()).collect()
}

impl Pkg5Version {
    /// `None` if a component is not a dot sequence of integers.
    pub fn from_version(v: &Version) -> Option<Self> {
        Some(Self {
            release: dot_sequence(&v.release)?,
            branch: match &v.build {
                Some(b) => Some(dot_sequence(b)?),
                None => None,
            },
            timestamp: v.timestamp.clone(),
            text: v.to_string(),
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::from_version(&Version::parse(s).ok()?)
    }

    /// Publication time pkg5 recorded in the FMRI (`20240101T000000Z`).
    pub fn published(&self) -> Option<DateTime<Utc>> {
        let ts = self.timestamp.as_deref()?;
        let naive = NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%SZ")
            .or_else(|_| NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%.fZ"))
            .ok()?;
        Some(Utc.from_utc_datetime(&naive))
    }

    pub fn timestamp(&self) -> Option<&str> {
        self.timestamp.as_deref()
    }

    /// Whether this version satisfies a `require`-style dependency on
    /// `wanted`: pkg5 accepts that version or any successor.
    pub fn satisfies_minimum(&self, wanted: &Pkg5Version) -> bool {
        self.pkg5_cmp(wanted) != Ordering::Less
    }

    /// pkg5's order, in which versions differing only in the build release
    /// are equal.
    pub fn pkg5_cmp(&self, other: &Self) -> Ordering {
        // Python list and None-first ordering, as in pkg5's Version.__lt__.
        self.release
            .cmp(&other.release)
            .then_with(|| self.branch.cmp(&other.branch))
            .then_with(|| self.timestamp.cmp(&other.timestamp))
    }

    /// Whether this version satisfies an `incorporate` dependency on
    /// `wanted`: pkg5's `is_successor(wanted, CONSTRAINT_AUTO)`. Every
    /// component `wanted` specifies must match: release and branch as
    /// leading subsequences, the timestamp exactly.
    pub fn matches_incorporation(&self, wanted: &Pkg5Version) -> bool {
        if !is_prefix(&wanted.release, &self.release) {
            return false;
        }
        match (&wanted.branch, &self.branch) {
            (None, _) => {}
            (Some(w), Some(s)) if is_prefix(w, s) => {}
            _ => return false,
        }
        match (&wanted.timestamp, &self.timestamp) {
            (None, _) => true,
            (Some(w), Some(s)) => w == s,
            (Some(_), None) => false,
        }
    }
}

fn is_prefix(prefix: &[u64], of: &[u64]) -> bool {
    prefix.len() <= of.len() && prefix.iter().zip(of).all(|(a, b)| a == b)
}

impl Ord for Pkg5Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pkg5_cmp(other)
            // Tie-break on the full text so distinct build releases never
            // compare equal in sorted containers.
            .then_with(|| self.text.cmp(&other.text))
    }
}

impl PartialOrd for Pkg5Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Pkg5Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Pkg5Version {
        Pkg5Version::parse(s).unwrap()
    }

    #[test]
    fn orders_like_pkg5() {
        assert!(v("1.0") < v("1.0.1"));
        assert!(v("1.2") < v("1.10"));
        assert!(v("1.0-1") > v("1.0"));
        assert!(v("1.0-2020.0.1.1") < v("1.0-2020.0.1.2"));
        assert!(v("1.0-1:20200101T000000Z") > v("1.0-1"));
        assert!(v("1.0-1:20200101T000000Z") < v("1.0-1:20200102T000000Z"));
        // The build release takes no part in the order.
        assert_eq!(
            v("1.0,5.11-1:20200101T000000Z").cmp(&v("1.0,5.12-1:20200101T000000Z")),
            Ordering::Less
        );
        assert!(v("2.0,5.11") > v("1.0,5.11-9:20990101T000000Z"));
    }

    #[test]
    fn require_accepts_successors() {
        let dep = v("1.2-1");
        assert!(v("1.2-1:20200101T000000Z").satisfies_minimum(&dep));
        assert!(v("1.3").satisfies_minimum(&dep));
        assert!(v("1.0,5.11").satisfies_minimum(&v("1.0,5.12")));
        assert!(!v("1.1-9").satisfies_minimum(&dep));
    }

    #[test]
    fn incorporate_matches_auto_constraint() {
        let dep = v("1.2-2020.0.1");
        assert!(v("1.2-2020.0.1.5:20200101T000000Z").matches_incorporation(&dep));
        assert!(v("1.2-2020.0.1").matches_incorporation(&dep));
        assert!(!v("1.2.1-2020.0.1").matches_incorporation(&v("1.3")));
        assert!(!v("1.2-2020.0.2").matches_incorporation(&dep));
        assert!(!v("1.2").matches_incorporation(&dep));
        // A longer release is a successor at the incorporated precision.
        assert!(v("1.2.7-2020.0.1").matches_incorporation(&v("1.2")));
        let exact = v("1.2-1:20200101T000000Z");
        assert!(v("1.2-1:20200101T000000Z").matches_incorporation(&exact));
        assert!(!v("1.2-1:20200102T000000Z").matches_incorporation(&exact));
    }

    #[test]
    fn reads_publication_time() {
        let t = v("1.0-1:20200421T195136Z").published().unwrap();
        assert_eq!(t.to_rfc3339(), "2020-04-21T19:51:36+00:00");
        assert!(v("1.0").published().is_none());
    }
}
