//! The ready-queue state machine that drives parallel builds.
//!
//! [`Scheduler`] tracks, over the components' dependency graph, which are ready
//! to build, which are in flight, and when the run is done or cancelled. It is
//! pure state — no threads, no builds — so a parallel driver can wrap it in a
//! mutex and a condition variable, and it can be tested by simulating a sequence
//! of claims and completions.
//!
//! Components are addressed by position in the build order (`0..total`), so a
//! single worker draining the queue reproduces the sequential order exactly: a
//! ready component with a lower position is always claimed first.

/// What a worker should do when it asks the scheduler for work.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Build the component at this order position; it is now in flight.
    Build(usize),
    /// Nothing is ready yet, but work remains: wait to be notified.
    Wait,
    /// No more work will come — every component is done, or the run was
    /// cancelled. The worker should exit.
    Stop,
}

/// Tracks readiness over the build graph: which components can build now, which
/// are running, and whether the run is finished or cancelled.
pub(crate) struct Scheduler {
    /// Remaining unbuilt producers for each component; it becomes ready at zero.
    in_degree: Vec<usize>,
    /// Each component's dependents, by order position.
    dependents: Vec<Vec<usize>>,
    /// Ready-but-unclaimed component positions.
    ready: Vec<usize>,
    /// Components currently being built.
    in_flight: usize,
    /// Set when a failure cancels the run, so no further work is handed out.
    cancelled: bool,
}

impl Scheduler {
    /// Creates a scheduler over a graph given as each component's in-set producer
    /// count (`in_degree`) and its dependents' positions, both indexed by order
    /// position. Components with no producers start ready.
    pub(crate) fn new(in_degree: Vec<usize>, dependents: Vec<Vec<usize>>) -> Scheduler {
        let ready = (0..in_degree.len())
            .filter(|&i| in_degree[i] == 0)
            .collect();
        Scheduler {
            in_degree,
            dependents,
            ready,
            in_flight: 0,
            cancelled: false,
        }
    }

    /// Hands out the lowest-positioned ready component, or reports that the
    /// worker should wait or stop. A claimed component is counted in flight until
    /// [`complete`](Self::complete).
    pub(crate) fn claim(&mut self) -> Claim {
        if self.cancelled {
            return Claim::Stop;
        }
        if let Some(pos) = self.take_ready() {
            self.in_flight += 1;
            return Claim::Build(pos);
        }
        // Nothing ready. If nothing is in flight either, no component can still be
        // released, so the run is done; otherwise an in-flight component may
        // release more when it finishes.
        if self.in_flight == 0 {
            Claim::Stop
        } else {
            Claim::Wait
        }
    }

    /// Records that the component at `pos` finished. Its dependents' producer
    /// counts drop — releasing any that reach zero — whether it succeeded or not,
    /// unless a failure cancels the run: with `cancel_on_fail` set, a failure
    /// stops the whole run; without it (keep-going), a dependent of a failed
    /// producer is still released and attempted, exactly as the sequential build
    /// reaches it in order (its build then fails for the missing dependency).
    pub(crate) fn complete(&mut self, pos: usize, success: bool, cancel_on_fail: bool) {
        self.in_flight -= 1;
        if !success && cancel_on_fail {
            self.cancelled = true;
            return;
        }
        let dependents = self.dependents[pos].clone();
        for dependent in dependents {
            self.in_degree[dependent] -= 1;
            if self.in_degree[dependent] == 0 {
                self.ready.push(dependent);
            }
        }
    }

    /// Stops the run: no further component is handed out, and every waiting
    /// worker is told to stop once it is woken.
    ///
    /// This is the run-cancelled path, distinct from the failure path
    /// [`complete`](Self::complete) takes: it is not tied to any component's
    /// outcome, and it holds whether or not the run keeps going past failures —
    /// a cancel stops the run, and `--keep-going` does not override it.
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Removes and returns the lowest-positioned ready component, so a single
    /// worker reproduces the sequential build order.
    fn take_ready(&mut self) -> Option<usize> {
        let (index, _) = self.ready.iter().enumerate().min_by_key(|&(_, &pos)| pos)?;
        Some(self.ready.swap_remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drains the scheduler with a single worker, completing each claim
    /// successfully, and returns the order components were built in.
    fn drain_sequentially(mut scheduler: Scheduler) -> Vec<usize> {
        let mut built = Vec::new();
        loop {
            match scheduler.claim() {
                Claim::Build(pos) => {
                    built.push(pos);
                    scheduler.complete(pos, true, true);
                }
                Claim::Stop => break,
                Claim::Wait => unreachable!("a single worker never waits on itself"),
            }
        }
        built
    }

    #[test]
    fn a_single_worker_reproduces_the_order() {
        // A chain 0 -> 1 -> 2: each releases the next.
        let scheduler = Scheduler::new(vec![0, 1, 1], vec![vec![1], vec![2], vec![]]);
        assert_eq!(drain_sequentially(scheduler), [0, 1, 2]);
    }

    #[test]
    fn independent_components_are_built_in_position_order() {
        // No edges: all ready at once, claimed lowest-position first.
        let scheduler = Scheduler::new(vec![0, 0, 0], vec![vec![], vec![], vec![]]);
        assert_eq!(drain_sequentially(scheduler), [0, 1, 2]);
    }

    #[test]
    fn a_producer_releases_its_dependent_only_once_built() {
        let mut scheduler = Scheduler::new(vec![0, 1], vec![vec![1], vec![]]);
        // Only the producer is ready; the consumer waits behind it.
        assert_eq!(scheduler.claim(), Claim::Build(0));
        assert_eq!(scheduler.claim(), Claim::Wait);
        // Completing the producer releases the consumer.
        scheduler.complete(0, true, true);
        assert_eq!(scheduler.claim(), Claim::Build(1));
        scheduler.complete(1, true, true);
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn two_ready_components_are_handed_to_two_workers() {
        let mut scheduler = Scheduler::new(vec![0, 0], vec![vec![], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        assert_eq!(scheduler.claim(), Claim::Build(1));
        // Both in flight, nothing ready: a third worker waits until one finishes,
        // in case completing it releases more.
        assert_eq!(scheduler.claim(), Claim::Wait);
        // Once both finish and nothing new is ready, further claims stop.
        scheduler.complete(0, true, true);
        scheduler.complete(1, true, true);
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn a_failure_cancels_the_run_when_not_keeping_going() {
        // Two independent components; the first fails with cancel_on_fail.
        let mut scheduler = Scheduler::new(vec![0, 0], vec![vec![], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        scheduler.complete(0, false, true);
        // The second is ready, but the run is cancelled, so no more is handed out.
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn a_failure_keeps_going_when_asked() {
        let mut scheduler = Scheduler::new(vec![0, 0], vec![vec![], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        // Fail without cancelling: the rest still build.
        scheduler.complete(0, false, false);
        assert_eq!(scheduler.claim(), Claim::Build(1));
        scheduler.complete(1, true, false);
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn keep_going_still_releases_a_failed_producers_dependent() {
        // 0 -> 1, keep going: 0 fails, but 1 is still released and attempted,
        // exactly as the sequential build reaches it in order (its own build then
        // fails for the missing dependency). The run terminates cleanly.
        let mut scheduler = Scheduler::new(vec![0, 1], vec![vec![1], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        scheduler.complete(0, false, false);
        assert_eq!(scheduler.claim(), Claim::Build(1));
        scheduler.complete(1, false, false);
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn cancelling_the_run_stops_it_even_when_keeping_going() {
        // Two independent components, keep-going: cancelling hands out no
        // further work, which the failure path would not do here.
        let mut scheduler = Scheduler::new(vec![0, 0], vec![vec![], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        scheduler.complete(0, true, false);
        scheduler.cancel();
        assert_eq!(scheduler.claim(), Claim::Stop);
    }

    #[test]
    fn cancelling_leaves_a_failed_producers_dependent_unbuilt() {
        // 0 -> 1, not keeping going: 0 fails and cancels, so 1 is never handed
        // out and the run stops.
        let mut scheduler = Scheduler::new(vec![0, 1], vec![vec![1], vec![]]);
        assert_eq!(scheduler.claim(), Claim::Build(0));
        scheduler.complete(0, false, true);
        assert_eq!(scheduler.claim(), Claim::Stop);
    }
}
