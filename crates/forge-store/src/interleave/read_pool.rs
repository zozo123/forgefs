//! Pure model of `ReadPool::acquire` in `meta.rs` (issue #308 / #23).
//!
//! Models `fetch_add` cursor, `try_lock` scan, lazy populate, and blocking fallback.

use super::harness::{explore, ExploreOutcome, ScheduleDomain};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PoolPhase {
    PickStart,
    Scanning { start: usize, offset: usize },
    Holding { slot: usize },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PoolStep {
    PickStart,
    TryLockScan,
    TryLockPopulate,
    BlockLockPopulate,
    ReleaseHold,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PoolThread {
    phase: PoolPhase,
    start: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReadPoolModel {
    width: usize,
    next: usize,
    populated: Vec<bool>,
    locked: Vec<Option<usize>>,
    threads: Vec<PoolThread>,
}

impl ReadPoolModel {
    pub fn new(width: usize, threads: usize) -> Self {
        assert!(width >= 1);
        Self {
            width,
            next: 0,
            populated: vec![false; width],
            locked: vec![None; width],
            threads: (0..threads)
                .map(|_| PoolThread {
                    phase: PoolPhase::PickStart,
                    start: 0,
                })
                .collect(),
        }
    }

    pub(crate) fn explore(width: usize, threads: usize, step_budget: usize) -> ExploreOutcome {
        explore(Self::new(width, threads), step_budget)
    }
}

pub fn explore_read_pool(width: usize, threads: usize, step_budget: usize) -> ExploreOutcome {
    ReadPoolModel::explore(width, threads, step_budget)
}

impl ScheduleDomain for ReadPoolModel {
    type ThreadId = usize;
    type StepLabel = PoolStep;

    fn thread_ids(&self) -> Vec<Self::ThreadId> {
        (0..self.threads.len()).collect()
    }

    fn is_finished(&self, thread: Self::ThreadId) -> bool {
        matches!(self.threads[thread].phase, PoolPhase::Done)
    }

    fn legal_steps(&self, thread: Self::ThreadId) -> Vec<Self::StepLabel> {
        match self.threads[thread].phase {
            PoolPhase::PickStart => vec![PoolStep::PickStart],
            PoolPhase::Scanning { offset, .. } => {
                if offset < self.width {
                    vec![PoolStep::TryLockScan]
                } else {
                    vec![PoolStep::TryLockPopulate, PoolStep::BlockLockPopulate]
                }
            }
            PoolPhase::Holding { .. } => vec![PoolStep::ReleaseHold],
            PoolPhase::Done => Vec::new(),
        }
    }

    fn step(&mut self, thread: Self::ThreadId, label: Self::StepLabel) {
        let t = &mut self.threads[thread];
        match (t.phase, label) {
            (PoolPhase::PickStart, PoolStep::PickStart) => {
                let start = self.next % self.width;
                self.next = self.next.wrapping_add(1);
                t.start = start;
                t.phase = PoolPhase::Scanning { start, offset: 0 };
            }
            (PoolPhase::Scanning { start, offset }, PoolStep::TryLockScan) => {
                let slot = (start + offset) % self.width;
                if self.locked[slot].is_none() && self.populated[slot] {
                    self.locked[slot] = Some(thread);
                    t.phase = PoolPhase::Holding { slot };
                    return;
                }
                t.phase = PoolPhase::Scanning {
                    start,
                    offset: offset + 1,
                };
            }
            (PoolPhase::Holding { slot }, PoolStep::ReleaseHold) => {
                self.locked[slot] = None;
                t.phase = PoolPhase::Done;
            }
            (PoolPhase::Scanning { start, offset: _ }, PoolStep::TryLockPopulate) => {
                let mut chosen = None;
                for off in 0..self.width {
                    let slot = (start + off) % self.width;
                    if self.locked[slot].is_none() {
                        chosen = Some(slot);
                        break;
                    }
                }
                let Some(slot) = chosen else {
                    return;
                };
                self.locked[slot] = Some(thread);
                if !self.populated[slot] {
                    self.populated[slot] = true;
                }
                t.phase = PoolPhase::Done;
                self.locked[slot] = None;
            }
            (PoolPhase::Scanning { start, .. }, PoolStep::BlockLockPopulate) => {
                let slot = start;
                if self.locked[slot].is_some() {
                    // blocked: no progress in this step label
                    return;
                }
                self.locked[slot] = Some(thread);
                if !self.populated[slot] {
                    self.populated[slot] = true;
                }
                t.phase = PoolPhase::Done;
                self.locked[slot] = None;
            }
            _ => panic!("illegal pool step {label:?} in phase {:?}", t.phase),
        }
    }

    fn check_invariants(&self) -> Result<(), String> {
        for (slot, holder) in self.locked.iter().enumerate() {
            if let Some(t) = holder {
                if !matches!(self.threads[*t].phase, PoolPhase::Holding { .. }) {
                    return Err(format!(
                        "slot {slot} locked by thread {t} not in Holding phase"
                    ));
                }
            }
        }
        for (i, t) in self.threads.iter().enumerate() {
            if let PoolPhase::Holding { slot } = t.phase {
                if self.locked[slot] != Some(i) {
                    return Err(format!("thread {i} holds slot {slot} without lock"));
                }
                if !self.populated[slot] {
                    return Err(format!(
                        "thread {i} acquired unpopulated slot {slot} on fast path"
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn assert_read_pool_schedules(width: usize, threads: usize, step_budget: usize) {
    let outcome = ReadPoolModel::explore(width, threads, step_budget);
    assert!(
        outcome.invariant_violation.is_none(),
        "read pool invariant failed: {:?}",
        outcome.invariant_violation
    );
    assert!(
        outcome.schedules_explored > 0,
        "read pool explorer found no schedules"
    );
}
