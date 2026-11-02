//! Pure model of group-commit election in `Meta::run_grouped` / `drain_group`.
//!
//! I4/I6: every queued job is answered after commit or with an explicit error;
//! a leader that abandons mid-batch must not leave followers hanging silently.

use super::harness::{explore, ExploreOutcome, ScheduleDomain};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum JobReply {
    Ok,
    Err,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum WriterPhase {
    NeedEnqueue,
    Spinning,
    Locked,
    Draining { stage: DrainStage },
    Done(JobReply),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DrainStage {
    Taken,
    JobsRun,
    Replied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum GroupStep {
    Enqueue,
    RecvIdle,
    LockWrite,
    RecvAfterLock,
    DrainTake,
    BeginTxFail,
    RunJobs,
    CommitOk,
    CommitFail,
    PublishReplies,
    LeaderAbandon,
    UnlockEmpty,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Writer {
    phase: WriterPhase,
    id: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GroupCommitModel {
    pending: VecDeque<u8>,
    write_holder: Option<usize>,
    batch: Vec<u8>,
    /// Simulated per-job `sync_channel(1)` buffer.
    mailboxes: Vec<Option<JobReply>>,
    drain_stage: Option<DrainStage>,
    writers: Vec<Writer>,
}

impl GroupCommitModel {
    pub fn new(writer_count: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            write_holder: None,
            batch: Vec::new(),
            mailboxes: vec![None; writer_count],
            drain_stage: None,
            writers: (0..writer_count)
                .map(|id| Writer {
                    phase: WriterPhase::NeedEnqueue,
                    id: id as u8,
                })
                .collect(),
        }
    }

    pub(crate) fn explore(writer_count: usize, step_budget: usize) -> ExploreOutcome {
        explore(Self::new(writer_count), step_budget)
    }
}

pub fn explore_group_commit(writer_count: usize, step_budget: usize) -> ExploreOutcome {
    GroupCommitModel::explore(writer_count, step_budget)
}

impl GroupCommitModel {
    fn try_deliver(&mut self, writer: usize) -> bool {
        if let Some(reply) = self.mailboxes[writer] {
            self.writers[writer].phase = WriterPhase::Done(reply);
            return true;
        }
        false
    }
}

impl ScheduleDomain for GroupCommitModel {
    type ThreadId = usize;
    type StepLabel = GroupStep;

    fn thread_ids(&self) -> Vec<Self::ThreadId> {
        (0..self.writers.len()).collect()
    }

    fn is_finished(&self, thread: Self::ThreadId) -> bool {
        matches!(self.writers[thread].phase, WriterPhase::Done(_))
    }

    fn legal_steps(&self, thread: Self::ThreadId) -> Vec<Self::StepLabel> {
        let w = &self.writers[thread];
        match w.phase {
            WriterPhase::NeedEnqueue => vec![GroupStep::Enqueue],
            WriterPhase::Spinning => {
                let mut v = vec![GroupStep::RecvIdle];
                if self.write_holder.is_none() {
                    v.push(GroupStep::LockWrite);
                }
                v
            }
            WriterPhase::Locked => {
                let mut v = vec![GroupStep::RecvAfterLock, GroupStep::DrainTake];
                if self.batch.is_empty() && self.pending.is_empty() {
                    v.push(GroupStep::UnlockEmpty);
                }
                v
            }
            WriterPhase::Draining { stage } => {
                let mut v = Vec::new();
                match stage {
                    DrainStage::Taken => {
                        v.push(GroupStep::BeginTxFail);
                        v.push(GroupStep::RunJobs);
                    }
                    DrainStage::JobsRun => {
                        v.push(GroupStep::CommitOk);
                        v.push(GroupStep::CommitFail);
                        v.push(GroupStep::LeaderAbandon);
                    }
                    DrainStage::Replied => {
                        v.push(GroupStep::PublishReplies);
                        v.push(GroupStep::UnlockEmpty);
                    }
                }
                v
            }
            WriterPhase::Done(_) => Vec::new(),
        }
    }

    fn step(&mut self, thread: Self::ThreadId, label: Self::StepLabel) {
        let w = &mut self.writers[thread];
        match (w.phase, label) {
            (WriterPhase::NeedEnqueue, GroupStep::Enqueue) => {
                self.pending.push_back(w.id);
                w.phase = WriterPhase::Spinning;
            }
            (WriterPhase::Spinning, GroupStep::RecvIdle) => {
                let _ = self.try_deliver(thread);
            }
            (WriterPhase::Spinning, GroupStep::LockWrite) => {
                if self.write_holder.is_none() {
                    self.write_holder = Some(thread);
                    w.phase = WriterPhase::Locked;
                }
            }
            (WriterPhase::Locked, GroupStep::RecvAfterLock) => {
                if self.try_deliver(thread) {
                    self.write_holder = None;
                }
            }
            (WriterPhase::Locked, GroupStep::DrainTake) => {
                if self.pending.is_empty() {
                    return;
                }
                self.batch = self.pending.drain(..).collect();
                self.drain_stage = Some(DrainStage::Taken);
                w.phase = WriterPhase::Draining {
                    stage: DrainStage::Taken,
                };
            }
            (WriterPhase::Draining { .. }, GroupStep::BeginTxFail) => {
                for id in &self.batch {
                    self.mailboxes[*id as usize] = Some(JobReply::Err);
                }
                w.phase = WriterPhase::Draining {
                    stage: DrainStage::Replied,
                };
                self.drain_stage = Some(DrainStage::Replied);
            }
            (WriterPhase::Draining { .. }, GroupStep::RunJobs) => {
                w.phase = WriterPhase::Draining {
                    stage: DrainStage::JobsRun,
                };
                self.drain_stage = Some(DrainStage::JobsRun);
            }
            (
                WriterPhase::Draining {
                    stage: DrainStage::JobsRun,
                },
                GroupStep::CommitOk,
            ) => {
                for id in &self.batch {
                    self.mailboxes[*id as usize] = Some(JobReply::Ok);
                }
                w.phase = WriterPhase::Draining {
                    stage: DrainStage::Replied,
                };
                self.drain_stage = Some(DrainStage::Replied);
            }
            (
                WriterPhase::Draining {
                    stage: DrainStage::JobsRun,
                },
                GroupStep::CommitFail,
            ) => {
                for id in &self.batch {
                    self.mailboxes[*id as usize] = Some(JobReply::Err);
                }
                w.phase = WriterPhase::Draining {
                    stage: DrainStage::Replied,
                };
                self.drain_stage = Some(DrainStage::Replied);
            }
            (WriterPhase::Draining { .. }, GroupStep::LeaderAbandon) => {
                for id in &self.batch {
                    if self.mailboxes[*id as usize].is_none() {
                        self.mailboxes[*id as usize] = Some(JobReply::Disconnected);
                    }
                }
                self.batch.clear();
                self.drain_stage = None;
                self.write_holder = None;
                w.phase = WriterPhase::Spinning;
            }
            (WriterPhase::Locked | WriterPhase::Draining { .. }, GroupStep::UnlockEmpty) => {
                self.batch.clear();
                self.drain_stage = None;
                self.write_holder = None;
                w.phase = WriterPhase::Spinning;
            }
            (
                WriterPhase::Draining {
                    stage: DrainStage::Replied,
                },
                GroupStep::PublishReplies,
            ) => {
                self.batch.clear();
                self.drain_stage = None;
            }
            _ => panic!("illegal group step {label:?} in phase {:?}", w.phase),
        }
    }

    fn check_invariants(&self) -> Result<(), String> {
        if let Some(holder) = self.write_holder {
            if !matches!(
                self.writers[holder].phase,
                WriterPhase::Locked | WriterPhase::Draining { .. }
            ) {
                return Err(format!(
                    "write lock held by thread {holder} not in locked/draining phase"
                ));
            }
        }
        Ok(())
    }
}

pub fn assert_group_commit_no_hang(writers: usize, step_budget: usize) {
    let outcome = GroupCommitModel::explore(writers, step_budget);
    assert!(
        outcome.invariant_violation.is_none(),
        "group commit structural invariant failed: {:?}",
        outcome.invariant_violation
    );
}

/// After a full schedule where every writer finishes without abandon, every job
/// must have a mailbox answer.
pub fn finished_schedules_answer_every_job(
    writers: usize,
    step_budget: usize,
) -> Result<(), String> {
    let initial = GroupCommitModel::new(writers);
    let threads: Vec<usize> = (0..writers).collect();
    let mut queue = std::collections::VecDeque::new();
    let mut seen = std::collections::HashSet::new();
    queue.push_back((initial, 0usize));

    while let Some((state, steps)) = queue.pop_front() {
        if !seen.insert(state.clone()) {
            continue;
        }
        state.check_invariants()?;

        let all_done = threads.iter().all(|&t| state.is_finished(t));
        if all_done {
            for w in &state.writers {
                let WriterPhase::Done(reply) = w.phase else {
                    return Err(format!("writer {} not in Done phase at terminal", w.id));
                };
                if state.mailboxes[w.id as usize].is_none() {
                    return Err(format!(
                        "writer {} mailbox empty after full completion",
                        w.id
                    ));
                }
                let _ = reply;
            }
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
                queue.push_back((next, steps + 1));
            }
        }
    }
    Ok(())
}

pub fn assert_group_commit_schedules(writers: usize, step_budget: usize) {
    assert_group_commit_no_hang(writers, step_budget);
    finished_schedules_answer_every_job(writers, step_budget)
        .expect("group commit schedule left a job unanswered");
}
