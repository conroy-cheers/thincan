//! Async helpers for integrating ISO-TP with runtimes (tokio, embassy, ...).
//!
//! The goal of this module is to keep `can-iso-tp` runtime-agnostic: instead of depending on a
//! particular executor or timer API, [`AsyncRuntime`] models just the two operations ISO-TP needs:
//! sleeping and timing out a future.

use core::future::Future;
use core::time::Duration;

/// Timeout marker returned by [`AsyncRuntime::timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

/// Minimal runtime abstraction: sleeping and applying timeouts to futures.
///
/// Implementations may be thin wrappers over:
/// - `tokio::time::sleep` / `tokio::time::timeout`
/// - `embassy_time::Timer` / `embassy_time::with_timeout`
/// - custom embedded schedulers
pub trait AsyncRuntime {
    /// Error type returned by [`AsyncRuntime::timeout`].
    type TimeoutError;

    /// Future returned by [`AsyncRuntime::sleep`].
    type Sleep<'a>: Future<Output = ()> + 'a
    where
        Self: 'a;

    /// Sleep for a duration.
    fn sleep<'a>(&'a self, duration: Duration) -> Self::Sleep<'a>;

    /// Future returned by [`AsyncRuntime::timeout`].
    type Timeout<'a, F>: Future<Output = Result<F::Output, Self::TimeoutError>> + 'a
    where
        Self: 'a,
        F: Future + 'a;

    /// Run `future` but error if it doesn't complete within `duration`.
    ///
    /// `can-iso-tp` treats the error as a timeout signal and maps it to a [`crate::IsoTpError::Timeout`]
    /// variant appropriate to the phase being executed.
    fn timeout<'a, F>(&'a self, duration: Duration, future: F) -> Self::Timeout<'a, F>
    where
        F: Future + 'a;
}
