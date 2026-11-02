//! Small deterministic schedule explorer for pure concurrency models.
//!
//! Issue #23: explore thread interleavings without the Loom crate (which fights
//! rusqlite/parking_lot in CI). Each domain names legal one-step transitions;
//! BFS walks schedules until every thread is finished or a step budget trips.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// One thread may take one labeled step; `Finished` means that thread's role ended.
pub(crate) trait ScheduleDomain {
    type ThreadId: Copy + Eq + Hash + std::fmt::Debug;
    type StepLabel: Copy + Eq + Hash + std::fmt::Debug;

    fn thread_ids(&self) -> Vec<Self::ThreadId>;
    fn is_finished(&self, thread: Self::ThreadId) -> bool;
    fn legal_steps(&self, thread: Self::ThreadId) -> Vec<Self::StepLabel>;
    fn step(&mut self, thread: Self::ThreadId, label: Self::StepLabel);
    fn check_invariants(&self) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreOutcome {
    pub schedules_explored: usize,
    pub max_depth_reached: usize,
    pub invariant_violation: Option<String>,
    pub step_budget_hit: bool,
}

/// Breadth-first exploration of all interleavings up to `step_budget` total steps.
pub(crate) fn explore<D: ScheduleDomain + Clone + Eq + Hash>(
    initial: D,
    step_budget: usize,
) -> ExploreOutcome {
    let threads = initial.thread_ids();
    let mut queue: VecDeque<(D, usize, usize)> = VecDeque::new();
    let mut seen: HashSet<D> = HashSet::new();
    queue.push_back((initial, 0, 0));
    let mut schedules = 0usize;
    let mut max_depth = 0usize;
    let mut invariant_violation = None;

    while let Some((state, depth, steps)) = queue.pop_front() {
        if !seen.insert(state.clone()) {
            continue;
        }
        schedules += 1;
        max_depth = max_depth.max(depth);

        if let Err(msg) = state.check_invariants() {
            invariant_violation = Some(msg);
            break;
        }

        if threads.iter().all(|&t| state.is_finished(t)) {
            continue;
        }

        if steps >= step_budget {
            continue;
        }

        for &thread in &threads {
            if state.is_finished(thread) {
                continue;
            }
            for label in state.legal_steps(thread) {
                let mut next = state.clone();
                next.step(thread, label);
                queue.push_back((next, depth + 1, steps + 1));
            }
        }
    }

    let step_budget_hit = invariant_violation.is_none()
        && queue.is_empty()
        && seen
            .into_iter()
            .any(|s| !threads.iter().all(|&t| s.is_finished(t)));

    ExploreOutcome {
        schedules_explored: schedules,
        max_depth_reached: max_depth,
        invariant_violation,
        step_budget_hit,
    }
}
