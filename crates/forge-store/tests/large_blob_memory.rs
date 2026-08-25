//! Characterisation of what one large object costs in memory (issue #11).
//!
//! I2: one logical object is one byte string is one ObjectId. Nothing here may
//! move an ObjectId. These assertions only pin how much transient memory the
//! VERSION 1 encoding costs to publish and to read back, so the practical
//! ceiling on object size is a measured fact rather than folklore.
//!
//! A counting global allocator is used instead of RSS: it is deterministic and
//! portable, and it is not confused by the allocator returning pages to the OS.
//! SQLite allocates through C `malloc`, so catalog work does not pollute it.
//!
//! The counters are process-global, so every phase lives in one `#[test]`; a
//! second test function would run concurrently and interleave its allocations.

use forge_core::{hash_bytes, Blob};
use forge_store::Store;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            bump(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
            bump(new);
        }
        q
    }
}

fn bump(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Peak *additional* live bytes observed while `f` runs, as a multiple of the
/// payload size.
fn peak_payloads<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(before) as f64 / N as f64)
}

const N: usize = 8 * 1024 * 1024;

fn payload() -> Vec<u8> {
    let mut v = vec![0u8; N];
    let mut x: u32 = 0x9e37_79b9;
    for c in v.chunks_mut(4) {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
    }
    v
}

#[test]
fn one_large_blob_costs_a_measured_multiple_of_itself() {
    let a = tempdir().unwrap();
    let store = Store::open(a.path()).unwrap();
    let data = payload();

    // Phase 1 - first publication. The v1 blob file is a 16-byte frame
    // followed by the payload verbatim, so a publisher can hash and write the
    // caller's buffer in place and allocate nothing proportional to it.
    //
    // Before that was true, `put_blob_data` allocated the payload twice (a
    // `to_vec` into a temporary `Blob`, then the encode buffer). Peak was 3x
    // the payload and the largest publishable object was about a third of
    // addressable memory: on a 512 MiB address-space budget the bisected
    // ceiling was 164 MiB.
    let (id, publish) = peak_payloads(|| store.put_blob_data(&data).unwrap());
    assert!(
        publish < 0.5,
        "first publication of a {N}-byte blob peaked at {publish:.2}x the \
         payload; a copy-free put allocates only the 16-byte frame"
    );

    // Identity is the point of the exercise: the streaming publication must
    // produce exactly the bytes and the ObjectId of the buffered encoding (I2,
    // FORMAT.md VERSION 1).
    assert_eq!(id, hash_bytes(&Blob { data: data.clone() }.encode()));

    // Phase 2 - republishing identical bytes. Dedup still re-reads the whole
    // durable object to re-prove its hash at the trust boundary (I3), so it
    // costs one payload. Characterised, not endorsed.
    let (again, dedup) = peak_payloads(|| store.put_blob_data(&data).unwrap());
    assert_eq!(again, id);
    assert!(
        (0.75..1.5).contains(&dedup),
        "republishing a {N}-byte blob peaked at {dedup:.2}x the payload; \
         the characterised cost is one payload for the verifying re-read"
    );

    // Phase 3 - reading it back from a cold store. Three payloads are live at
    // once: the durable read buffer, the clone the object cache keeps, and the
    // decoded copy handed to the caller. That cache is bounded by entry count
    // (256), never by bytes, so 256 large objects are 256 payloads resident.
    // This is the half of issue #11 that a chunked read path would have to beat.
    drop(store);
    let cold = Store::open(a.path()).unwrap();
    let (got, read) = peak_payloads(|| cold.get_blob_data(id).unwrap());
    assert_eq!(got.len(), N);
    assert!(
        (2.5..3.5).contains(&read),
        "cold get_blob_data of a {N}-byte blob peaked at {read:.2}x the \
         payload; the characterised cost is 3x. If this moved, issue #11's \
         measured ceiling moved with it and docs/BENCH.md needs re-measuring"
    );
}
