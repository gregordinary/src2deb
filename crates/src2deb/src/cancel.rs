//! The signal that stops a run in progress.
//!
//! A build run is long and mostly uninterruptible from the outside: a
//! bootstrap fetching several hundred packages, then a compile of many
//! minutes. [`Cancel`] is how a caller asks the run to stop at the next point
//! where stopping is clean, rather than having the process killed mid-flight.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A run's cancellation signal: a flag set from outside the run and consulted
/// inside it wherever stopping leaves a coherent state behind.
///
/// The library installs no signal handlers and keeps no process-global state.
/// A caller that wants Ctrl-C to stop a run registers its own handler against
/// the [`flag`](Cancel::flag) this hands out and passes the `Cancel` in on
/// [`RunOptions`](crate::RunOptions); a caller that wants no cancellation
/// passes the default, which is never set.
///
/// Cloning shares one flag, and the type is [`Send`] and [`Sync`], so every
/// worker thread of a parallel run consults the same signal.
///
/// # Example
///
/// ```
/// use src2deb::Cancel;
///
/// let cancel = Cancel::new();
/// assert!(!cancel.requested());
///
/// // A clone shares the flag: setting either is visible through both.
/// let from_a_handler = cancel.clone();
/// from_a_handler.request();
/// assert!(cancel.requested());
/// ```
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    requested: Arc<AtomicBool>,
}

impl Cancel {
    /// Creates a signal that has not been requested.
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// Requests cancellation. Idempotent: a run stops once, however many times
    /// it is asked.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// The underlying flag, for a caller that sets it from a signal handler.
    ///
    /// A handler may touch only async-signal-safe state, which an
    /// [`AtomicBool`] is. Handing the flag out lets a caller register it
    /// directly with whatever installs its handlers, so nothing of src2deb's
    /// runs in signal context.
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_signal_is_unset_and_requesting_is_idempotent() {
        let cancel = Cancel::new();
        assert!(!cancel.requested());
        cancel.request();
        assert!(cancel.requested());
        cancel.request();
        assert!(cancel.requested());
    }

    #[test]
    fn clones_and_the_handed_out_flag_share_one_signal() {
        let cancel = Cancel::new();
        let clone = cancel.clone();
        let flag = cancel.flag();
        // Setting the raw flag, as a signal handler would, is visible through
        // every clone.
        flag.store(true, Ordering::Relaxed);
        assert!(cancel.requested());
        assert!(clone.requested());
    }
}
