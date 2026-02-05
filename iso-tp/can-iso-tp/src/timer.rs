//! Clock abstraction to support `std` and `no_std` environments.

use core::time::Duration;

/// Abstraction over a monotonic clock.
///
/// ISO-TP needs a monotonically increasing time source to implement deadlines (N_As, N_Ar, …) and
/// pacing (STmin).
pub trait Clock {
    /// Instant type produced by the clock.
    type Instant: Copy + PartialOrd;

    /// Current instant.
    fn now(&self) -> Self::Instant;
    /// Elapsed duration since an instant.
    fn elapsed(&self, earlier: Self::Instant) -> Duration;
    /// Add a duration to an instant (saturating if needed).
    fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant;
}

#[cfg(feature = "std")]
/// Standard library clock wrapper.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdClock;

#[cfg(feature = "std")]
impl Clock for StdClock {
    type Instant = std::time::Instant;

    /// Return `Instant::now()`.
    fn now(&self) -> Self::Instant {
        std::time::Instant::now()
    }

    /// Use `Instant::elapsed()`.
    fn elapsed(&self, earlier: Self::Instant) -> Duration {
        earlier.elapsed()
    }

    /// Add with `checked_add`, saturating on overflow.
    fn add(&self, instant: Self::Instant, dur: Duration) -> Self::Instant {
        instant.checked_add(dur).unwrap_or(instant)
    }
}
