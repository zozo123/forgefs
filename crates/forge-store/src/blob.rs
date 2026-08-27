use crate::metrics::TimingCounter;
use forge_core::{blob_frame_prefix, hash_bytes, hash_parts, hash_reader};
use forge_types::{Error, ObjectId, Result};
use lru::LruCache;
use parking_lot::{Condvar, Mutex};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The name of the lock that serialises a deduplicating put against a sweep.
///
/// It lives in `tmp/` because that is the one repository directory with no
/// layout contract: `objects/` is asserted shard-by-shard by `fsck`, and
/// `cleanup_stale_tmp` only reclaims tmp entries whose name is a ULID, so a
/// dotfile here is neither corruption nor litter.
const GC_LOCK_NAME: &str = ".gc-lock";

/// Outcome of refreshing a deduplicated object's age.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refreshed {
    /// The object is present and its age now reads as "just relied upon".
    Yes,
    /// The object was gone by the time we held the lock. A collector unlinked
    /// it, and the caller must republish the bytes rather than name absent
    /// ones.
    Vanished,
}

/// Open the reclamation lock. Created on demand; only write paths reach here.
fn open_gc_lock(root: &Path) -> Result<fs::File> {
    let tmp = root.join("tmp");
    // A fresh descriptor per acquisition is deliberate: `flock` locks are held
    // per open file description, so a cached descriptor shared between threads
    // would let a sweep and a publisher in the same process "both" hold the
    // lock by converting it.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(tmp.join(GC_LOCK_NAME))
        .map_err(|e| Error::Io(format!("cannot open the reclamation lock: {e}")))
}

fn flock(file: &fs::File, operation: i32, what: &str) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(Error::Io(format!("{what}: {error}")));
    }
    Ok(())
}

/// Exclusion held by a sweep while it decides and unlinks.
///
/// While this is alive no deduplicating put anywhere -- this process or another
/// -- can refresh an object's age, and therefore no put can pass the "these
/// bytes are already here, I need not write them" branch. That is what makes
/// the sweep's age check and its unlink one indivisible decision instead of
/// two syscalls with a race between them (I23).
pub struct GcObjectGuard {
    _file: fs::File,
}

/// A deduplicating publication refreshes the object's modification time,
/// under the reclamation lock.
///
/// This is the barrier content addressing makes necessary. I3 says a put whose
/// bytes already exist never rewrites them, so a writer that legitimately
/// reproduces an object's bytes and is about to name it from a ref leaves that
/// object looking arbitrarily old on disk. `gc`'s grace floor is expressed in
/// object age, so without this the floor bounds nothing for exactly the
/// objects most at risk of being swept out from under a live writer. After
/// this call an object's age reads as "time since a writer last relied on
/// these bytes", which is the quantity the floor has to bound (I23).
///
/// The shared lock is what makes it airtight rather than merely likely. A
/// sweep holds the same lock exclusively, so this refresh either happens
/// entirely before the sweep read the age -- in which case the sweep sees a
/// young object and withholds it -- or entirely after the sweep finished, in
/// which case the object is either still there or already gone and reported as
/// [`Refreshed::Vanished`]. There is no interleaving in which a sweep reads an
/// old age and unlinks bytes a publisher has just joined.
///
/// Any other failure is fatal to the put. A publisher that cannot refresh the
/// age of an object it is about to name cannot prove that object will survive
/// a concurrent collection, so it refuses rather than publishing a name over
/// bytes a sweep is entitled to delete.
fn refresh_dedup_mtime(root: &Path, path: &Path) -> Result<Refreshed> {
    use std::os::unix::ffi::OsStrExt;
    let lock = open_gc_lock(root)?;
    flock(&lock, libc::LOCK_SH, "cannot share the reclamation lock")?;
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| Error::Io(format!("object path is not a C string: {e}")))?;
    // atime is deliberately left alone: it is not a liveness signal and
    // touching it would fight `relatime` for no benefit.
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
    ];
    // SAFETY: `raw` is a valid NUL-terminated path and `times` is a
    // two-element array of the layout utimensat documents.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, raw.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        return Ok(Refreshed::Yes);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(Refreshed::Vanished);
    }
    Err(Error::Io(format!(
        "cannot refresh the age of deduplicated object {}: {error}; publishing a ref over bytes \
         whose age cannot be refreshed would expose them to collection (I23)",
        path.display()
    )))
}

const STALE_TMP_MS: u64 = 24 * 60 * 60 * 1000;
const DURABLE_DIR_CACHE_CAPACITY: usize = 65_536;
const DURABLE_OID_CACHE_CAPACITY: usize = 65_536;

/// How a [`PublishBatch`] proves the directory edges that reach its objects.
///
/// All three settings satisfy I4 identically: when `finish` returns, every
/// object file the batch published or joined *and* every directory entry on
/// the path to it is durable, and only then may a ref name it. They differ
/// solely in how many barriers the kernel is asked for to establish that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryBarrier {
    /// One `fsync` per touched directory, taken as the batch touches it.
    ///
    /// The default, and the shape this store has always had. `Deferred`
    /// measures within noise of it rather than beating it (docs/BENCH.md), so
    /// the durability-critical path keeps the shape that has been exercised
    /// the longest.
    PerDirectory,
    /// One `fsync` per *distinct* touched directory, all of them taken in a
    /// single phase immediately before `finish` returns.
    ///
    /// Same primitive and same count of proved edges as `PerDirectory`, and
    /// portable to every platform. What changes is when they are issued: a
    /// journaling filesystem commits its running transaction on the first
    /// barrier of the phase, so the rest find nothing left to commit and cost
    /// a bare device flush instead of a journal commit each.
    Deferred,
    /// One filesystem-wide barrier for the whole batch, shared with every
    /// concurrent batch that is also waiting for one.
    ///
    /// This is a *stronger* barrier than the set it replaces, not a weaker
    /// one: `syncfs(2)` forces every dirty inode and page on the filesystem
    /// and ends in the same device cache flush, so it makes durable a strict
    /// superset of `{this batch's object bytes} ∪ {this batch's directory
    /// edges}`. The saving is in barrier count, never in coverage.
    ///
    /// The cost of that strength is that a repository sharing a filesystem
    /// with an unrelated heavy writer pays for that writer's dirty data on
    /// every checkin. Set `FORGEFS_DIR_BARRIER=per-directory` there.
    Collapsed,
}

impl DirectoryBarrier {
    /// `FORGEFS_DIR_BARRIER` selects the policy explicitly; an unset or
    /// unrecognised value takes the platform default. The variable is read
    /// once per object-store open and never on a barrier path.
    fn from_env() -> Self {
        match std::env::var("FORGEFS_DIR_BARRIER").ok().as_deref() {
            Some("per-directory") => Self::PerDirectory,
            Some("deferred") => Self::Deferred,
            Some("collapsed") => Self::Collapsed,
            _ => Self::platform_default(),
        }
    }

    /// `PerDirectory` everywhere. `Deferred` proves exactly the same edges
    /// with exactly the same primitive and is portable, but it measures within
    /// noise of `PerDirectory` at every concurrency point (docs/BENCH.md), and
    /// a behaviour change that does not materially improve is not worth taking
    /// in the durability-critical path. `Collapsed` is not the default because
    /// a filesystem-wide barrier costs far more than one `fsync` and is a
    /// global serialisation point, so it loses 15-22% under concurrency even
    /// though it issues fewer barriers -- see docs/BENCH.md.
    const fn platform_default() -> Self {
        Self::PerDirectory
    }

    /// Whether every directory barrier waits for the single phase before
    /// `finish` returns.
    fn defers(self) -> bool {
        matches!(self, Self::Deferred | Self::Collapsed)
    }
}

/// The completion state of the shared filesystem-wide barrier.
///
/// `completed` counts barriers that ran to completion and is monotone;
/// `in_flight` names the generation currently executing. A batch that needs a
/// barrier computes the first generation that is guaranteed to *start* after
/// its own writes were already in the kernel, and waits for exactly that
/// generation. That is why a follower can never be acknowledged by a barrier
/// that began before its own `link(2)` returned.
#[derive(Debug, Default)]
struct BarrierGate {
    completed: u64,
    in_flight: Option<u64>,
}

/// One filesystem-wide durability barrier, shared by every batch of one
/// object store that is waiting for one.
#[derive(Debug)]
struct FsBarrier {
    /// Any descriptor on the object store's filesystem selects the filesystem
    /// to force. The store root is held open for the store's lifetime so a
    /// barrier costs one syscall and no path walk.
    anchor: fs::File,
    state: Mutex<BarrierGate>,
    ready: Condvar,
}

impl FsBarrier {
    fn open(root: &Path) -> Option<Arc<Self>> {
        if !cfg!(any(target_os = "linux", target_os = "android")) {
            return None;
        }
        let anchor = fs::File::open(root).ok()?;
        Some(Arc::new(Self {
            anchor,
            state: Mutex::new(BarrierGate::default()),
            ready: Condvar::new(),
        }))
    }

    /// Return only once a filesystem-wide barrier that *started after this
    /// call* has completed successfully.
    ///
    /// The ordering argument, which is the whole correctness case:
    ///
    /// * `target` is read after the caller's last `write`/`link`/`mkdir` has
    ///   returned, so those are already visible to the kernel.
    /// * `completed` only ever advances by one, to the generation a leader
    ///   just finished, so `completed >= target` proves generation `target`
    ///   itself ran to completion.
    /// * generation `target` cannot have started before `target` was read:
    ///   at that instant either `completed == target - 1` and nothing was in
    ///   flight, or `target - 1` was the generation in flight. Either way the
    ///   leader of `target` had not yet called into the kernel.
    ///
    /// So the barrier this call waits for began after this caller's bytes and
    /// directory entries existed, and a caller is never told "durable" by a
    /// barrier that could not have seen its work.
    fn wait(&self, stats: &BlobStoreCounters) -> Result<()> {
        let target = {
            let state = self.state.lock();
            state.in_flight.unwrap_or(state.completed) + 1
        };
        loop {
            let generation = {
                let mut state = self.state.lock();
                if state.completed >= target {
                    stats.barrier_fs_batches.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                if state.in_flight.is_some() {
                    self.ready.wait(&mut state);
                    continue;
                }
                let generation = state.completed + 1;
                state.in_flight = Some(generation);
                generation
            };
            let started = Instant::now();
            let outcome = sync_filesystem(&self.anchor);
            let elapsed = started.elapsed();
            {
                let mut state = self.state.lock();
                state.in_flight = None;
                // A failed barrier publishes nothing. `completed` does not
                // advance, so every waiter stays unacknowledged and the next
                // one retries rather than inheriting a proof that does not
                // exist.
                if outcome.is_ok() {
                    state.completed = generation;
                }
                self.ready.notify_all();
            }
            outcome?;
            stats.barrier_fs.observe(elapsed);
        }
    }
}

/// Force every dirty inode and data page on the filesystem holding `anchor`,
/// ending in a device cache flush.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn sync_filesystem(anchor: &fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::syncfs(anchor.as_raw_fd()) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn sync_filesystem(_anchor: &fs::File) -> Result<()> {
    Err(Error::Internal(
        "no filesystem-wide durability barrier on this platform".into(),
    ))
}

/// Monotonic process-local counters for physical durability work.
/// `puts` counts newly published OIDs; dedup hits do not increment it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobStoreStats {
    pub puts: u64,
    /// Publications satisfied by an object that was already present. These
    /// perform no new file barrier, so they are excluded from `puts` and from
    /// every `fsync_*` total.
    pub dedup_hits: u64,
    /// Successful file durability barriers.
    pub fsync_file: u64,
    /// Cumulative elapsed time for successful file durability barriers.
    pub fsync_file_us: u64,
    /// Successful directory durability barriers.
    pub fsync_dir: u64,
    /// Cumulative elapsed time for successful directory durability barriers.
    pub fsync_dir_us: u64,
    /// Successful filesystem-wide durability barriers this store executed.
    /// One of these stands in for the whole per-directory set of one or more
    /// batches, so it is counted once per barrier and never once per batch.
    pub barrier_fs: u64,
    /// Cumulative elapsed time for successful filesystem-wide barriers.
    pub barrier_fs_us: u64,
    /// Batches whose directory phase was satisfied by a filesystem-wide
    /// barrier, counting followers as well as the leader that ran it.
    /// `barrier_fs_batches / barrier_fs` is the achieved sharing depth.
    pub barrier_fs_batches: u64,
    /// Object-file bytes this process actually wrote, summed over `puts`. It
    /// is the payload length, not the on-disk allocation.
    pub put_bytes: u64,
    /// Object-file bytes a publication did NOT have to write because the
    /// object was already present, summed over `dedup_hits`. Paired with
    /// `put_bytes` it is the storage amplification content addressing avoided,
    /// which is the number issue #42 asks for and `puts` alone cannot give.
    pub dedup_bytes: u64,
    /// Object-file bytes read back from durable storage. Reads served from a
    /// `Store` cache never reach here, so `get_bytes` is physical read volume
    /// and the cache counters are what explain the difference.
    pub get_bytes: u64,
    /// Objects whose durable bytes did not rehash to the id that named them.
    /// Every one is a refused read (I1, I3, I15); the counter exists so a
    /// corrupt store is visible as a rate and not only as one error string.
    pub hash_failures: u64,
}

impl BlobStoreStats {
    /// Saturating sum over this process-lifetime snapshot. It is not a
    /// per-publication or per-checkin measurement.
    pub fn barrier_us(&self) -> u64 {
        self.fsync_file_us
            .saturating_add(self.fsync_dir_us)
            .saturating_add(self.barrier_fs_us)
    }

    /// Every barrier that proved a directory edge durable, whichever policy
    /// took it. This is the count to compare across
    /// [`DirectoryBarrier`] settings; `fsync_dir` alone is policy-specific.
    pub fn directory_barriers(&self) -> u64 {
        self.fsync_dir.saturating_add(self.barrier_fs)
    }
}

#[derive(Debug, Default)]
struct BlobStoreCounters {
    puts: AtomicU64,
    put_bytes: AtomicU64,
    dedup_hits: AtomicU64,
    dedup_bytes: AtomicU64,
    get_bytes: AtomicU64,
    hash_failures: AtomicU64,
    fsync_file: TimingCounter,
    fsync_dir: TimingCounter,
    barrier_fs: TimingCounter,
    barrier_fs_batches: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    root: PathBuf,
    /// A read-only store refuses every publication instead of discovering that
    /// the media is immutable halfway through one.
    read_only: bool,
    stats: Arc<BlobStoreCounters>,
    // Positive durability proofs only. A directory is inserted after its
    // parent barrier succeeds; an OID is inserted only after the batch's leaf
    // barrier succeeds. Both caches are deliberately cold on every Store open
    // so a new process re-proves state left visible by a crashed peer.
    durable_dirs: Arc<Mutex<LruCache<PathBuf, ()>>>,
    durable_oids: Arc<Mutex<LruCache<ObjectId, ()>>>,
    dir_barrier: DirectoryBarrier,
    /// Present exactly when `dir_barrier` is [`DirectoryBarrier::Collapsed`].
    fs_barrier: Option<Arc<FsBarrier>>,
}

/// Checkin-scoped durable publication. Every object is file-fsynced before its
/// immutable final name is linked. Final shard-directory barriers are deferred
/// until `finish`; metadata CAS is allowed only after `finish` succeeds. A crash
/// before CAS may leave durable orphan objects, which is safe.
#[must_use = "staged blob bytes are not authenticated until finish() succeeds"]
pub struct StagedBlobReader {
    file: fs::File,
    id: ObjectId,
    hasher: blake3::Hasher,
    remaining: u64,
    payload_len: u64,
    object_len: u64,
    stats: Arc<BlobStoreCounters>,
}

impl StagedBlobReader {
    pub fn payload_len(&self) -> u64 {
        self.payload_len
    }

    /// Complete the I15 proof for bytes already copied to a staged sink.
    ///
    /// A caller must publish that sink only after this returns `Ok(())`. A
    /// mismatch is deliberately detected after the final payload byte so one
    /// pass over a large Blob is enough.
    pub fn finish(mut self) -> Result<()> {
        if self.remaining != 0 {
            return Err(Error::Corrupt(format!(
                "blob {} ended with {} payload bytes unread",
                self.id, self.remaining
            )));
        }
        let mut extra = [0u8; 1];
        if self.file.read(&mut extra)? != 0 {
            return Err(Error::Corrupt(format!(
                "blob {} grew while staged output was being built",
                self.id
            )));
        }
        let actual = ObjectId(*self.hasher.finalize().as_bytes());
        if actual != self.id {
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {}", self.id)));
        }
        self.stats
            .get_bytes
            .fetch_add(self.object_len, Ordering::Relaxed);
        Ok(())
    }
}

impl Read for StagedBlobReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let limit = usize::try_from(self.remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = self.file.read(&mut buf[..limit])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("blob {} payload truncated", self.id),
            ));
        }
        self.hasher.update(&buf[..n]);
        self.remaining -= n as u64;
        Ok(n)
    }
}

pub struct PublishBatch<'a> {
    store: &'a LocalBlobStore,
    dirs: BTreeSet<PathBuf>,
    /// Ancestor directories whose entry set this batch changed, deferred for
    /// the single barrier phase. Always empty under
    /// [`DirectoryBarrier::PerDirectory`], where the barrier is taken as the
    /// entry is created.
    path_dirs: BTreeSet<PathBuf>,
    /// Directories whose existence the `path_dirs` barriers prove. They enter
    /// the process-wide positive-proof cache only after that phase succeeds,
    /// for the same reason `oids` do.
    proofs: BTreeSet<PathBuf>,
    oids: BTreeSet<ObjectId>,
    new_puts: u64,
    new_put_bytes: u64,
    new_dedup: u64,
    new_dedup_bytes: u64,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        Self::with_directory_barrier(root, DirectoryBarrier::from_env())
    }

    /// Open with an explicit directory-barrier policy. Every policy publishes
    /// the same durable state; see [`DirectoryBarrier`].
    pub fn with_directory_barrier(root: PathBuf, dir_barrier: DirectoryBarrier) -> Result<Self> {
        let stats = Arc::new(BlobStoreCounters::default());
        let durable_dirs = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DURABLE_DIR_CACHE_CAPACITY).expect("non-zero directory cache"),
        )));
        let durable_oids = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(DURABLE_OID_CACHE_CAPACITY).expect("non-zero OID cache"),
        )));
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        ensure_dir_durable(&root, &objects, &stats, &durable_dirs)?;
        ensure_dir_durable(&root, &tmp, &stats, &durable_dirs)?;
        cleanup_stale_tmp(&tmp, &stats)?;
        let fs_barrier = match dir_barrier {
            DirectoryBarrier::Collapsed => FsBarrier::open(&root),
            DirectoryBarrier::PerDirectory | DirectoryBarrier::Deferred => None,
        };
        // A store that cannot hold a descriptor on its own filesystem, or is
        // on a platform without `syncfs`, falls back to the deferred phase of
        // ordinary directory barriers rather than discovering that on a
        // barrier path. That proves the same edges with the portable
        // primitive.
        let dir_barrier = if dir_barrier == DirectoryBarrier::Collapsed && fs_barrier.is_none() {
            DirectoryBarrier::Deferred
        } else {
            dir_barrier
        };
        Ok(Self {
            root,
            read_only: false,
            stats,
            durable_dirs,
            durable_oids,
            dir_barrier,
            fs_barrier,
        })
    }

    /// Open an existing object store without writing anything: no directory
    /// creation, no directory fsync barrier, and no stale-tmp reclamation.
    /// `objects/` must already exist, because nothing here may create it.
    pub fn open_read_only(root: PathBuf) -> Result<Self> {
        let objects = root.join("objects");
        if !objects.is_dir() {
            return Err(Error::Invalid(format!(
                "missing object directory {}",
                objects.display()
            )));
        }
        Ok(Self {
            root,
            read_only: true,
            stats: Arc::new(BlobStoreCounters::default()),
            durable_dirs: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(DURABLE_DIR_CACHE_CAPACITY).expect("non-zero directory cache"),
            ))),
            durable_oids: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(DURABLE_OID_CACHE_CAPACITY).expect("non-zero OID cache"),
            ))),
            // A read-only store publishes nothing, so it needs no barrier.
            dir_barrier: DirectoryBarrier::PerDirectory,
            fs_barrier: None,
        })
    }

    /// The policy this store actually settled on, after any platform or
    /// descriptor fallback.
    pub fn directory_barrier(&self) -> DirectoryBarrier {
        self.dir_barrier
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, id: ObjectId) -> PathBuf {
        let (a, b) = id.shard_dirs();
        self.root.join("objects").join(a).join(b).join(id.hex())
    }

    pub fn begin_batch(&self) -> PublishBatch<'_> {
        PublishBatch {
            store: self,
            dirs: BTreeSet::new(),
            path_dirs: BTreeSet::new(),
            proofs: BTreeSet::new(),
            oids: BTreeSet::new(),
            new_puts: 0,
            new_put_bytes: 0,
            new_dedup: 0,
            new_dedup_bytes: 0,
        }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.put_parts(&[bytes])
    }

    /// Single-object form of [`PublishBatch::put_parts`].
    pub fn put_parts(&self, parts: &[&[u8]]) -> Result<ObjectId> {
        let mut batch = self.begin_batch();
        let id = batch.put_parts(parts)?;
        batch.finish()?;
        Ok(id)
    }

    fn verify_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
        let mut file = fs::File::open(path)?;
        verify_object_stream(id, &mut file)
    }

    fn verify_and_sync_existing(&self, id: ObjectId, path: &Path) -> Result<()> {
        require_regular_file(path, id)?;
        // Verify and force the same descriptor. Streaming removes the
        // object-sized allocation without weakening the I3/I4 proof boundary.
        // Opening writable is intentional: macOS F_FULLFSYNC is a fail-closed
        // durability contract, not a best-effort read hint.
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        verify_object_stream(id, &mut file)?;
        sync_file_counted(
            &file,
            &self.stats,
            crate::DurabilityBarrier::ObjectExistingFile,
        )?;
        Ok(())
    }

    fn oid_is_durable(&self, id: ObjectId) -> bool {
        self.durable_oids.lock().get(&id).is_some()
    }

    pub fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        let p = self.object_path(id);
        require_regular_file(&p, id)?;
        let bytes = fs::read(&p).map_err(|_| Error::NotFound(format!("object {id}")))?;
        if hash_bytes(&bytes) != id {
            // Counted before the refusal, so a store that is corrupting reads
            // is visible in `forge stats` and not only in whichever command
            // happened to touch the object first.
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        self.stats
            .get_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    /// Verify one Blob's durable identity and canonical v1 frame without
    /// materializing its payload. This is the typed-graph trust check used by
    /// `Store::intro_walk` (I1/I15).
    pub fn verify_blob(&self, id: ObjectId) -> Result<()> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file =
            fs::File::open(&path).map_err(|_| Error::NotFound(format!("object {id}")))?;

        let actual = hash_reader(&mut file)?;
        let len = file.stream_position()?;
        if actual != id {
            self.stats.hash_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Error::Corrupt(format!("hash mismatch {id}")));
        }
        self.stats.get_bytes.fetch_add(len, Ordering::Relaxed);
        verify_blob_frame(&mut file, len)?;
        Ok(())
    }

    /// Open a Blob payload for a sink that is itself staged and unpublished.
    ///
    /// The canonical frame and payload length are checked up front, but payload
    /// authentication completes only in [`StagedBlobReader::finish`]. This is
    /// intentionally *not* the API for stdout, sockets, or any irreversible
    /// sink: callers must discard their staged output when `finish` fails.
    pub fn open_blob_payload_for_staged_output(&self, id: ObjectId) -> Result<StagedBlobReader> {
        let path = self.object_path(id);
        require_regular_file(&path, id)?;
        let mut file =
            fs::File::open(&path).map_err(|_| Error::NotFound(format!("object {id}")))?;
        let object_len = file.metadata()?.len();
        let (prefix, payload_len) = verify_blob_frame(&mut file, object_len)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&prefix);
        Ok(StagedBlobReader {
            file,
            id,
            hasher,
            remaining: payload_len,
            payload_len,
            object_len,
            stats: Arc::clone(&self.stats),
        })
    }

    pub fn has(&self, id: ObjectId) -> bool {
        self.object_path(id).exists()
    }

    /// Take the reclamation lock exclusively for the lifetime of the guard.
    ///
    /// A sweep holds this while it reads a candidate's age and unlinks it, so
    /// no deduplicating put can slip between the two. See
    /// [`refresh_dedup_mtime`].
    pub fn gc_exclusive(&self) -> Result<GcObjectGuard> {
        let file = open_gc_lock(&self.root)?;
        flock(&file, libc::LOCK_EX, "cannot take the reclamation lock")?;
        Ok(GcObjectGuard { _file: file })
    }

    pub fn stats(&self) -> BlobStoreStats {
        let fsync_file = self.stats.fsync_file.snapshot();
        let fsync_dir = self.stats.fsync_dir.snapshot();
        let barrier_fs = self.stats.barrier_fs.snapshot();
        BlobStoreStats {
            puts: self.stats.puts.load(Ordering::Relaxed),
            dedup_hits: self.stats.dedup_hits.load(Ordering::Relaxed),
            fsync_file: fsync_file.count,
            fsync_file_us: fsync_file.total_us,
            fsync_dir: fsync_dir.count,
            fsync_dir_us: fsync_dir.total_us,
            barrier_fs: barrier_fs.count,
            barrier_fs_us: barrier_fs.total_us,
            barrier_fs_batches: self.stats.barrier_fs_batches.load(Ordering::Relaxed),
            put_bytes: self.stats.put_bytes.load(Ordering::Relaxed),
            dedup_bytes: self.stats.dedup_bytes.load(Ordering::Relaxed),
            get_bytes: self.stats.get_bytes.load(Ordering::Relaxed),
            hash_failures: self.stats.hash_failures.load(Ordering::Relaxed),
        }
    }
}

impl PublishBatch<'_> {
    pub fn put(&mut self, bytes: &[u8]) -> Result<ObjectId> {
        self.put_parts(&[bytes])
    }

    /// Make `child` exist under `parent` and arrange for the barrier that
    /// proves that entry.
    ///
    /// Under [`DirectoryBarrier::PerDirectory`], the default, the barrier is
    /// taken here, as it always was. Under [`DirectoryBarrier::Deferred`] it
    /// waits for the single phase before `finish` returns, and under
    /// [`DirectoryBarrier::Collapsed`] for the one filesystem-wide barrier
    /// that covers it along with every other edge the batch touched.
    /// Deferral cannot weaken I4 because
    /// nothing between here and `finish` may publish a ref, and the positive
    /// proof that lets a *later* batch skip the barrier is likewise recorded
    /// only after `finish` succeeds.
    fn ensure_path_dir(&mut self, parent: &Path, child: &Path) -> Result<()> {
        if self.store.durable_dirs.lock().get(child).is_some() {
            return Ok(());
        }
        ensure_dir_present(child)?;
        if self.store.dir_barrier.defers() {
            self.path_dirs.insert(parent.to_path_buf());
            self.proofs.insert(child.to_path_buf());
        } else {
            sync_dir_counted(
                parent,
                &self.store.stats,
                crate::DurabilityBarrier::ObjectPathDirectory,
            )?;
            self.store.durable_dirs.lock().put(child.to_path_buf(), ());
        }
        Ok(())
    }

    /// Publish the object file formed by concatenating `parts`. Identity,
    /// bytes, dedup and every durability barrier are exactly those of
    /// `put(&parts.concat())`; the concatenation is never allocated, so a
    /// caller that already holds a payload does not need a second copy of it
    /// in order to publish one (I2, I3, I4).
    /// The one place a publication outcome is counted.
    ///
    /// Outcome and byte volume move together here so a new branch in
    /// `put_parts` -- and it has six -- cannot record one and forget the
    /// other. That is the standard #311 established for `txn_count`: source
    /// the count where the work happens, not where someone remembers to.
    fn count_put(&mut self, bytes: u64) {
        self.new_puts += 1;
        self.new_put_bytes = self.new_put_bytes.saturating_add(bytes);
    }

    /// A publication satisfied by bytes that were already durable. `bytes` is
    /// what was NOT written, which is the storage-amplification signal.
    fn count_dedup(&mut self, bytes: u64) {
        self.new_dedup += 1;
        self.new_dedup_bytes = self.new_dedup_bytes.saturating_add(bytes);
    }

    pub fn put_parts(&mut self, parts: &[&[u8]]) -> Result<ObjectId> {
        // Every object write in the process funnels through here, so this is
        // the whole write boundary for a read-only store.
        if self.store.read_only {
            return Err(Error::Denied(
                "repository is open read-only; objects cannot be published".into(),
            ));
        }
        let payload_bytes: u64 = parts.iter().map(|p| p.len() as u64).sum();
        let id = hash_parts(parts);
        let dest = self.store.object_path(id);
        let (a, b) = id.shard_dirs();
        let objects = self.store.root.join("objects");
        let shard_a = objects.join(a);
        let shard_b = shard_a.join(b);

        // Prove the complete pathname before trusting either an existing link
        // or a link we are about to publish. A cold Store cannot inherit a
        // crashed or older VERSION=1 process's unforced shard ancestors.
        self.ensure_path_dir(&objects, &shard_a)?;
        self.ensure_path_dir(&shard_a, &shard_b)?;

        // The age refresh comes first, because it is what decides whether this
        // is a dedup at all: an object a sweep unlinked while we were looking
        // at it is not "already here", and naming it would be the corruption
        // this whole mechanism exists to prevent.
        if dest.exists() && refresh_dedup_mtime(&self.store.root, &dest)? == Refreshed::Yes {
            if self.store.oid_is_durable(id) {
                // Rehash at every trust boundary even when a process-local
                // durability proof lets us avoid another physical barrier.
                self.store.verify_existing(id, &dest)?;
                self.count_dedup(payload_bytes);
                return Ok(id);
            }
            self.store.verify_and_sync_existing(id, &dest)?;
            // The visible link may have been published by another batch that
            // has not yet synced this directory, or by a legacy process whose
            // file/ancestor barriers were weaker. Reproduce the entire proof
            // before allowing our caller to publish metadata that names it.
            self.dirs.insert(shard_b);
            self.oids.insert(id);
            self.count_dedup(payload_bytes);
            return Ok(id);
        }

        let tmp_dir = self.store.root.join("tmp");
        let tmp = tmp_dir.join(ulid::Ulid::new().to_string());
        {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            for part in parts {
                f.write_all(part)?;
            }
            sync_file_counted(&f, &self.store.stats, crate::DurabilityBarrier::ObjectFile)?;
            crate::inject_barrier_failure(crate::DurabilityBarrier::ObjectFileAfter)?;
        }

        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                crate::inject_barrier_failure(crate::DurabilityBarrier::ObjectLinkAfter)?;
                self.dirs.insert(shard_b);
                self.oids.insert(id);
                self.count_put(payload_bytes);
                let _ = fs::remove_file(&tmp);
                Ok(id)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || dest.exists() => {
                if refresh_dedup_mtime(&self.store.root, &dest)? == Refreshed::Yes {
                    let _ = fs::remove_file(&tmp);
                    if !self.store.oid_is_durable(id) {
                        self.store.verify_and_sync_existing(id, &dest)?;
                        self.dirs.insert(shard_b);
                        self.oids.insert(id);
                    } else {
                        self.store.verify_existing(id, &dest)?;
                    }
                    self.count_dedup(payload_bytes);
                    // Another publisher won after our existence check. Its file
                    // and directory barriers may still be pending, so this batch
                    // joins the proof unless another completed batch cached it.
                    return Ok(id);
                }
                // A sweep unlinked the object between our failed link and our
                // refresh. Our tmp file still holds the exact bytes, so link
                // them back rather than naming something that is not there.
                if fs::hard_link(&tmp, &dest).is_ok() {
                    self.count_put(payload_bytes);
                } else {
                    self.store.verify_and_sync_existing(id, &dest)?;
                    self.count_dedup(payload_bytes);
                }
                self.dirs.insert(shard_b);
                self.oids.insert(id);
                let _ = fs::remove_file(&tmp);
                Ok(id)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(Error::Io(e.to_string()))
            }
        }
    }

    pub fn finish(self) -> Result<()> {
        let PublishBatch {
            store,
            dirs,
            path_dirs,
            proofs,
            oids,
            new_puts,
            new_put_bytes,
            new_dedup,
            new_dedup_bytes,
        } = self;

        // One filesystem-wide barrier is only the cheaper proof when it
        // replaces more than one directory barrier. A batch that touched a
        // single directory takes the ordinary `fsync` on it.
        let collapse = store.dir_barrier == DirectoryBarrier::Collapsed
            && store.fs_barrier.is_some()
            && path_dirs.len() + dirs.len() > 1;
        if collapse {
            // The collapsed barrier stands in for exactly these per-directory
            // barriers, so it presents the same failpoints, once per edge it
            // subsumes and in the same order. An armed fault therefore still
            // refuses the batch before any ref can move.
            for _ in &path_dirs {
                crate::inject_barrier_failure(crate::DurabilityBarrier::ObjectPathDirectory)?;
            }
            for _ in &dirs {
                crate::inject_barrier_failure(
                    crate::DurabilityBarrier::ObjectPublicationDirectory,
                )?;
            }
            let barrier = store.fs_barrier.as_ref().expect("collapse implies barrier");
            barrier.wait(&store.stats)?;
            crate::inject_barrier_failure(
                crate::DurabilityBarrier::ObjectPublicationDirectoryAfter,
            )?;
        } else {
            // `path_dirs` is empty under `PerDirectory`; its barriers were
            // taken as each entry was created.
            for dir in &path_dirs {
                sync_dir_counted(
                    dir,
                    &store.stats,
                    crate::DurabilityBarrier::ObjectPathDirectory,
                )?;
            }
            for dir in &dirs {
                sync_dir_counted(
                    dir,
                    &store.stats,
                    crate::DurabilityBarrier::ObjectPublicationDirectory,
                )?;
                crate::inject_barrier_failure(
                    crate::DurabilityBarrier::ObjectPublicationDirectoryAfter,
                )?;
            }
        }
        // This is the sole proof publication point. A dropped or failed batch
        // never teaches later callers that its visible links or its shard
        // ancestors are durable.
        {
            let mut durable_dirs = store.durable_dirs.lock();
            for dir in proofs {
                durable_dirs.put(dir, ());
            }
        }
        {
            let mut durable_oids = store.durable_oids.lock();
            for id in oids {
                durable_oids.put(id, ());
            }
        }
        store.stats.puts.fetch_add(new_puts, Ordering::Relaxed);
        store
            .stats
            .put_bytes
            .fetch_add(new_put_bytes, Ordering::Relaxed);
        store
            .stats
            .dedup_hits
            .fetch_add(new_dedup, Ordering::Relaxed);
        store
            .stats
            .dedup_bytes
            .fetch_add(new_dedup_bytes, Ordering::Relaxed);
        Ok(())
    }
}

/// `LocalBlobStore` is the sole production implementation of the seam. The
/// inherent methods stay for direct local callers and these delegate to them,
/// so the two cannot drift apart.
impl crate::ObjectBatch for PublishBatch<'_> {
    fn put_parts(&mut self, parts: &[&[u8]]) -> Result<ObjectId> {
        PublishBatch::put_parts(self, parts)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<ObjectId> {
        PublishBatch::put(self, bytes)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        PublishBatch::finish(*self)
    }
}

impl crate::ObjectStore for LocalBlobStore {
    fn durability_class(&self) -> crate::DurabilityClass {
        // Every publication path below ends in the strongest platform barrier
        // on the object file and on every directory edge that names it. The
        // crash and cross-process evidence backing this claim is named in
        // `objectstore.rs`; a future backend owes the equivalent.
        crate::DurabilityClass::CrashDurable
    }

    fn begin_batch(&self) -> Box<dyn crate::ObjectBatch + '_> {
        Box::new(LocalBlobStore::begin_batch(self))
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        LocalBlobStore::get(self, id)
    }

    fn verify_blob(&self, id: ObjectId) -> Result<()> {
        LocalBlobStore::verify_blob(self, id)
    }

    fn has(&self, id: ObjectId) -> bool {
        LocalBlobStore::has(self, id)
    }

    fn read_only(&self) -> bool {
        LocalBlobStore::read_only(self)
    }

    fn stats(&self) -> BlobStoreStats {
        LocalBlobStore::stats(self)
    }

    fn put_parts(&self, parts: &[&[u8]]) -> Result<ObjectId> {
        LocalBlobStore::put_parts(self, parts)
    }

    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        LocalBlobStore::put(self, bytes)
    }
}

impl<O: crate::ObjectStore> crate::Store<O> {
    pub fn stats(&self) -> BlobStoreStats {
        crate::ObjectStore::stats(&self.blobs)
    }
}

/// Validate the complete v1 Blob frame without reading or allocating the
/// payload. On success the descriptor is positioned at the first payload byte.
fn verify_blob_frame(file: &mut fs::File, object_len: u64) -> Result<(Vec<u8>, u64)> {
    if object_len < 5 {
        return Err(Error::Corrupt("object file too short".into()));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut frame = [0u8; 5];
    file.read_exact(&mut frame)?;
    let ty = forge_types::ObjectType::from_u8(frame[0])?;
    if ty != forge_types::ObjectType::Blob {
        return Err(Error::Corrupt("not a blob".into()));
    }

    let header_len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as u64;
    let header_end = 5u64
        .checked_add(header_len)
        .ok_or_else(|| Error::Corrupt("object header length overflow".into()))?;
    if header_end > object_len {
        return Err(Error::Corrupt("object header truncated".into()));
    }
    let payload_len = object_len - header_end;
    let expected = blob_frame_prefix(payload_len);
    if expected.len() as u64 != header_end {
        return Err(Error::Corrupt("non-canonical blob header".into()));
    }

    file.seek(SeekFrom::Start(0))?;
    let mut observed = vec![0u8; expected.len()];
    file.read_exact(&mut observed)?;
    if observed != expected {
        return Err(Error::Corrupt("invalid blob header".into()));
    }
    Ok((expected, payload_len))
}

/// Re-prove an existing object's identity with memory independent of its size.
/// The caller owns descriptor choice: the durability path passes the exact
/// descriptor it will subsequently force, so verified bytes and synced bytes
/// cannot diverge through a pathname reopen.
fn verify_object_stream(id: ObjectId, reader: &mut impl Read) -> Result<()> {
    let actual = hash_reader(reader)?;
    if actual != id {
        return Err(Error::Corrupt(format!(
            "existing object does not match its id: {id}"
        )));
    }
    Ok(())
}

/// A durable object must be a regular file. Without this check a FIFO planted
/// at an object path made `fs::read` block forever, so fsck/export/import hung
/// indefinitely instead of failing closed -- while a mere byte flip in the same
/// file was correctly reported as corruption.
fn require_regular_file(path: &Path, id: ObjectId) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(()),
        Ok(meta) => Err(Error::Corrupt(format!(
            "object {id} is not a regular file: {:?}",
            meta.file_type()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::NotFound(format!("object {id}")))
        }
        Err(e) => Err(Error::Io(e.to_string())),
    }
}

/// Make `child` exist and be a directory, taking no barrier. Existence is
/// never a durability proof: the caller still owes the parent barrier that
/// proves the entry before any ref may name an object beneath it.
fn ensure_dir_present(child: &Path) -> Result<()> {
    match fs::create_dir(child) {
        Ok(()) => Ok(()),
        // Existence is not proof that another process durably published the
        // directory entry. The caller reproduces the parent barrier anyway.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(child)?;
            if !metadata.file_type().is_dir() {
                return Err(Error::Invalid(format!(
                    "object-store path is not a directory: {}",
                    child.display()
                )));
            }
            Ok(())
        }
        Err(e) => Err(Error::Io(e.to_string())),
    }
}

fn ensure_dir_durable(
    parent: &Path,
    child: &Path,
    stats: &BlobStoreCounters,
    durable_dirs: &Mutex<LruCache<PathBuf, ()>>,
) -> Result<()> {
    if durable_dirs.lock().get(child).is_some() {
        return Ok(());
    }
    ensure_dir_present(child)?;
    sync_dir_counted(parent, stats, crate::DurabilityBarrier::ObjectPathDirectory)?;
    durable_dirs.lock().put(child.to_path_buf(), ());
    Ok(())
}

/// Reclaim only crash debris old enough that it cannot plausibly be an active
/// publication. Every Forge temp file is named by its creation-time ULID.
/// Eagerly deleting all temp files on every Store::open races other processes.
fn cleanup_stale_tmp(tmp: &Path, stats: &BlobStoreCounters) -> Result<()> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Internal(format!("clock before unix epoch: {e}")))?
        .as_millis() as u64;
    let mut removed = false;
    for entry in fs::read_dir(tmp)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if !(ty.is_file() || ty.is_symlink()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = ulid::Ulid::from_string(&name) else {
            continue;
        };
        if now_ms.saturating_sub(id.timestamp_ms()) < STALE_TMP_MS {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    if removed {
        sync_dir_counted(
            tmp,
            stats,
            crate::DurabilityBarrier::ObjectTemporaryDirectory,
        )?;
    }
    Ok(())
}

fn sync_dir_counted(
    path: &Path,
    stats: &BlobStoreCounters,
    point: crate::DurabilityBarrier,
) -> Result<()> {
    let started = Instant::now();
    crate::durable_sync_dir_at(path, point)?;
    stats.fsync_dir.observe(started.elapsed());
    Ok(())
}

fn sync_file_counted(
    file: &std::fs::File,
    stats: &BlobStoreCounters,
    point: crate::DurabilityBarrier,
) -> Result<()> {
    let started = Instant::now();
    crate::durable_sync_file_at(file, point)?;
    stats.fsync_file.observe(started.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    /// Barrier-accounting tests name the policy they are counting. The
    /// per-directory proof obligations below are the portable contract and
    /// are asserted exactly as they always were; the collapsed policy has its
    /// own tests, because one barrier there proves what several prove here.
    fn per_dir_store(root: &Path) -> LocalBlobStore {
        LocalBlobStore::with_directory_barrier(root.to_path_buf(), DirectoryBarrier::PerDirectory)
            .unwrap()
    }

    fn deferred_store(root: &Path) -> LocalBlobStore {
        LocalBlobStore::with_directory_barrier(root.to_path_buf(), DirectoryBarrier::Deferred)
            .unwrap()
    }

    /// `None` when this platform has no filesystem-wide barrier, so the
    /// collapsed-policy tests skip instead of asserting a fallback.
    fn collapsed_store(root: &Path) -> Option<LocalBlobStore> {
        let store =
            LocalBlobStore::with_directory_barrier(root.to_path_buf(), DirectoryBarrier::Collapsed)
                .unwrap();
        (store.directory_barrier() == DirectoryBarrier::Collapsed).then_some(store)
    }

    #[test]
    fn put_get_idempotent() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let a = s.put(b"abc").unwrap();
        let b = s.put(b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.get(a).unwrap(), b"abc");
        assert_eq!(
            std::fs::read_dir(d.path().join("objects")).unwrap().count(),
            1
        );
    }

    #[test]
    fn startup_reclaims_old_tmp_but_never_fresh_tmp() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("tmp")).unwrap();
        let stale = d.path().join("tmp").join(ulid::Ulid::nil().to_string());
        let fresh = d.path().join("tmp").join(ulid::Ulid::new().to_string());
        fs::write(&stale, b"crash debris").unwrap();
        fs::write(&fresh, b"live publication").unwrap();

        let _s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn concurrent_same_object_writers_converge() {
        let d = tempdir().unwrap();
        let s = Arc::new(LocalBlobStore::new(d.path().to_path_buf()).unwrap());
        let mut joins = Vec::new();
        for _ in 0..32 {
            let s = s.clone();
            joins.push(thread::spawn(move || {
                s.put(b"same durable object").unwrap()
            }));
        }
        let ids: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(s.get(ids[0]).unwrap(), b"same durable object");
    }

    #[test]
    fn published_object_survives_store_reopen() {
        let d = tempdir().unwrap();
        let id = {
            let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
            s.put(b"durable").unwrap()
        };
        let reopened = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(id).unwrap(), b"durable");
    }

    #[test]
    fn stats_count_new_durable_publications_not_dedup_hits() {
        let d = tempdir().unwrap();
        let s = per_dir_store(d.path());
        let before = s.stats();
        let id = s.put(b"measured").unwrap();
        let after = s.stats();
        assert_eq!(after.puts, before.puts + 1);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert!(after.fsync_dir > before.fsync_dir);
        s.put(b"measured").unwrap();
        let after_dedup = s.stats();
        // A dedup hit republishes nothing and performs no new barrier, so the
        // dedup PAIR is all that may move: the outcome and the bytes it did not
        // have to write. Every put field and every barrier field must be
        // untouched, which is what the struct update below states.
        assert_eq!(
            after_dedup,
            BlobStoreStats {
                dedup_hits: after.dedup_hits + 1,
                dedup_bytes: after.dedup_bytes + b"measured".len() as u64,
                ..after
            }
        );
        assert_eq!(s.get(id).unwrap(), b"measured");
    }

    #[test]
    fn existing_directory_reproduces_a_possible_crashed_creators_parent_barrier() {
        let d = tempdir().unwrap();
        let parent = d.path().join("parent");
        let child = parent.join("visible-but-not-proven-durable");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&child).unwrap();
        let stats = BlobStoreCounters::default();
        let durable_dirs = Mutex::new(LruCache::new(NonZeroUsize::new(8).unwrap()));

        ensure_dir_durable(&parent, &child, &stats, &durable_dirs).unwrap();

        assert_eq!(stats.fsync_dir.snapshot().count, 1);
        ensure_dir_durable(&parent, &child, &stats, &durable_dirs).unwrap();
        assert_eq!(
            stats.fsync_dir.snapshot().count,
            1,
            "a successful positive proof is reusable within one Store"
        );
    }

    #[test]
    fn dedup_batch_reproduces_an_unfinished_publishers_directory_barrier() {
        let d = tempdir().unwrap();
        let s = per_dir_store(d.path());
        let mut first = s.begin_batch();
        let id = first.put(b"race-safe durable object").unwrap();
        let before = s.stats();

        let mut second = s.begin_batch();
        assert_eq!(second.put(b"race-safe durable object").unwrap(), id);
        let joined = s.stats();
        assert_eq!(joined.puts, before.puts);
        assert_eq!(joined.fsync_file, before.fsync_file + 1);
        assert_eq!(
            joined.fsync_dir, before.fsync_dir,
            "the pathname barrier is deferred to batch finish"
        );

        // Model the winning publisher dying after link(2) but before finish.
        // The deduplicating batch must still make the shared directory entry
        // durable before its caller is allowed to publish a ref.
        drop(first);
        second.finish().unwrap();
        let after = s.stats();
        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 1);
        assert_eq!(s.get(id).unwrap(), b"race-safe durable object");
    }

    #[test]
    fn cold_store_dedup_reproves_file_and_full_path() {
        let d = tempdir().unwrap();
        let first = per_dir_store(d.path());
        let mut unfinished = first.begin_batch();
        let id = unfinished.put(b"cross-process durability join").unwrap();

        // A newly opened Store has no inherited proof cache. Model a second
        // process joining the visible link after the first dies before finish.
        let second = per_dir_store(d.path());
        let before = second.stats();
        let mut joining = second.begin_batch();
        assert_eq!(joining.put(b"cross-process durability join").unwrap(), id);
        drop(unfinished);
        joining.finish().unwrap();
        let after = second.stats();

        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(
            after.fsync_dir,
            before.fsync_dir + 3,
            "objects->aa, aa->bb, and bb->OID must all be re-proved"
        );
    }

    fn same_shard_pair() -> (Vec<u8>, Vec<u8>) {
        let mut seen = std::collections::BTreeMap::new();
        for i in 0..200_000u64 {
            let bytes = format!("same-shard-{i}").into_bytes();
            let id = hash_bytes(&bytes);
            let shard = id.shard_dirs();
            if let Some(first) = seen.insert(shard, bytes.clone()) {
                if first != bytes {
                    return (first, bytes);
                }
            }
        }
        panic!("failed to find deterministic shard collision");
    }

    #[test]
    fn batch_coalesces_same_shard_directory_barrier() {
        let d = tempdir().unwrap();
        let (a, b) = same_shard_pair();

        let serial_root = d.path().join("serial");
        std::fs::create_dir(&serial_root).unwrap();
        let serial = per_dir_store(&serial_root);
        let serial_before = serial.stats();
        serial.put(&a).unwrap();
        serial.put(&b).unwrap();
        let serial_after = serial.stats();
        let serial_dirs = serial_after.fsync_dir - serial_before.fsync_dir;

        let batched_root = d.path().join("batched");
        std::fs::create_dir(&batched_root).unwrap();
        let batched = per_dir_store(&batched_root);
        let batch_before = batched.stats();
        let mut batch = batched.begin_batch();
        batch.put(&a).unwrap();
        batch.put(&b).unwrap();
        let mid = batched.stats();
        assert_eq!(mid.puts, batch_before.puts);
        batch.finish().unwrap();
        let batch_after = batched.stats();
        let batch_dirs = batch_after.fsync_dir - batch_before.fsync_dir;

        assert_eq!(batch_after.puts - batch_before.puts, 2);
        assert_eq!(batch_after.fsync_file - batch_before.fsync_file, 2);
        assert_eq!(serial_dirs, batch_dirs + 1);
    }

    #[test]
    fn warm_same_shard_put_pays_only_the_new_leaf_barrier() {
        let d = tempdir().unwrap();
        let (a, b) = same_shard_pair();
        let s = per_dir_store(d.path());
        s.put(&a).unwrap();
        let before = s.stats();

        s.put(&b).unwrap();
        let after = s.stats();

        assert_eq!(after.puts, before.puts + 1);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 1);
    }

    #[test]
    fn legacy_visible_object_reproves_uncached_ancestors() {
        let d = tempdir().unwrap();
        let s = per_dir_store(d.path());
        let bytes = b"legacy visible object";
        let id = hash_bytes(bytes);
        let dest = s.object_path(id);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, bytes).unwrap();
        let before = s.stats();

        assert_eq!(s.put(bytes).unwrap(), id);
        let after = s.stats();

        assert_eq!(after.puts, before.puts);
        assert_eq!(after.fsync_file, before.fsync_file + 1);
        assert_eq!(after.fsync_dir, before.fsync_dir + 3);
    }

    /// A byte flip at an object path is corruption; so is a FIFO. Before the
    /// file-type check, fs::read on a FIFO blocked forever, so fsck/export/import
    /// hung indefinitely instead of failing closed.
    #[test]
    #[cfg(unix)]
    fn non_regular_file_at_an_object_path_is_corruption_not_a_hang() {
        use std::os::unix::fs::FileTypeExt;

        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let id = s.put(b"durable").unwrap();
        let path = s.object_path(id);
        fs::remove_file(&path).unwrap();

        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());

        // Would block forever before the check. Corrupt, promptly.
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
        assert!(matches!(s.put(b"durable"), Err(Error::Corrupt(_))));

        // A directory in an object's place is equally not an object.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
    }

    /// The default policy's proof obligation, and the reason the positive
    /// proof cache is published in `finish` and nowhere else (I4).
    ///
    /// A batch that dies after `mkdir` and `link` made a whole shard path
    /// *visible*, but before any barrier proved it, must teach a later batch
    /// nothing. If the deferred proof were recorded when the directory was
    /// created, the next batch would skip the barrier for an edge that was
    /// never durable -- and its caller would then CAS a ref onto an object a
    /// crash can still lose. That is exactly the I4 violation deferral must
    /// not introduce, and it is invisible to any test that only kills a
    /// process with the page cache intact.
    #[test]
    fn deferred_unfinished_batch_publishes_no_directory_proof() {
        let d = tempdir().unwrap();
        let s = deferred_store(d.path());

        let mut abandoned = s.begin_batch();
        abandoned.put(b"visible but never proven").unwrap();
        drop(abandoned);
        let before = s.stats();

        let mut second = s.begin_batch();
        second.put(b"visible but never proven").unwrap();
        second.finish().unwrap();
        let after = s.stats();

        assert_eq!(
            after.fsync_dir - before.fsync_dir,
            3,
            "objects->aa, aa->bb and bb->OID must all be proved by the batch \
             that is about to let a ref name the object"
        );
    }

    /// The deferred phase proves the same edges as `PerDirectory`, only later
    /// and deduplicated: nothing is durable before `finish`, everything is
    /// after it.
    #[test]
    fn deferred_takes_every_directory_barrier_in_one_phase_at_finish() {
        let d = tempdir().unwrap();
        let s = deferred_store(d.path());
        let before = s.stats();

        let mut batch = s.begin_batch();
        let a = batch.put(b"deferred one").unwrap();
        let b = batch.put(b"deferred two").unwrap();
        let mid = s.stats();
        assert_eq!(
            mid.directory_barriers(),
            before.directory_barriers(),
            "no directory edge may be proved before the batch is complete"
        );

        batch.finish().unwrap();
        let after = s.stats();
        assert_eq!(after.fsync_file, before.fsync_file + 2);
        assert!(
            after.fsync_dir > before.fsync_dir,
            "the phase proves every edge it deferred"
        );
        let reopened = LocalBlobStore::open_read_only(d.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(a).unwrap(), b"deferred one");
        assert_eq!(reopened.get(b).unwrap(), b"deferred two");
    }

    /// I4 under `Collapsed`: one filesystem-wide barrier stands in for every
    /// directory edge the batch touched -- the two shard ancestors and the
    /// leaf that names each object -- and the batch is acknowledged only
    /// after it returns.
    #[test]
    fn collapsed_batch_takes_one_barrier_for_every_edge_it_touched() {
        let d = tempdir().unwrap();
        let Some(s) = collapsed_store(d.path()) else {
            return;
        };
        let before = s.stats();

        let mut batch = s.begin_batch();
        let a = batch.put(b"collapsed one").unwrap();
        let b = batch.put(b"collapsed two").unwrap();
        let c = batch.put(b"collapsed three").unwrap();
        let mid = s.stats();
        assert_eq!(
            mid.directory_barriers(),
            before.directory_barriers(),
            "no directory edge may be proved before the batch is complete"
        );
        batch.finish().unwrap();
        let after = s.stats();

        assert_eq!(after.puts, before.puts + 3);
        assert_eq!(after.fsync_file, before.fsync_file + 3);
        assert_eq!(
            after.fsync_dir, before.fsync_dir,
            "the collapsed policy takes no per-directory barrier"
        );
        assert_eq!(
            after.barrier_fs,
            before.barrier_fs + 1,
            "three objects across three shard paths cost one barrier"
        );
        assert_eq!(after.barrier_fs_batches, before.barrier_fs_batches + 1);
        for id in [a, b, c] {
            let reopened = LocalBlobStore::open_read_only(d.path().to_path_buf()).unwrap();
            assert!(reopened.get(id).is_ok());
        }
    }

    /// A single touched directory is cheaper to prove with the ordinary
    /// `fsync` than with a whole-filesystem barrier, and the collapsed policy
    /// says so.
    #[test]
    fn collapsed_policy_still_uses_a_plain_barrier_for_one_directory() {
        let d = tempdir().unwrap();
        let Some(s) = collapsed_store(d.path()) else {
            return;
        };
        let (a, b) = same_shard_pair();
        s.put(&a).unwrap();
        let before = s.stats();

        s.put(&b).unwrap();
        let after = s.stats();

        assert_eq!(after.fsync_dir, before.fsync_dir + 1);
        assert_eq!(after.barrier_fs, before.barrier_fs);
    }

    /// The positive proofs are published only after the barrier, so a batch
    /// that dies before `finish` teaches a later batch nothing -- for shard
    /// ancestors exactly as for OIDs.
    #[test]
    fn collapsed_unfinished_batch_publishes_no_directory_proof() {
        let d = tempdir().unwrap();
        let Some(s) = collapsed_store(d.path()) else {
            return;
        };
        let mut abandoned = s.begin_batch();
        abandoned.put(b"never acknowledged").unwrap();
        drop(abandoned);
        let before = s.stats();

        // The shard ancestors this object created are visible but unproven.
        // A second batch that lands in the same shards must barrier again.
        let mut second = s.begin_batch();
        assert_eq!(
            second.put(b"never acknowledged").unwrap(),
            hash_bytes(b"never acknowledged")
        );
        second.finish().unwrap();
        let after = s.stats();

        assert_eq!(after.barrier_fs, before.barrier_fs + 1);
    }

    /// A cold store joining another publisher's visible-but-unproven link
    /// must reproduce the file barrier and the whole pathname before its
    /// caller may name the object from a ref (I4).
    #[test]
    fn collapsed_cold_store_dedup_reproves_file_and_full_path() {
        let d = tempdir().unwrap();
        let Some(first) = collapsed_store(d.path()) else {
            return;
        };
        let mut unfinished = first.begin_batch();
        let id = unfinished.put(b"cross-process durability join").unwrap();

        let second = collapsed_store(d.path()).unwrap();
        let before = second.stats();
        let mut joining = second.begin_batch();
        assert_eq!(joining.put(b"cross-process durability join").unwrap(), id);
        drop(unfinished);
        joining.finish().unwrap();
        let after = second.stats();

        assert_eq!(after.puts, before.puts);
        assert_eq!(
            after.fsync_file,
            before.fsync_file + 1,
            "the joined object's bytes are re-forced"
        );
        assert_eq!(
            after.barrier_fs,
            before.barrier_fs + 1,
            "objects->aa, aa->bb and bb->OID are all re-proved, in one barrier"
        );
        assert_eq!(second.get(id).unwrap(), b"cross-process durability join");
    }

    /// Concurrent batches share one barrier. The accounting identity is the
    /// assertion: every batch is accounted, and no batch is acknowledged by
    /// fewer than one completed barrier.
    #[test]
    fn collapsed_barrier_is_shared_across_concurrent_batches() {
        let d = tempdir().unwrap();
        let Some(s) = collapsed_store(d.path()) else {
            return;
        };
        let s = Arc::new(s);
        let writers = 8usize;
        let start = Arc::new(std::sync::Barrier::new(writers));
        let mut joins = Vec::new();
        for i in 0..writers {
            let s = s.clone();
            let start = start.clone();
            joins.push(thread::spawn(move || {
                let mut batch = s.begin_batch();
                batch
                    .put(format!("shared barrier {i} a").as_bytes())
                    .unwrap();
                batch
                    .put(format!("shared barrier {i} b").as_bytes())
                    .unwrap();
                start.wait();
                batch.finish().unwrap();
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        let after = s.stats();
        assert_eq!(after.barrier_fs_batches, writers as u64);
        assert!(after.barrier_fs >= 1);
        assert!(
            after.barrier_fs <= writers as u64,
            "a batch never runs more than one barrier: {after:?}"
        );
    }

    /// The gate's ordering rule, stated directly: a barrier that started
    /// before a caller asked for one never satisfies that caller.
    #[test]
    fn collapsed_gate_never_acknowledges_a_barrier_that_started_first() {
        let d = tempdir().unwrap();
        let Some(s) = collapsed_store(d.path()) else {
            return;
        };
        let gate = s.fs_barrier.clone().unwrap();
        // Model a barrier that is already running when a new caller arrives.
        {
            let mut state = gate.state.lock();
            state.in_flight = Some(1);
        }
        let waiting = {
            let state = gate.state.lock();
            state.in_flight.unwrap_or(state.completed) + 1
        };
        assert_eq!(
            waiting, 2,
            "a caller behind an in-flight barrier waits for the next one"
        );
        // Completing generation 1 must not release that caller.
        {
            let mut state = gate.state.lock();
            state.in_flight = None;
            state.completed = 1;
        }
        let state = gate.state.lock();
        assert!(state.completed < waiting);
    }

    #[test]
    fn corrupt_existing_object_is_rejected() {
        let d = tempdir().unwrap();
        let s = LocalBlobStore::new(d.path().to_path_buf()).unwrap();
        let id = s.put(b"good").unwrap();
        fs::write(s.object_path(id), b"evil").unwrap();
        assert!(matches!(s.get(id), Err(Error::Corrupt(_))));
        assert!(matches!(s.put(b"good"), Err(Error::Corrupt(_))));
    }
}
