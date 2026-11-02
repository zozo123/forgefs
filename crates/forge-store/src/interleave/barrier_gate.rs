//! Pure model of `BarrierGate` / `FsBarrier::wait` in `blob.rs`.
//!
//! I4: a follower must not be acknowledged by a barrier whose leader had not
//! yet entered the kernel sync call — modeled as `entered_kernel` for each
//! generation before `completed` may advance to it.

use super::harness::{explore, ExploreOutcome, ScheduleDomain};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Phase {
    NeedTarget,
    Waiting,
    Leading { generation: u64 },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BarrierStep {
    ReadTarget,
    CheckComplete,
    WaitOnInFlight,
    BeginLead,
    FinishSyncOk,
    FinishSyncFail,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ThreadState {
    phase: Phase,
    target: Option<u64>,
    ack_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BarrierGateModel {
    completed: u64,
    in_flight: Option<u64>,
    entered_kernel: BTreeSet<u64>,
    finished_sync: BTreeSet<u64>,
    threads: Vec<ThreadState>,
}

impl BarrierGateModel {
    pub fn new(waiters: usize) -> Self {
        Self {
            completed: 0,
            in_flight: None,
            entered_kernel: BTreeSet::new(),
            finished_sync: BTreeSet::new(),
            threads: (0..waiters)
                .map(|_| ThreadState {
                    phase: Phase::NeedTarget,
                    target: None,
                    ack_generation: None,
                })
                .collect(),
        }
    }

    pub(crate) fn explore(waiters: usize, step_budget: usize) -> ExploreOutcome {
        explore(Self::new(waiters), step_budget)
    }
}

pub fn explore_barrier_gate(waiters: usize, step_budget: usize) -> ExploreOutcome {
    BarrierGateModel::explore(waiters, step_budget)
}

impl ScheduleDomain for BarrierGateModel {
    type ThreadId = usize;
    type StepLabel = BarrierStep;

    fn thread_ids(&self) -> Vec<Self::ThreadId> {
        (0..self.threads.len()).collect()
    }

    fn is_finished(&self, thread: Self::ThreadId) -> bool {
        matches!(self.threads[thread].phase, Phase::Done)
    }

    fn legal_steps(&self, thread: Self::ThreadId) -> Vec<Self::StepLabel> {
        let t = &self.threads[thread];
        match t.phase {
            Phase::NeedTarget => vec![BarrierStep::ReadTarget],
            Phase::Waiting => {
                let Some(target) = t.target else {
                    return Vec::new();
                };
                if self.completed >= target {
                    return vec![BarrierStep::CheckComplete];
                }
                if self.in_flight.is_some() {
                    return vec![BarrierStep::WaitOnInFlight];
                }
                vec![BarrierStep::BeginLead]
            }
            Phase::Leading { .. } => {
                vec![BarrierStep::FinishSyncOk, BarrierStep::FinishSyncFail]
            }
            Phase::Done => Vec::new(),
        }
    }

    fn step(&mut self, thread: Self::ThreadId, label: Self::StepLabel) {
        let t = &mut self.threads[thread];
        match (t.phase, label) {
            (Phase::NeedTarget, BarrierStep::ReadTarget) => {
                let target = self.in_flight.unwrap_or(self.completed) + 1;
                t.target = Some(target);
                t.phase = Phase::Waiting;
            }
            (Phase::Waiting, BarrierStep::CheckComplete) => {
                let target = t.target.expect("target");
                debug_assert!(self.completed >= target);
                t.ack_generation = Some(target);
                t.phase = Phase::Done;
            }
            (Phase::Waiting, BarrierStep::WaitOnInFlight) => {}
            (Phase::Waiting, BarrierStep::BeginLead) => {
                let generation = self.completed + 1;
                self.in_flight = Some(generation);
                self.entered_kernel.insert(generation);
                t.phase = Phase::Leading { generation };
            }
            (Phase::Leading { generation }, BarrierStep::FinishSyncOk) => {
                self.in_flight = None;
                self.completed = generation;
                self.finished_sync.insert(generation);
                t.phase = Phase::Waiting;
            }
            (Phase::Leading { generation }, BarrierStep::FinishSyncFail) => {
                self.in_flight = None;
                t.phase = Phase::Waiting;
                let _ = generation;
            }
            _ => panic!("illegal barrier step {label:?} in phase {:?}", t.phase),
        }
    }

    fn check_invariants(&self) -> Result<(), String> {
        if self.completed > 0 && !self.finished_sync.contains(&self.completed) {
            return Err(format!(
                "completed={} but generation never finished sync successfully",
                self.completed
            ));
        }
        for (i, t) in self.threads.iter().enumerate() {
            if let Some(ack) = t.ack_generation {
                if !self.finished_sync.contains(&ack) {
                    return Err(format!(
                        "thread {i} acked generation {ack} before it finished sync"
                    ));
                }
                if !self.entered_kernel.contains(&ack) {
                    return Err(format!(
                        "thread {i} acked generation {ack} whose leader never entered the kernel"
                    ));
                }
            }
        }
        if self.completed > 0 && !self.finished_sync.contains(&self.completed) {
            return Err(format!(
                "completed {} is not in finished_sync set",
                self.completed
            ));
        }
        Ok(())
    }
}

/// Every explored schedule respects barrier ordering (issue #23 / I4 barrier case).
pub fn assert_barrier_schedules(waiters: usize, step_budget: usize) {
    let outcome = BarrierGateModel::explore(waiters, step_budget);
    assert!(
        outcome.invariant_violation.is_none(),
        "barrier invariant failed: {:?}",
        outcome.invariant_violation
    );
    assert!(
        outcome.schedules_explored > 0,
        "barrier explorer found no schedules"
    );
}
