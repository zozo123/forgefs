//! The `ObjectStore` seam: the whole object plane, expressed as a trait.
//!
//! ForgeFS already made the object-storage bet. Objects are immutable,
//! content-addressed and write-once; the only mutable cell is a ref published
//! by CAS. That is the commit protocol object stores converged on. This module
//! makes the bet legible by naming the operations a backend must provide, so
//! `LocalBlobStore` is *an* implementation rather than *the* design.
//!
//! # The durability contract (I4)
//!
//! I4 says: a committed ref implies fsynced object bytes AND every directory
//! edge needed to reach them; visibility alone is never a durability proof.
//! Stated in files, fsync and hard links, that invariant cannot survive a
//! second backend. So the trait splits it in two, and is explicit about which
//! half it can and cannot enforce.
//!
//! ## The half the trait DOES express: ordering and completeness
//!
//! Publication is two-phase, and both phases are in the signatures.
//!
//! * [`ObjectBatch::put`] returns an [`ObjectId`] once the object *bytes* are
//!   on durable media. The object may now be readable through
//!   [`ObjectStore::get`], but it is NOT yet safe to name from a ref: the path
//!   that reaches it may itself be unproven.
//! * [`ObjectBatch::finish`] returns `Ok` only once every object the batch
//!   published *or joined* -- and every naming edge required to reach each one
//!   -- is durable. This is the only point at which a caller is permitted to
//!   CAS-publish a ref naming those OIDs.
//! * Dropping a batch without `finish` publishes nothing: no ref may name its
//!   OIDs, and the implementation must not record a durability proof for them.
//!   A crash here may leave durable orphan objects, which is safe.
//! * "Joined" is load-bearing. If a batch deduplicates against an object some
//!   *other, unfinished* batch made visible, the joining batch inherits no
//!   proof: it must reproduce the barrier itself before its own `finish`
//!   returns. This is exactly the rule a naive `put`/`get`/`has` trait silently
//!   loses, and [`conformance`] asserts it on every implementation.
//! * [`ObjectStore::has`] is a *visibility* predicate, never a durability
//!   proof. It answers "can this be read right now", which is precisely the
//!   thing I4 forbids treating as durability. Nothing that publishes a ref may
//!   use it as a barrier.
//!
//! ## The half the trait CANNOT express: physics
//!
//! No Rust signature can force an implementation to actually reach stable
//! media. Whether `finish` means `F_FULLFSYNC` on a leaf directory, or a
//! conditional `PUT` acknowledged by a quorum, is the implementation's
//! responsibility and stays there. To keep that responsibility from becoming
//! invisible, a backend must:
//!
//! 1. declare a [`DurabilityClass`] and be honest about it. Only
//!    [`DurabilityClass::CrashDurable`] may back a repository that publishes
//!    refs; [`DurabilityClass::ProcessLifetime`] is a test/bench backend and
//!    says so where the type system cannot.
//! 2. pass [`conformance::assert_object_store_contract`] unchanged. That suite
//!    is backend-neutral: it asserts ordering, accounting and the join rule,
//!    never fsync counts.
//! 3. supply its own *physical* evidence, which conformance cannot fabricate:
//!    a fault-injection point per barrier (see [`crate::DurabilityBarrier`]), a
//!    crash test that kills the process between barriers and shows no surviving
//!    ref names a non-durable object, and a cross-process test that a second
//!    process re-proves state a dead peer left merely visible. For the local
//!    backend those are `barrier_fault_injection.rs`, `sigkill_recovery.rs` and
//!    `cross_process_put.rs`.
//!
//! Point 3 is the honest gap. It is a review checklist, not a compile error,
//! and a reviewer of any future backend should demand it by name.
//!
//! ## Known local-only surface, deliberately left outside the trait
//!
//! `fsck`'s orphan sweep and `gc --dry-run`'s candidate scan both walk
//! `objects/` directly through [`crate::Store::root`], and `gc` additionally
//! reads each candidate's size and age. Enumeration is a real capability a
//! remote backend would have to provide (a `list`/`scan` method yielding at
//! least an id, a size and an age), and pretending otherwise inside this trait
//! would be the leak this module exists to avoid. Until it exists, those two
//! operations are local-backend operations rather than seam operations. It is
//! named here so the next backend author finds it before their first bug.

use crate::blob::BlobStoreStats;
use forge_types::{ObjectId, Result};

/// What an implementation's `finish` barrier is actually worth.
///
/// This exists because the trait cannot check physics. An implementation
/// declares its class; the conformance suite asserts that a store used to back
/// a ref-publishing repository declares [`Self::CrashDurable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurabilityClass {
    /// `finish` has forced object bytes and every naming edge to stable media.
    /// The implementation owes crash and cross-process evidence for that claim.
    CrashDurable,
    /// Durable only for this process's lifetime. Legal for tests and
    /// benchmarks; never legal under a ref a reader may trust after power loss.
    ProcessLifetime,
}

/// One publication unit. See the module docs for the two-phase contract.
///
/// Batches are consumed by [`Self::finish`]; dropping one instead publishes
/// nothing. The `Box<Self>` receiver keeps that consuming shape usable through
/// a trait object.
pub trait ObjectBatch: Send {
    /// Publish the object file formed by concatenating `parts`, returning its
    /// content address. On return the bytes are durable; the *name* is not yet
    /// proven.
    ///
    /// Scatter-gather is the primitive rather than a convenience because a
    /// publisher usually already holds the payload and only needs a frame
    /// prefixed to it; a single-buffer signature forces that publisher to
    /// allocate the concatenation, which for a large blob is a second full
    /// copy of it. A seam expressed only over `&[u8]` would reimpose a cost
    /// the local backend stopped paying in #320.
    ///
    /// Identity is the concatenation, never the split: for every way of
    /// cutting the same byte string, `put_parts` MUST return the same
    /// [`ObjectId`] as [`Self::put`] over the joined bytes, publish the same
    /// object file, and take the same barriers (I2, I3, I4). `parts` is a
    /// representation of one object, never a structure inside it. An
    /// implementation that cannot write vectored must concatenate visibly in
    /// its own body, so the omission is a legible cost rather than a silent
    /// one.
    fn put_parts(&mut self, parts: &[&[u8]]) -> Result<ObjectId>;

    /// Single-part form of [`Self::put_parts`].
    fn put(&mut self, bytes: &[u8]) -> Result<ObjectId> {
        self.put_parts(&[bytes])
    }

    /// Complete every deferred naming barrier for this batch. Only after `Ok`
    /// may a ref name any OID this batch returned.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// The object plane. Immutable, content-addressed, write-once.
pub trait ObjectStore: Send + Sync {
    /// What this backend's `finish` barrier is worth. See [`DurabilityClass`].
    fn durability_class(&self) -> DurabilityClass;

    /// Begin a publication batch.
    fn begin_batch(&self) -> Box<dyn ObjectBatch + '_>;

    /// Read and re-verify an object. Implementations MUST re-hash the durable
    /// bytes and fail with `Error::Corrupt` on mismatch (I15): a
    /// content-addressed read that trusts its own index is not a read.
    fn get(&self, id: ObjectId) -> Result<Vec<u8>>;

    /// Verify that `id` names a well-formed Blob, including the backend's
    /// ordinary durable-byte identity check. Implementations may stream this
    /// validation; callers must not infer that the payload was materialized or
    /// cached. The default is deliberately boring and correct for simple
    /// backends.
    fn verify_blob(&self, id: ObjectId) -> Result<()> {
        forge_core::Blob::decode(&self.get(id)?)?;
        Ok(())
    }

    /// Visibility only -- never a durability proof. See the module docs.
    fn has(&self, id: ObjectId) -> bool;

    /// A read-only store refuses every publication at the write boundary
    /// rather than discovering immutability halfway through one.
    fn read_only(&self) -> bool;

    /// Process-lifetime durability accounting. `fsync_file` counts object-byte
    /// barriers and `fsync_dir` counts naming barriers; the physical mechanism
    /// behind each is backend-defined. The field names are frozen by the
    /// `forge stats --json` schema and are read as "object barrier" and "path
    /// barrier" by any non-POSIX backend.
    fn stats(&self) -> BlobStoreStats;

    /// Single-object publication of the concatenation of `parts`. The default
    /// composes the two-phase contract correctly -- one batch, one `put_parts`,
    /// one `finish` -- so an implementation overrides it only to go faster.
    /// See [`ObjectBatch::put_parts`] for why the gather is the primitive.
    fn put_parts(&self, parts: &[&[u8]]) -> Result<ObjectId> {
        let mut batch = self.begin_batch();
        let id = batch.put_parts(parts)?;
        batch.finish()?;
        Ok(id)
    }

    /// Single-object, single-buffer publication.
    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.put_parts(&[bytes])
    }
}

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod memory;
#[cfg(test)]
mod tests;
