//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! The retention clock: UTC wall-clock time for eligibility, access records
//! and audit records. No command line option or API parameter moves it.
//! Only unit tests and builds with the test-only `test-clock` feature (which
//! libips enables for its own integration tests, never for the binaries)
//! can pin it with [`pin`].

use chrono::{DateTime, Utc};

#[cfg(any(test, feature = "test-clock"))]
static PINNED: std::sync::Mutex<Option<DateTime<Utc>>> = std::sync::Mutex::new(None);

/// The current time.
pub fn now() -> DateTime<Utc> {
    #[cfg(any(test, feature = "test-clock"))]
    if let Some(t) = *PINNED.lock().unwrap_or_else(|e| e.into_inner()) {
        return t;
    }
    Utc::now()
}

/// Pin the clock of this process to `t` (`None`: wall clock again). Tests
/// run one per process under nextest, so the pin is process-wide.
#[cfg(any(test, feature = "test-clock"))]
pub fn pin(t: Option<DateTime<Utc>>) {
    *PINNED.lock().unwrap_or_else(|e| e.into_inner()) = t;
}
