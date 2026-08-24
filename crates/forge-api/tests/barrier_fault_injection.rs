#![cfg(all(
    debug_assertions,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]

use forge_api::Forge;
use forge_core::{hash_bytes, Blob, Tree, TreeEntry};
use forge_store::{barrier_fault, sanitize_agent, DurabilityBarrier, Meta};
use forge_types::{CasResult, EntryKind, RefRow};
use std::collections::HashSet;
use std::path::Path;
use tempfile::tempdir;

fn refs(forge: &Forge) -> Vec<RefRow> {
    let root = forge.root_cap().unwrap();
    forge.refs(&root).unwrap()
}

fn assert_reopens_clean(dir: &Path, expected_refs: &[RefRow]) {
    let reopened = Forge::open(dir).unwrap();
    let root = reopened.root_cap().unwrap();
    assert_eq!(reopened.refs(&root).unwrap(), expected_refs);
    let report = reopened.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
}

fn payload_whose_tree_needs_a_new_shard(forge: &Forge) -> Vec<u8> {
    let objects = forge.root().join("objects");
    let existing = std::fs::read_dir(objects)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<HashSet<_>>();
    for attempt in 0..10_000u64 {
        let payload = format!("deterministic-barrier-{attempt}").into_bytes();
        let blob = hash_bytes(
            &Blob {
                data: payload.clone(),
            }
            .encode(),
        );
        let tree = Tree::new(vec![TreeEntry {
            name: "barrier.txt".into(),
            kind: EntryKind::Blob,
            id: blob,
            exec: false,
        }])
        .unwrap();
        let tree = hash_bytes(&tree.encode().unwrap());
        let (blob_a, _) = blob.shard_dirs();
        let (tree_a, _) = tree.shard_dirs();
        if tree_a != blob_a && !existing.contains(&tree_a) {
            return payload;
        }
    }
    panic!("could not select a deterministic unused tree shard");
}

#[test]
fn failed_object_barrier_never_acknowledges_a_ref() {
    // These are the three required transitions for a newly published object:
    // durable ancestors, durable bytes, then a durable final directory entry.
    for point in [
        DurabilityBarrier::ObjectPathDirectory,
        DurabilityBarrier::ObjectFile,
        DurabilityBarrier::ObjectPublicationDirectory,
    ] {
        let dir = tempdir().unwrap();
        let forge = Forge::init(dir.path()).unwrap();
        let root = forge.root_cap().unwrap();
        let ns = forge.session_open(&root, "main").unwrap();
        let payload = payload_whose_tree_needs_a_new_shard(&forge);
        forge
            .write(&root, &ns, "/barrier.txt", &payload, false)
            .unwrap();
        let before = refs(&forge);

        let fault = barrier_fault::fail_at(point, 1);
        let result = forge.checkin(&root, &ns, "/", "must fail before CAS");
        assert!(fault.fired(), "barrier was not reached: {point:?}");
        assert!(result.is_err(), "injected {point:?} was acknowledged");
        assert_eq!(refs(&forge), before, "{point:?} advanced a durable ref");
        drop(fault);

        drop(forge);
        assert_reopens_clean(dir.path(), &before);
    }
}

#[test]
fn interrupted_completed_object_transitions_never_acknowledge_a_ref() {
    // These failpoints run after the named operation succeeds, modeling loss
    // of control between durable state transitions rather than an I/O error
    // returned by the transition itself.
    for point in [
        DurabilityBarrier::ObjectFileAfter,
        DurabilityBarrier::ObjectLinkAfter,
        DurabilityBarrier::ObjectPublicationDirectoryAfter,
    ] {
        let dir = tempdir().unwrap();
        let forge = Forge::init(dir.path()).unwrap();
        let root = forge.root_cap().unwrap();
        let ns = forge.session_open(&root, "main").unwrap();
        let payload = payload_whose_tree_needs_a_new_shard(&forge);
        forge
            .write(&root, &ns, "/barrier.txt", &payload, false)
            .unwrap();
        let before = refs(&forge);

        let fault = barrier_fault::fail_at(point, 1);
        let result = forge.checkin(&root, &ns, "/", "interrupt after transition");
        assert!(fault.fired(), "transition was not reached: {point:?}");
        assert!(result.is_err(), "interrupted {point:?} was acknowledged");
        assert_eq!(refs(&forge), before, "{point:?} advanced a durable ref");
        drop(fault);

        drop(forge);
        assert_reopens_clean(dir.path(), &before);
    }
}

#[test]
fn failed_reproof_of_an_orphaned_object_never_acknowledges_a_ref() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge
        .write(&root, &ns, "/orphan.txt", b"recoverable orphan", false)
        .unwrap();
    let before = refs(&forge);

    // Leave fully written, visible objects behind while withholding the leaf
    // directory proof and metadata CAS.
    let publication_fault =
        barrier_fault::fail_at(DurabilityBarrier::ObjectPublicationDirectory, 1);
    assert!(forge.checkin(&root, &ns, "/", "orphan producer").is_err());
    assert!(publication_fault.fired());
    drop(publication_fault);
    assert_eq!(refs(&forge), before);
    drop(forge);

    // A cold Store cannot inherit the failed process's positive proof. It must
    // rehash and force the existing file before it can finish publication.
    let reopened = Forge::open(dir.path()).unwrap();
    let root = reopened.root_cap().unwrap();
    let reproof_fault = barrier_fault::fail_at(DurabilityBarrier::ObjectExistingFile, 1);
    assert!(reopened
        .checkin(&root, &ns, "/", "orphan recovery")
        .is_err());
    assert!(reproof_fault.fired());
    assert_eq!(refs(&reopened), before);
    drop(reproof_fault);
    drop(reopened);

    assert_reopens_clean(dir.path(), &before);
}

#[test]
fn failed_prepublication_init_barriers_leave_no_repository() {
    // Exercise both bootstrap-owned barriers and object-store barriers reached
    // while constructing the initial tree and commit.
    for point in [
        DurabilityBarrier::InitFile,
        DurabilityBarrier::ObjectPathDirectory,
        DurabilityBarrier::ObjectFile,
        DurabilityBarrier::ObjectPublicationDirectory,
        DurabilityBarrier::InitKeyDirectory,
        DurabilityBarrier::InitStagingDirectory,
    ] {
        let dir = tempdir().unwrap();
        let fault = barrier_fault::fail_at(point, 1);
        let result = Forge::init(dir.path());
        assert!(fault.fired(), "barrier was not reached: {point:?}");
        assert!(result.is_err(), "injected {point:?} was acknowledged");
        assert!(
            !dir.path().join(".forge").exists(),
            "prepublication failure {point:?} exposed a repository"
        );
        drop(fault);

        let initialized = Forge::init(dir.path()).unwrap();
        let expected = refs(&initialized);
        drop(initialized);
        assert_reopens_clean(dir.path(), &expected);
    }
}

#[test]
fn failed_init_parent_barrier_is_retryable() {
    let outer = tempdir().unwrap();
    let target = outer.path().join("missing").join("repository");
    let fault = barrier_fault::fail_at(DurabilityBarrier::InitParentDirectory, 1);
    let result = Forge::init(&target);
    assert!(fault.fired());
    assert!(result.is_err());
    assert!(!target.join(".forge").exists());
    drop(fault);

    let initialized = Forge::init(&target).unwrap();
    let expected = refs(&initialized);
    drop(initialized);
    assert_reopens_clean(&target, &expected);
}

#[test]
fn failed_staging_cleanup_barrier_is_retryable() {
    let dir = tempdir().unwrap();
    let stale = dir
        .path()
        .join(format!(".forge.init-999-{}", ulid::Ulid::new()));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("debris"), b"incomplete init").unwrap();

    let fault = barrier_fault::fail_at(DurabilityBarrier::InitCleanupDirectory, 1);
    let result = Forge::init(dir.path());
    assert!(fault.fired());
    assert!(result.is_err());
    assert!(!dir.path().join(".forge").exists());
    assert!(!stale.exists(), "cleanup failure left reclaimed debris visible");
    drop(fault);

    let initialized = Forge::init(dir.path()).unwrap();
    let expected = refs(&initialized);
    drop(initialized);
    assert_reopens_clean(dir.path(), &expected);
}

#[test]
fn failed_init_publication_barrier_exposes_only_a_complete_repository() {
    let dir = tempdir().unwrap();
    let fault = barrier_fault::fail_at(DurabilityBarrier::InitPublicationDirectory, 1);
    let result = Forge::init(dir.path());
    assert!(fault.fired());
    assert!(
        result.is_err(),
        "failed publication barrier was acknowledged"
    );
    assert_eq!(
        std::fs::read(dir.path().join(".forge/VERSION")).unwrap(),
        b"1\n"
    );
    drop(fault);

    // A cold open joins the parent-directory durability proof. The repository
    // was atomically visible but init correctly did not report success.
    let reopened = Forge::open(dir.path()).unwrap();
    let expected = refs(&reopened);
    let root = reopened.root_cap().unwrap();
    let report = reopened.fsck(&root, true).unwrap();
    assert!(report.ok, "{:#?}", report.findings);
    drop(reopened);
    assert_reopens_clean(dir.path(), &expected);
}

#[test]
fn failed_cold_open_publication_join_never_exposes_a_handle() {
    let dir = tempdir().unwrap();
    let initialized = Forge::init(dir.path()).unwrap();
    let expected = refs(&initialized);
    drop(initialized);

    let fault = barrier_fault::fail_at(DurabilityBarrier::OpenPublicationDirectory, 1);
    let result = Forge::open(dir.path());
    assert!(fault.fired());
    assert!(result.is_err(), "failed open barrier returned a live handle");
    drop(fault);

    assert_reopens_clean(dir.path(), &expected);
}

#[test]
fn interrupted_post_commit_acknowledgement_is_durable_and_retryable() {
    let dir = tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    let live = format!("heads/agents/{}/{}", sanitize_agent(root.agent_id()), ns);
    forge
        .write(&root, &ns, "/committed.txt", b"durable outcome", false)
        .unwrap();
    let before = refs(&forge);
    let before_live = before.iter().find(|row| row.name == live).unwrap().oid;

    let fault = barrier_fault::fail_at(DurabilityBarrier::MetadataRefCommitAfter, 1);
    let result = forge.checkin(&root, &ns, "/", "commit then lose acknowledgement");
    assert!(fault.fired());
    assert!(result.is_err(), "post-commit interruption was acknowledged");
    let after = refs(&forge);
    let after_live = after.iter().find(|row| row.name == live).unwrap().oid;
    assert_ne!(after_live, before_live, "SQL commit did not become visible");
    drop(fault);

    let retry = forge.checkin(&root, &ns, "/", "recover outcome").unwrap();
    assert_eq!(
        retry,
        CasResult::Noop {
            name: live,
            oid: after_live,
        },
        "retry must recover the already committed outcome without republishing"
    );
    drop(forge);

    assert_reopens_clean(dir.path(), &after);
}

#[test]
fn failed_checkpoint_transition_preserves_acknowledged_refs() {
    for point in [
        DurabilityBarrier::MetadataCheckpointBefore,
        DurabilityBarrier::MetadataCheckpointAfter,
    ] {
        let dir = tempdir().unwrap();
        let forge = Forge::init(dir.path()).unwrap();
        let expected = refs(&forge);
        drop(forge);

        let meta = Meta::open(&dir.path().join(".forge/meta.sqlite")).unwrap();
        let fault = barrier_fault::fail_at(point, 1);
        let result = meta.checkpoint_truncate();
        assert!(fault.fired(), "barrier was not reached: {point:?}");
        assert!(result.is_err(), "injected {point:?} was acknowledged");
        drop(fault);
        drop(meta);

        assert_reopens_clean(dir.path(), &expected);
    }
}
