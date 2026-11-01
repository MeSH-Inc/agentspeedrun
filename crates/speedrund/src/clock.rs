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

    /// The time to record an event at, or why it is too far behind `last`.
    ///
    /// `last` is the run's newest event by seq, so a replay is judged against
    /// the log as it stands rather than against wall-clock now.
    pub fn admit(
        &self,
        at: Option<DateTime<Utc>>,
        last: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, String> {
        let at = self.clamp(at, now);
        match last {
            Some(last) if at < last - self.allowance => Err(format!(
                "occurred_at is more than {} minutes before the run's last event",
                self.allowance.num_minutes()
            )),
            _ => Ok(at),
        }
    }
}
