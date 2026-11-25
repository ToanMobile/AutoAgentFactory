//! The wall clock behind the [`Clock`] port. It sits at the bottom of the
//! module graph so read-only modules (telemetry, watch) need not depend on
//! the factory to tell the time.

use std::time::{SystemTime, UNIX_EPOCH};

use aaf_domain::ports::Clock;

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        // A clock before 1970 is a broken machine; 0 keeps time monotone
        // enough for the stores instead of panicking.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}
