//! The backend-neutral contract every [`ObjectStore`] must satisfy.
//!
//! This is the answer to "how would a future backend be checked". It asserts
//! only what the seam can actually promise -- ordering, accounting and the join
//! rule -- and never an fsync count, a path, or a syscall. A backend that
//! cannot pass it unchanged is not an object store for ForgeFS.
//!
//! It deliberately does NOT prove physical durability. See the module docs of
//! [`super`] for the crash and cross-process evidence each backend still owes
//! on its own.

use super::{DurabilityClass, ObjectStore};
use forge_core::hash_bytes;
use forge_types::{Error, ObjectId};

/// How to build the store under test. A fixture owns whatever scratch state
/// (temp directories, servers) its backend needs and keeps it alive.
pub(crate) trait ObjectStoreFixture {
    type S: ObjectStore;

    /// Name used in assertion messages, so a failure says which backend broke.
    fn name(&self) -> &'static str;

    /// The class this backend claims. Checked, so a backend cannot quietly
    /// weaken its claim without a test diff.
    fn expected_class(&self) -> DurabilityClass;

    /// A fresh, empty, writable store.
    fn writable(&self) -> Self::S;

    /// A store opened against immutable media.
    fn read_only(&self) -> Self::S;

    /// Overwrite an object's stored bytes behind the store's back, modelling
    /// media corruption. Return `false` if the backend offers no such hook;
    /// the verification assertion is then skipped rather than faked.
    fn corrupt(&self, store: &Self::S, id: ObjectId, bytes: &[u8]) -> bool;
}

/// Run the whole contract. Call this from a `#[test]` per backend.
pub(crate) fn assert_object_store_contract<F: ObjectStoreFixture>(fixture: &F) {
    let who = fixture.name();
    declares_its_durability_class(fixture, who);
    content_addressed_and_idempotent(fixture, who);
    gather_addresses_the_concatenation(fixture, who);
    absent_object_is_not_found(fixture, who);
    batch_publishes_every_object(fixture, who);
    dropped_batch_publishes_no_accounting(fixture, who);
    new_publication_pays_object_and_naming_barriers(fixture, who);
    dedup_of_a_proven_object_pays_no_barrier(fixture, who);
    join_of_an_unfinished_peer_reproves_the_whole_path(fixture, who);
    read_only_store_refuses_publication(fixture, who);
    get_reverifies_the_content_address(fixture, who);
}

fn declares_its_durability_class<F: ObjectStoreFixture>(f: &F, who: &str) {
    assert_eq!(
        f.writable().durability_class(),
        f.expected_class(),
        "{who}: a backend must declare what its finish barrier is worth"
    );
}

fn content_addressed_and_idempotent<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let a = s.put(b"abc").unwrap();
    let b = s.put(b"abc").unwrap();
    assert_eq!(a, b, "{who}: identical bytes must publish to one address");
    assert_eq!(a, hash_bytes(b"abc"), "{who}: address must be the content");
    assert_eq!(
        s.get(a).unwrap(),
        b"abc",
        "{who}: read back what was written"
    );
    assert!(s.has(a), "{who}: a published object is visible");
}

/// The seam has to carry #320: the local publisher writes the caller's payload
/// in place instead of allocating the concatenation, and `put_blob_data` frames
/// a blob as `[prefix, data]`. That only preserves I2 if every backend agrees
/// that `parts` is one object cut anywhere, never a structure inside it. A
/// backend that hashed the parts separately, framed them, or reordered them
/// would publish a different object under the same call, and the same repository
/// would have two addresses for one byte string depending on how it was written.
fn gather_addresses_the_concatenation<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let whole: &[u8] = b"a frame prefix and then the payload";
    let id = s.put(whole).unwrap();

    for cut in 0..=whole.len() {
        let (head, tail) = whole.split_at(cut);
        assert_eq!(
            s.put_parts(&[head, tail]).unwrap(),
            id,
            "{who}: put_parts must address the concatenation, not the split at {cut}"
        );
    }
    assert_eq!(
        s.get(id).unwrap(),
        whole,
        "{who}: a gathered publication stores the joined bytes"
    );

    // The batch form is the one a real publisher uses, and it is the one that
    // owes the naming barrier, so assert it separately rather than trusting the
    // single-object default to stand in for it.
    let mut batch = s.begin_batch();
    let (head, tail) = whole.split_at(7);
    assert_eq!(
        batch.put_parts(&[head, tail]).unwrap(),
        id,
        "{who}: a batched gather addresses the same object"
    );
    batch.finish().unwrap();

    // Empty parts are still parts of nothing: they must not change the address.
    let empty = s.put(b"").unwrap();
    assert_eq!(
        s.put_parts(&[]).unwrap(),
        empty,
        "{who}: no parts is the empty object"
    );
    assert_eq!(
        s.put_parts(&[b"", b"", b""]).unwrap(),
        empty,
        "{who}: empty parts do not frame anything"
    );
    let (head, tail) = whole.split_at(3);
    assert_eq!(
        s.put_parts(&[b"", head, b"", tail, b""]).unwrap(),
        id,
        "{who}: interleaved empty parts do not change the concatenation"
    );
}

fn absent_object_is_not_found<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let id = hash_bytes(b"never published");
    assert!(!s.has(id), "{who}: an unpublished object is not visible");
    assert!(
        matches!(s.get(id), Err(Error::NotFound(_))),
        "{who}: an unpublished object must be NotFound"
    );
}

fn batch_publishes_every_object<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let payloads: [&[u8]; 3] = [b"one", b"two", b"three"];
    let mut batch = s.begin_batch();
    let ids: Vec<_> = payloads
        .iter()
        .map(|bytes| batch.put(bytes).unwrap())
        .collect();
    batch.finish().unwrap();
    for (id, bytes) in ids.iter().zip(payloads) {
        assert_eq!(
            s.get(*id).unwrap(),
            bytes,
            "{who}: every object in a finished batch must be readable"
        );
    }
}

/// I4's negative half at the accounting level: nothing a dropped batch touched
/// may be reported as published, because no ref is allowed to name it.
fn dropped_batch_publishes_no_accounting<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let before = s.stats();
    let mut batch = s.begin_batch();
    batch.put(b"abandoned publication").unwrap();
    drop(batch);
    let after = s.stats();
    assert_eq!(
        after.puts, before.puts,
        "{who}: a dropped batch must not count as a publication"
    );
    assert_eq!(
        after.dedup_hits, before.dedup_hits,
        "{who}: a dropped batch must not count as a dedup hit"
    );
}

/// I4's positive half: bytes AND the name that reaches them, both proven by the
/// time `finish` returns, with the publication itself only counted then.
fn new_publication_pays_object_and_naming_barriers<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let before = s.stats();
    let mut batch = s.begin_batch();
    let id = batch.put(b"a brand new durable object").unwrap();
    assert_eq!(
        s.stats().puts,
        before.puts,
        "{who}: publication accounting must be deferred to finish"
    );
    batch.finish().unwrap();
    let after = s.stats();
    assert_eq!(
        after.puts,
        before.puts + 1,
        "{who}: a finished batch publishes exactly its new objects"
    );
    assert!(
        after.fsync_file > before.fsync_file,
        "{who}: a new object must pay an object-bytes barrier"
    );
    assert!(
        after.fsync_dir > before.fsync_dir,
        "{who}: a new object must pay a naming barrier by finish"
    );
    assert_eq!(s.get(id).unwrap(), b"a brand new durable object");
}

fn dedup_of_a_proven_object_pays_no_barrier<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let bytes = b"already proven durable";
    let id = s.put(bytes).unwrap();
    let before = s.stats();

    let mut batch = s.begin_batch();
    assert_eq!(batch.put(bytes).unwrap(), id);
    batch.finish().unwrap();

    let after = s.stats();
    assert_eq!(
        after.puts, before.puts,
        "{who}: a dedup hit is not a publication"
    );
    assert_eq!(
        after.dedup_hits,
        before.dedup_hits + 1,
        "{who}: a dedup hit must be counted as one"
    );
    assert_eq!(
        after.fsync_file, before.fsync_file,
        "{who}: an already-proven object must not re-pay the bytes barrier"
    );
    assert_eq!(
        after.fsync_dir, before.fsync_dir,
        "{who}: an already-proven name must not re-pay the naming barrier"
    );
}

/// The rule a naive put/get/has trait loses, and the reason this seam is not
/// three methods. A batch that deduplicates against an object made *visible*
/// by another batch that has not finished inherits no proof at all: visibility
/// is never durability (I4). It must reproduce both barriers itself, because
/// its caller is about to CAS-publish a ref naming that object while the peer
/// that made it visible may already be dead.
fn join_of_an_unfinished_peer_reproves_the_whole_path<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let bytes = b"joined from an unfinished peer";

    let mut winner = s.begin_batch();
    let id = winner.put(bytes).unwrap();
    let before = s.stats();

    let mut joiner = s.begin_batch();
    assert_eq!(joiner.put(bytes).unwrap(), id);
    // Model the peer dying after making the object visible and before it ever
    // proved anything. The joiner is now the only batch that can.
    drop(winner);
    joiner.finish().unwrap();

    let after = s.stats();
    assert_eq!(
        after.puts, before.puts,
        "{who}: joining an existing object is not a new publication"
    );
    assert_eq!(
        after.dedup_hits,
        before.dedup_hits + 1,
        "{who}: joining an existing object is a dedup hit"
    );
    assert!(
        after.fsync_file > before.fsync_file,
        "{who}: a joiner must re-prove the bytes of an unproven visible object"
    );
    assert!(
        after.fsync_dir > before.fsync_dir,
        "{who}: a joiner must re-prove the name of an unproven visible object"
    );
    assert_eq!(s.get(id).unwrap(), bytes);
}

fn read_only_store_refuses_publication<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.read_only();
    assert!(s.read_only(), "{who}: a read-only store must say so");
    assert!(
        matches!(s.put(b"forbidden"), Err(Error::Denied(_))),
        "{who}: a read-only store must refuse at the write boundary"
    );
    let mut batch = s.begin_batch();
    assert!(
        matches!(batch.put(b"forbidden"), Err(Error::Denied(_))),
        "{who}: a read-only store must refuse inside a batch too"
    );
}

/// I15: a content-addressed read that trusts its own index is not a read.
fn get_reverifies_the_content_address<F: ObjectStoreFixture>(f: &F, who: &str) {
    let s = f.writable();
    let id = s.put(b"trust boundary").unwrap();
    if !f.corrupt(&s, id, b"evil") {
        return;
    }
    assert!(
        matches!(s.get(id), Err(Error::Corrupt(_))),
        "{who}: get must re-hash durable bytes and reject a mismatch"
    );
}
