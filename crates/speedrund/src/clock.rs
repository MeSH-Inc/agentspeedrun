//! How far a client clock may disagree with the daemon.
//!
//! Both rules that depend on the allowance live here, so neither the HTTP
//! layer nor the store has to know it: a future time is clamped, and an event
//! dated further behind the run's newest event than the allowance is refused.

use chrono::{DateTime, Duration, Utc};

/// A client clock may be this far ahead of, or behind, the daemon.
const SKEW_ALLOWANCE: Duration = Duration::minutes(5);

/// The daemon's clock tolerance, hidden behind these operations.
pub struct ClockWindow {
    allowance: Duration,
}

impl ClockWindow {
    /// The daemon's policy: a few minutes of skew in either direction.
    pub const DAEMON: Self = Self {
        allowance: SKEW_ALLOWANCE,
    };

    /// Clamp a future-dated client time to the allowed skew; `None` means now.
    pub fn clamp(&self, at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
        match at {
            Some(t) => t.min(now + self.allowance),
            None => now,
        }
    }

    /// Why an event's time is outside the run's window, if it is.
    ///
    /// `start` is when the run began and `last` its newest event by seq. A
    /// spool replay (`from_spool`) was dated while the daemon was unreachable,
    /// so it is exempt from the last-event bound, but never from the start of
    /// its run.
    pub fn admit(
        &self,
        at: Option<DateTime<Utc>>,
        start: DateTime<Utc>,
        last: DateTime<Utc>,
        now: DateTime<Utc>,
        from_spool: bool,
    ) -> Result<(), String> {
        let at = self.clamp(at, now);
        if from_spool && at < start {
            return Err("occurred_at is before the run started".into());
        }
        if !from_spool && at < last - self.allowance {
            return Err(format!(
                "occurred_at is more than {} minutes before the run's last event",
                self.allowance.num_minutes()
            ));
        }
        Ok(())
    }
}
