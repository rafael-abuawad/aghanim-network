//! Hybrid logical clock.
//!
//! An [`Hlc`] packs 48 bits of wall-clock milliseconds and a 16-bit counter
//! into a `u64`, so timestamps compare as plain integers. Ties between
//! different writers are broken by the author's public key (see `doc.rs`).

use std::fmt;

#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc(pub u64);

impl Hlc {
    pub const ZERO: Hlc = Hlc(0);

    pub fn new(ms: u64, counter: u16) -> Self {
        Hlc((ms & 0xffff_ffff_ffff) << 16 | counter as u64)
    }

    pub fn ms(self) -> u64 {
        self.0 >> 16
    }

    pub fn counter(self) -> u16 {
        self.0 as u16
    }

    /// The smallest timestamp after this one. A counter overflow carries into
    /// the millisecond field, which keeps the clock monotonic.
    pub fn next(self) -> Self {
        Hlc(self.0 + 1)
    }
}

impl fmt::Debug for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hlc({}.{})", self.ms(), self.counter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftError {
    pub remote_ms: u64,
    pub local_ms: u64,
}

impl fmt::Display for DriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "timestamp {}ms is too far ahead of local clock {}ms",
            self.remote_ms, self.local_ms
        )
    }
}

/// Default limit on how far ahead of our wall clock a remote timestamp may be.
pub const DEFAULT_MAX_DRIFT_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Clock {
    last: Hlc,
    pub max_drift_ms: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_DRIFT_MS)
    }
}

impl Clock {
    pub fn new(max_drift_ms: u64) -> Self {
        Self {
            last: Hlc::ZERO,
            max_drift_ms,
        }
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    /// A fresh timestamp, strictly greater than every timestamp seen so far.
    pub fn now(&mut self, wall_ms: u64) -> Hlc {
        self.last = Hlc::new(wall_ms, 0).max(self.last.next());
        self.last
    }

    /// Checks a remote timestamp against the drift limit.
    pub fn check(&self, remote: Hlc, wall_ms: u64) -> Result<(), DriftError> {
        if remote.ms() > wall_ms.saturating_add(self.max_drift_ms) {
            return Err(DriftError {
                remote_ms: remote.ms(),
                local_ms: wall_ms,
            });
        }
        Ok(())
    }

    /// Advances the clock past an accepted remote timestamp.
    pub fn observe(&mut self, remote: Hlc) {
        self.last = self.last.max(remote);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_even_if_wall_clock_goes_back() {
        let mut c = Clock::default();
        let a = c.now(1000);
        let b = c.now(900);
        let d = c.now(1000);
        assert!(a < b && b < d);
        assert_eq!(a.ms(), 1000);
    }

    #[test]
    fn observe_moves_forward() {
        let mut c = Clock::default();
        c.observe(Hlc::new(5000, 3));
        assert!(c.now(10) > Hlc::new(5000, 3));
    }

    #[test]
    fn rejects_far_future() {
        let c = Clock::new(1000);
        assert!(c.check(Hlc::new(2001, 0), 1000).is_err());
        assert!(c.check(Hlc::new(2000, 0), 1000).is_ok());
    }
}
