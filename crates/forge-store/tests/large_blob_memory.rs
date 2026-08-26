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
use forge_store::{Store, OBJECT_CACHE_MAX_BYTES};
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
fn large_blob_cost_and_raw_cache_residency_are_measured_and_bounded() {
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

    // Phase 3 - reading one object from a cold store. At 8 MiB the object is
    // intentionally below the 64 MiB cache budget, so the single-object peak
    // remains three payloads: durable read buffer, cached clone, decoded copy.
    // Phase 4 below is the important bound: walking many such blobs no longer
    // retains one payload per object without limit.
    drop(store);
    let cold = Store::open(a.path()).unwrap();
    let (got, read) = peak_payloads(|| cold.get_blob_data(id).unwrap());
    assert_eq!(got.len(), N);
    assert!(
        (2.5..3.5).contains(&read),
        "cold get_blob_data of a {N}-byte blob peaked at {read:.2}x the \
         payload; one cacheable object still costs the characterised 3x"
    );
    drop(got);
    drop(cold);

    // Phase 4 - the raw-object LRU is bounded by bytes as well as entries.
    // Use enough distinct 8 MiB blobs to exceed 64 MiB. The former 256-entry
    // policy retained every one (80+ MiB here, and tens of GiB at the measured
    // 164 MiB object ceiling). The byte-bound cache must evict old entries and
    // keep both resident and transient live memory bounded by the declared
    // budget plus the two buffers needed by the current decode.
    let writer = Store::open(a.path()).unwrap();
    let mut many = payload();
    let object_count = OBJECT_CACHE_MAX_BYTES / N + 2;
    let mut ids = Vec::with_capacity(object_count);
    for i in 0..object_count {
        many[..8].copy_from_slice(&(i as u64).to_le_bytes());
        ids.push(writer.put_blob_data(&many).unwrap());
    }
    drop(many);
    drop(writer);

    let cold_many = Store::open(a.path()).unwrap();
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    for object in &ids {
        let got = cold_many.get_blob_data(*object).unwrap();
        assert_eq!(got.len(), N);
        drop(got);
    }
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    assert!(
        retained < OBJECT_CACHE_MAX_BYTES,
        "walking {object_count} large objects retained {retained} bytes after \
         callers dropped their buffers; raw-object cache budget is \
         {OBJECT_CACHE_MAX_BYTES} bytes"
    );
    assert!(
        peak < OBJECT_CACHE_MAX_BYTES + 3 * N,
        "walking {object_count} large objects peaked at {peak} additional live \
         bytes; a byte-bound cache may add only the current read/decode buffers \
         above its {OBJECT_CACHE_MAX_BYTES}-byte resident budget"
    );

    // LRU semantics remain real, not merely accounting: the oldest object was
    // evicted, so asking for it again causes one physical-cache miss.
    let misses_before = cold_many.cache_stats().object_cache_misses;
    let reread = cold_many.get_blob_data(ids[0]).unwrap();
    assert_eq!(reread.len(), N);
    let misses_after = cold_many.cache_stats().object_cache_misses;
    assert_eq!(
        misses_after,
        misses_before + 1,
        "the oldest object should have been evicted when the byte budget filled"
    );
}
