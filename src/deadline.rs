//! The check deadline: the budget as a hard limit on every run that mutation starts.
//!
//! A run that the deadline cuts short, or that cannot start before it, has no result and is
//! reported as not run. It is never a kill, a timeout, or a survivor.

use std::time::{Duration, Instant};

/// The instant by which a run must end, or no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Deadline(Option<Instant>);

/// The timeout for one process, and whether the deadline is what sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub timeout: Option<Duration>,
    pub at_deadline: bool,
}

impl Deadline {
    /// No deadline: runs keep only their own timeouts.
    pub const NONE: Deadline = Deadline(None);

    pub fn at(instant: Instant) -> Deadline {
        Deadline(Some(instant))
    }

    /// Time left before the deadline, or None when there is no deadline.
    pub fn remaining(&self) -> Option<Duration> {
        self.0.map(|d| d.saturating_duration_since(Instant::now()))
    }

    pub fn passed(&self) -> bool {
        self.remaining().is_some_and(|r| r.is_zero())
    }

    /// Whether work expected to take `expected` can finish before the deadline.
    pub fn fits(&self, expected: Duration) -> bool {
        self.remaining().is_none_or(|r| expected <= r)
    }

    /// The timeout for a process whose own limit is `own`: the smaller of `own` and the time
    /// left.
    pub fn limit(&self, own: Option<Duration>) -> Limit {
        match (own, self.remaining()) {
            (Some(own), Some(left)) if left < own => Limit {
                timeout: Some(left),
                at_deadline: true,
            },
            (None, Some(left)) => Limit {
                timeout: Some(left),
                at_deadline: true,
            },
            (own, _) => Limit {
                timeout: own,
                at_deadline: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_deadline_keeps_the_own_timeout() {
        let limit = Deadline::NONE.limit(Some(Duration::from_secs(5)));
        assert_eq!(limit.timeout, Some(Duration::from_secs(5)));
        assert!(!limit.at_deadline);
        assert!(Deadline::NONE.fits(Duration::MAX));
        assert!(!Deadline::NONE.passed());
    }

    #[test]
    fn the_nearer_of_the_deadline_and_the_own_timeout_wins() {
        let deadline = Deadline::at(Instant::now() + Duration::from_secs(60));
        let own = deadline.limit(Some(Duration::from_secs(5)));
        assert_eq!(own.timeout, Some(Duration::from_secs(5)));
        assert!(!own.at_deadline);
        let cut = deadline.limit(Some(Duration::from_secs(600)));
        assert!(cut.at_deadline);
        assert!(cut.timeout.unwrap() <= Duration::from_secs(60));
        assert!(deadline.limit(None).at_deadline);
        assert!(deadline.fits(Duration::from_secs(30)));
        assert!(!deadline.fits(Duration::from_secs(90)));
    }

    #[test]
    fn a_past_deadline_leaves_no_time() {
        let deadline = Deadline::at(Instant::now());
        assert!(deadline.passed());
        assert_eq!(deadline.remaining(), Some(Duration::ZERO));
        assert!(!deadline.fits(Duration::from_millis(1)));
        assert_eq!(deadline.limit(None).timeout, Some(Duration::ZERO));
    }
}
