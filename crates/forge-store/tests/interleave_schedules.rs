//! Deterministic interleaving schedules for forge-store concurrency (#23).
//!
//! These tests explore small-state BFS schedules over pure models extracted
//! from the production primitives in `blob.rs` and `meta.rs`. They deliberately
//! avoid the Loom crate (rusqlite/parking_lot make Loom CI-hostile) while still
//! covering the memory-ordering blind spot the model-based harness cannot see.

use forge_store::interleave::{
    assert_barrier_schedules, assert_group_commit_schedules, assert_read_pool_schedules,
    explore_barrier_gate, explore_group_commit, explore_read_pool,
    finished_schedules_answer_every_job,
};

/// I4 object-plane barrier: `completed` advances one generation at a time and no
/// waiter is acknowledged before its leader entered the kernel sync.
#[test]
fn barrier_gate_schedules_never_ack_before_leader_enters() {
    assert_barrier_schedules(2, 48);
    assert_barrier_schedules(3, 64);
    let outcome = explore_barrier_gate(3, 64);
    assert!(
        outcome.invariant_violation.is_none(),
        "{:?}",
        outcome.invariant_violation
    );
    assert!(outcome.max_depth_reached > 4, "expected non-trivial depth");
}

/// Read pool: try_lock scan, lazy populate, and blocking fallback under a cursor.
#[test]
fn read_pool_acquire_schedules_respect_slot_locks() {
    assert_read_pool_schedules(2, 2, 40);
    assert_read_pool_schedules(3, 3, 56);
    let outcome = explore_read_pool(2, 3, 48);
    assert_eq!(outcome.invariant_violation, None);
}

/// I4/I6 catalog group commit: completed batches answer every member.
#[test]
fn group_commit_schedules_answer_queued_jobs() {
    assert_group_commit_schedules(2, 80);
    assert_group_commit_schedules(3, 96);
    let outcome = explore_group_commit(3, 96);
    assert!(
        outcome.schedules_explored > 20,
        "group commit schedule space too small"
    );
    finished_schedules_answer_every_job(3, 96).expect("happy-path schedules must answer jobs");
}
