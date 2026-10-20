use std::ops::{Add, Sub};
use std::time::Duration;

/// Monotonic time in microseconds, supplied by the host. Any epoch works as long
/// as it never goes backwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(pub u64);

impl Instant {
    pub const ZERO: Instant = Instant(0);

    pub fn from_micros(us: u64) -> Self {
        Instant(us)
    }

    pub fn from_millis(ms: u64) -> Self {
        Instant(ms.saturating_mul(1000))
    }

    pub fn as_micros(self) -> u64 {
        self.0
    }

    pub fn saturating_duration_since(self, earlier: Instant) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(d.as_micros() as u64))
    }
}

impl Sub for Instant {
    type Output = Duration;

    fn sub(self, earlier: Instant) -> Duration {
        self.saturating_duration_since(earlier)
    }
}
