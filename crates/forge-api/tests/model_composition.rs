//! Stateful model-based composition testing (I8, I9, I18, plus the liveness
//! property the invariant set does not yet state).
//!
//! Every other test in this repository drives one operation, or one race, and
//! checks one invariant. `#326` is the proof that this is not enough: `mount`
//! is correct, `write` is correct, `checkin` is correct, and the composition
//! silently loses the write. Nothing that tests operations one at a time can
//! see that class of defect.
//!
//! So this binary keeps a MODEL of what the repository should contain -- refs
//! to flat path/byte maps, a per-mount pinned base, a per-mount staged overlay,
//! and the observation set -- drives random but reproducible sequences of real
//! operations against a real `Forge`, and after EVERY step asserts that the
//! model and the real system agree and that the always-true properties hold.
//!
//! There is no property-testing dependency. The generator is a seeded xorshift
//! and the seed is printed with every failure, exactly as
//! `property_canonical.rs`, `property_merge_symmetry.rs` and
//! `property_attenuation.rs` already do.
//!
//! ## The model is deliberately the naive, obviously-correct one
//!
//! * a mount at `p` naming `ref:R` shows what `R` holds; a read-write mount
//!   pins that at mount time (I8), a read-only mount stays live (so foreign
//!   staleness is detectable, I9);
//! * `checkin p` folds the overlay staged under `p` onto `p`'s own pinned base
//!   and CASes `p`'s own ref;
//! * staged work is never lost and never silently ignored (I18);
//! * a session holding staged work can always publish it or explicitly abandon
//!   it.
//!
//! Where the implementation disagrees with that model, the harness records a
//! `Finding`, adopts the real state, and keeps going, so one run reports every
//! divergence rather than the first. Findings whose kind is in `KNOWN` are
//! characterised defects of the current tree; an unknown kind fails the test
//! immediately with the seed and the whole operation trace. `KNOWN` is also
//! asserted to be fully observed, so fixing one of these defects fails this
//! test until the entry is removed.

use forge_api::Forge;
use forge_cap::Cap;
use forge_store::Store;
use forge_types::{CasResult, EntryKind, Error, ObjectId};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// characterised defects of the current tree
// ---------------------------------------------------------------------------

/// Defects this harness rediscovers on the tree it is committed against.
///
/// Each entry must still be observed by the default run. When one is fixed the
/// "every known defect was observed" assertion fails and names it, which is the
/// signal to delete the row rather than to relax the check.
const KNOWN: &[(&str, &str)] = &[
    (
        "F4-SESSION-WEDGED-WITH-STAGED-WORK",
        "I20/I21 liveness, NOT closed: a read-write mount on a PROTECTED ref \
         accepts writes that `checkin` then denies (`ref R is protected`), and \
         `abandon` without an explicit discard refuses because work is staged. \
         Neither publish nor explicit abandon is possible. I20 refuses a \
         read-write `oid:` mount and a ref not holding a commit, which closed \
         the other two shapes of this; a protected ref is still accepted at \
         mount time and is still unpublishable.",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    kind: &'static str,
    detail: String,
}

// ---------------------------------------------------------------------------
// deterministic generator
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift64 has one fixed point; never seed it there.
        Rng(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }
}

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

/// A tree as the flat set of blob paths it holds. Two ForgeFS trees are equal
/// iff their flattenings are equal, because trees are content-addressed and
/// empty directories are pruned, so this loses nothing the model needs.
type FlatTree = BTreeMap<String, (Vec<u8>, bool)>;

/// Model-side commit identity. Real commit OIDs embed a timestamp, so equal
/// content does not imply an equal OID; the model needs its own token and the
/// harness learns the real OID for it from the operation's result.
type Cid = u64;

/// One mount's staged overlay: relative path -> new content, or a tombstone.
type OverlayM = BTreeMap<String, Option<(Vec<u8>, bool)>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Blob(Vec<u8>),
    Tree(FlatTree),
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SpecM {
    Ref(String),
    /// A raw `oid:` mount: a frozen tree, captured when the mount was made.
    Oid(FlatTree, ObjectId),
}

#[derive(Clone, Debug)]
struct MountM {
    path: String,
    spec: SpecM,
    rw: bool,
}

#[derive(Clone, Debug)]
struct SessionM {
    agent: usize,
    /// The private ref `session_open` published for this session. Kept so a
    /// failure trace names it; the model never resolves through it directly.
    #[allow(dead_code)]
    live_ref: String,
    mounts: Vec<MountM>,
    /// The commit each read-write `ref:` mount is pinned to. I8 says a session
    /// reads from a pinned base; the naive model pins each read-write mount to
    /// the ref it actually names, at the moment it was mounted.
    base: BTreeMap<String, Cid>,
    /// mount path -> relative path -> Some(bytes, exec) | None (tombstone)
    overlay: BTreeMap<String, OverlayM>,
    obs: BTreeMap<(String, String), Seen>,
}

impl SessionM {
    fn staged_total(&self) -> usize {
        self.overlay.values().map(|m| m.len()).sum()
    }
    fn ov(&self, mount: &str) -> OverlayM {
        self.overlay.get(mount).cloned().unwrap_or_default()
    }
}

#[derive(Default)]
struct Model {
    refs: BTreeMap<String, Cid>,
    protected: BTreeSet<String>,
    sealed: BTreeSet<String>,
    commits: BTreeMap<Cid, FlatTree>,
    sessions: BTreeMap<String, SessionM>,
    next_cid: Cid,
}

impl Model {
    fn mint(&mut self, tree: FlatTree) -> Cid {
        self.next_cid += 1;
        let cid = self.next_cid;
        self.commits.insert(cid, tree);
        cid
    }
    fn tree_of(&self, cid: Cid) -> &FlatTree {
        self.commits.get(&cid).expect("model commit")
    }
}

/// The tree that model-mount `m` shows.
fn model_mount_tree<'a>(model: &'a Model, s: &'a SessionM, m: &'a MountM) -> Option<&'a FlatTree> {
    match &m.spec {
        SpecM::Oid(t, _) => Some(t),
        SpecM::Ref(r) => {
            if m.rw {
                // I8: read-write mounts read their pinned base.
                s.base.get(&m.path).map(|c| model.tree_of(*c))
            } else {
                // Foreign read-only mounts stay live so staleness is visible.
                model.refs.get(r).map(|c| model.tree_of(*c))
            }
        }
    }
}

// --- path algebra, mirroring forge-ns -------------------------------------

fn normalize_abs(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    let mut p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

fn longest_mount<'a>(mounts: &'a [MountM], abs: &str) -> Option<&'a MountM> {
    let abs = normalize_abs(abs);
    let mut best: Option<&MountM> = None;
    let mut best_len = 0usize;
    for m in mounts {
        let mp = normalize_abs(&m.path);
        if (abs == mp || mp == "/" || abs.starts_with(&format!("{mp}/"))) && mp.len() >= best_len {
            best_len = mp.len();
            best = Some(m);
        }
    }
    best
}

fn rel_of(mount_path: &str, abs: &str) -> String {
    let mp = normalize_abs(mount_path);
    let abs = normalize_abs(abs);
    if mp == "/" {
        return abs.trim_start_matches('/').to_string();
    }
    if abs == mp {
        return String::new();
    }
    abs.strip_prefix(&format!("{mp}/"))
        .expect("generated paths stay under their mount")
        .to_string()
}

/// The copy-on-write fold, on flat trees. Valid because the generator never
/// puts a blob at a path that is also a directory prefix of another blob;
/// `overlay_prefix_conflict.rs` owns that case.
fn model_apply_overlay(base: &FlatTree, ov: &OverlayM) -> FlatTree {
    let mut t = base.clone();
    for (rel, op) in ov {
        match op {
            Some(v) => {
                t.insert(rel.clone(), v.clone());
            }
            None => {
                t.remove(rel);
            }
        }
    }
    t
}

fn subtree(tree: &FlatTree, rel: &str) -> FlatTree {
    let prefix = format!("{rel}/");
    tree.iter()
        .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|r| (r.to_string(), v.clone())))
        .collect()
}

fn model_current_at(tree: &FlatTree, rel: &str) -> Seen {
    if rel.is_empty() {
        return Seen::Tree(tree.clone());
    }
    if let Some((bytes, _)) = tree.get(rel) {
        return Seen::Blob(bytes.clone());
    }
    let sub = subtree(tree, rel);
    if sub.is_empty() {
        Seen::Absent
    } else {
        Seen::Tree(sub)
    }
}

fn overlay_shadows(ov: &OverlayM, rel: &str) -> bool {
    if ov.contains_key(rel) {
        return true;
    }
    let mut prefix = rel;
    while let Some(cut) = prefix.rfind('/') {
        prefix = &prefix[..cut];
        if ov.contains_key(prefix) {
            return true;
        }
    }
    false
}

fn model_observed_at(ov: &OverlayM, tree: &FlatTree, rel: &str) -> Seen {
    if !rel.is_empty() && overlay_shadows(ov, rel) {
        return match ov.get(rel) {
            Some(Some((bytes, _))) => Seen::Blob(bytes.clone()),
            _ => Seen::Absent,
        };
    }
    model_current_at(tree, rel)
}

// --- capability model -------------------------------------------------------

/// Mirrors the grant strings the harness actually issues; nothing more.
/// `capability_boundary.rs` and `property_attenuation.rs` own I13/I14 proper.
#[derive(Clone)]
struct AgentM {
    id: String,
    unrestricted: bool,
    patterns: Vec<String>,
}

impl AgentM {
    fn allows_ref(&self, name: &str) -> bool {
        if self.unrestricted {
            return true;
        }
        self.patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(pre) => name.starts_with(pre),
            None => name == p,
        })
    }
}

// ---------------------------------------------------------------------------
// the world: model + real system + the trace that produced them
// ---------------------------------------------------------------------------

struct World {
    _dir: TempDir,
    root_path: std::path::PathBuf,
    forge: Forge,
    caps: Vec<(AgentM, Cap)>,
    model: Model,
    /// Model commit token -> the real OID the harness learned for it.
    oids: BTreeMap<Cid, ObjectId>,
    trace: Vec<String>,
    findings: Vec<Finding>,
    seed: u64,
}

impl World {
    fn new(seed: u64) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = Forge::init(dir.path()).expect("init");
        let root = forge.root_cap().expect("root cap");
        let mut caps = vec![(
            AgentM {
                id: root.agent_id().to_string(),
                unrestricted: true,
                patterns: vec![],
            },
            root.clone(),
        )];
        for id in ["a1", "a2"] {
            let cap = forge
                .grant(
                    &root,
                    vec![
                        "ops=read,write,branch".into(),
                        format!("agent={id}"),
                        format!("ref=main,heads/agents/{id}/*"),
                    ],
                )
                .expect("grant");
            caps.push((
                AgentM {
                    id: id.into(),
                    unrestricted: false,
                    patterns: vec!["main".into(), format!("heads/agents/{id}/*")],
                },
                cap,
            ));
        }

        let mut model = Model::default();
        // `init` publishes exactly one protected commit ref with an empty tree.
        let empty = model.mint(FlatTree::new());
        model.refs.insert("main".into(), empty);
        model.protected.insert("main".into());
        let root_path = dir.path().to_path_buf();
        let (main_oid, _) = forge.peel_commit("main").expect("main");
        let mut oids = BTreeMap::new();
        oids.insert(empty, main_oid);

        World {
            _dir: dir,
            root_path,
            forge,
            caps,
            model,
            oids,
            trace: Vec::new(),
            findings: Vec::new(),
            seed,
        }
    }

    fn store(&self) -> Store {
        // A fresh read-only handle per verification pass: `Store` keeps hot LRU
        // caches, so re-reading through the writing handle would prove nothing
        // about the durable bytes.
        Store::open_read_only(&self.root_path.join(".forge")).expect("read-only store")
    }

    fn say(&mut self, line: String) {
        self.trace.push(line);
    }

    /// The model token for a real commit OID: reuse the existing one when this
    /// OID has already been named, so that "same commit" stays decidable in the
    /// model even after the harness has adopted state from reality.
    fn cid_for(&mut self, oid: ObjectId, tree: FlatTree) -> Cid {
        if let Some((cid, _)) = self.oids.iter().find(|(_, o)| **o == oid) {
            return *cid;
        }
        let cid = self.model.mint(tree);
        self.oids.insert(cid, oid);
        cid
    }

    fn record(&mut self, kind: &'static str, detail: String) {
        assert!(
            KNOWN.iter().any(|(k, _)| *k == kind),
            "internal: unregistered finding kind {kind}"
        );
        let f = Finding { kind, detail };
        if !self.findings.contains(&f) {
            self.findings.push(f);
        }
    }

    /// Fail with everything needed to replay: the seed and the full trace.
    fn bail(&self, what: &str) -> ! {
        let mut msg = format!(
            "\nMODEL/SYSTEM DIVERGENCE (unclassified)\n  seed = {}\n  {what}\n\noperation trace:\n",
            self.seed
        );
        for (i, line) in self.trace.iter().enumerate() {
            msg.push_str(&format!("  {i:3}. {line}\n"));
        }
        msg.push_str("\nreplay with: FORGEFS_MODEL_SEEDS=");
        msg.push_str(&self.seed.to_string());
        msg.push('\n');
        panic!("{msg}");
    }
}

// --- reading the real system ------------------------------------------------

fn flatten(store: &Store, tree: ObjectId, prefix: &str, out: &mut FlatTree) {
    for e in store.get_tree(tree).expect("tree").entries {
        let p = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        match e.kind {
            EntryKind::Tree => flatten(store, e.id, &p, out),
            EntryKind::Blob => {
                out.insert(p, (store.get_blob_data(e.id).expect("blob"), e.exec));
            }
        }
    }
}

/// Reread every object reachable from `tree` from durable bytes and check that
/// each one still hashes to the id it is filed under.
fn verify_reachable(store: &Store, tree: ObjectId) {
    for oid in store.reachable_oids(tree).expect("reachable") {
        store
            .get_raw_verified(oid)
            .unwrap_or_else(|e| panic!("object {} failed to reread/rehash: {e:?}", oid.hex()));
    }
}

// ---------------------------------------------------------------------------
// verification after every single operation
// ---------------------------------------------------------------------------

impl World {
    fn verify(&mut self) {
        let store = self.store();

        // Every ref that exists resolves to bytes that exist and hash correctly.
        let real_refs = self.forge.refs(&self.caps[0].1).expect("refs");
        let mut real_commit_refs: BTreeMap<String, ObjectId> = BTreeMap::new();
        for r in &real_refs {
            store
                .get_raw_verified(r.oid)
                .unwrap_or_else(|e| panic!("ref {} -> unreadable object: {e:?}", r.name));
            if r.kind == "commit" {
                let c = store.get_commit(r.oid).expect("commit");
                verify_reachable(&store, c.tree);
                real_commit_refs.insert(r.name.clone(), r.oid);
            }
        }

        // Model refs and real refs name the same commits with the same content.
        let model_refs: Vec<(String, Cid)> = self
            .model
            .refs
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (name, cid) in model_refs {
            let Some(real_oid) = real_commit_refs.get(&name).copied() else {
                self.bail(&format!(
                    "model ref {name} does not exist in the repository"
                ));
            };
            if let Some(known) = self.oids.get(&cid).copied() {
                if known != real_oid {
                    self.bail(&format!(
                        "ref {name}: model expects commit {} but the repository holds {}",
                        known.hex(),
                        real_oid.hex()
                    ));
                }
            } else {
                self.oids.insert(cid, real_oid);
            }
            let c = store.get_commit(real_oid).expect("commit");
            let mut real = FlatTree::new();
            flatten(&store, c.tree, "", &mut real);
            if &real != self.model.tree_of(cid) {
                let want = self.model.tree_of(cid).clone();
                self.bail(&format!(
                    "ref {name} content mismatch\n    model = {}\n    real  = {}",
                    show_tree(&want),
                    show_tree(&real)
                ));
            }
        }
        for name in real_commit_refs.keys() {
            if !self.model.refs.contains_key(name) {
                self.bail(&format!(
                    "repository has commit ref {name}, the model does not"
                ));
            }
        }

        // Sessions: mounts, staged overlay and observations agree exactly.
        let sessions: Vec<String> = self.model.sessions.keys().cloned().collect();
        for ns in sessions {
            self.verify_session(&store, &ns);
        }

        // fsck --full is clean at every point.
        let report = self.forge.fsck(&self.caps[0].1, true).expect("fsck");
        if !report.ok || !report.findings.is_empty() {
            self.bail(&format!(
                "fsck --full is not clean: ok={} findings={:?}",
                report.ok, report.findings
            ));
        }

        // Liveness, evaluated on the model: a session holding staged work must
        // have at least one transition that publishes or explicitly abandons
        // it. `abandon` without a discard refuses while anything is staged, so
        // this reduces to "some mount holding staged work can be checked in".
        let sessions: Vec<String> = self.model.sessions.keys().cloned().collect();
        for ns in sessions {
            let s = self.model.sessions[&ns].clone();
            if s.staged_total() == 0 {
                continue;
            }
            // Any successful checkin is a transition out of "holding staged
            // work". The authoritative check is the destructive drain at end
            // of sequence;
            // this one exists so a wedge is reported at the step that created
            // it, with the trace that led there.
            let publishable = s
                .overlay
                .iter()
                .any(|(mp, entries)| !entries.is_empty() && self.predict_checkin(&ns, mp).is_ok());
            if !publishable {
                let why: Vec<String> = s
                    .overlay
                    .iter()
                    .filter(|(_, e)| !e.is_empty())
                    .map(|(mp, e)| {
                        format!(
                            "mount {mp} ({} staged) -> checkin {:?}",
                            e.len(),
                            self.predict_checkin(&ns, mp)
                        )
                    })
                    .collect();
                self.record(
                    "F4-SESSION-WEDGED-WITH-STAGED-WORK",
                    format!(
                        "session {ns} holds {} staged entries; abandon without \
                         --discard refuses and no mount can be checked in: {}",
                        s.staged_total(),
                        why.join("; ")
                    ),
                );
            }
        }
    }

    fn verify_session(&mut self, store: &Store, ns: &str) {
        let s = self.model.sessions[ns].clone();

        let real_mounts = store.meta.list_mounts(ns).expect("mounts");
        let mut real: Vec<(String, String, String)> = real_mounts
            .iter()
            .map(|m| (m.path.clone(), m.spec.clone(), m.mode.clone()))
            .collect();
        real.sort();
        let mut want: Vec<(String, String, String)> = s
            .mounts
            .iter()
            .map(|m| {
                (
                    m.path.clone(),
                    match &m.spec {
                        SpecM::Ref(r) => format!("ref:{r}"),
                        SpecM::Oid(_, id) => format!("oid:{}", id.hex()),
                    },
                    if m.rw { "rw" } else { "ro" }.to_string(),
                )
            })
            .collect();
        want.sort();
        if real != want {
            self.bail(&format!(
                "session {ns} mounts differ\n    model = {want:?}\n    real  = {real:?}"
            ));
        }

        for m in &s.mounts {
            let rows = store.meta.overlay_list(ns, &m.path).expect("overlay");
            let mut real: OverlayM = BTreeMap::new();
            for r in rows {
                real.insert(
                    r.path.clone(),
                    r.blob_oid
                        .map(|id| (store.get_blob_data(id).expect("staged blob"), r.exec)),
                );
            }
            let want = s.ov(&m.path);
            if real != want {
                self.bail(&format!(
                    "session {ns} staged overlay under {} differs\n    model = {:?}\n    real  = {:?}",
                    m.path,
                    want.keys().collect::<Vec<_>>(),
                    real.keys().collect::<Vec<_>>()
                ));
            }
        }

        // Observations: compare semantically. The catalog stores OIDs; the
        // model stores the content those OIDs name, which is the same fact.
        let mut real_obs: BTreeMap<(String, String), Seen> = BTreeMap::new();
        for o in store.meta.observations(ns).expect("observations") {
            let seen = match o.seen {
                forge_store::Observed::Absent => Seen::Absent,
                forge_store::Observed::Blob(id) => {
                    Seen::Blob(store.get_blob_data(id).expect("observed blob"))
                }
                forge_store::Observed::Tree(id) => {
                    let mut t = FlatTree::new();
                    flatten(store, id, "", &mut t);
                    Seen::Tree(t)
                }
            };
            real_obs.insert((o.mount.clone(), o.path.clone()), seen);
        }
        if real_obs != s.obs {
            let mk = |m: &BTreeMap<(String, String), Seen>| m.keys().cloned().collect::<Vec<_>>();
            if mk(&real_obs) != mk(&s.obs) {
                self.bail(&format!(
                    "session {ns} observation set differs\n    model = {:?}\n    real  = {:?}",
                    mk(&s.obs),
                    mk(&real_obs)
                ));
            }
            for (k, want) in &s.obs {
                let got = &real_obs[k];
                if got != want {
                    self.bail(&format!(
                        "session {ns} observation at {k:?} differs\n    model = {want:?}\n    real  = {got:?}"
                    ));
                }
            }
        }
    }
}

fn show_tree(t: &FlatTree) -> String {
    let parts: Vec<String> = t
        .iter()
        .map(|(k, (v, x))| {
            format!(
                "{k}={}{}",
                String::from_utf8_lossy(v),
                if *x { "*" } else { "" }
            )
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

// ---------------------------------------------------------------------------
// checkin prediction, mirroring the real order of checks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Predicted {
    Noop,
    Updated(FlatTree),
    Forked(FlatTree),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    RoMount,
    OidMount,
    Denied,
    Sealed,
    Stale,
    /// I22: nothing to publish here, and the session holds staged work
    /// somewhere else.
    StagedElsewhere,
}

impl World {
    fn predict_checkin(&self, ns: &str, mount_arg: &str) -> Result<Predicted, Refusal> {
        let s = &self.model.sessions[ns];
        let agent = &self.caps[s.agent].0;
        let m = longest_mount(&s.mounts, mount_arg).expect("a session always has a / mount");
        if !m.rw {
            return Err(Refusal::RoMount);
        }
        let SpecM::Ref(ref_name) = &m.spec else {
            return Err(Refusal::OidMount);
        };
        if !agent.allows_ref(ref_name) {
            return Err(Refusal::Denied);
        }
        let Some(&ref_cid) = self.model.refs.get(ref_name) else {
            return Err(Refusal::Denied);
        };
        let base_cid = *s
            .base
            .get(&m.path)
            .expect("a read-write ref mount is pinned");
        let ov = s.ov(&m.path);

        // I9 staleness, over the whole observation set, not just this mount.
        //
        // Every mount is validated against the same tree its own reads resolve
        // against: a read-write mount against its OWN pin (I19), which cannot
        // move under it, so an authorised read through one can never make the
        // session unpublishable (I21); a read-only mount against its live ref,
        // which is exactly the cross-mount staleness I9 exists for.
        for ((obs_mount, rel), seen) in &s.obs {
            if obs_mount == &m.path && overlay_shadows(&ov, rel) {
                continue;
            }
            let tree = if obs_mount == &m.path {
                self.model.tree_of(base_cid).clone()
            } else if let Some(om) = s.mounts.iter().find(|x| &x.path == obs_mount) {
                match &om.spec {
                    SpecM::Oid(t, _) => t.clone(),
                    SpecM::Ref(_) if om.rw => match s.base.get(obs_mount) {
                        Some(c) => self.model.tree_of(*c).clone(),
                        None => continue,
                    },
                    SpecM::Ref(r) => match self.model.refs.get(r) {
                        Some(c) => self.model.tree_of(*c).clone(),
                        None => continue,
                    },
                }
            } else {
                continue;
            };
            if &model_current_at(&tree, rel) != seen {
                return Err(Refusal::Stale);
            }
        }

        let new_tree = model_apply_overlay(self.model.tree_of(base_cid), &ov);
        // The no-op shortcut is taken before the sealed/protected checks, so a
        // no-op checkin on a sealed or protected ref still reports Noop.
        if &new_tree == self.model.tree_of(base_cid) && base_cid == ref_cid {
            // I22: `Noop` is the one outcome that may never be said over work
            // that exists. This mount stages nothing publishable, so WORK
            // under any OTHER mount turns the answer into a refusal naming it.
            //
            // Work, not rows. An overlay that folds to its own mount's base --
            // a delete of a path that mount does not have, a write of bytes it
            // already holds -- is not work, and counting it as such wedges a
            // session all of whose mounts hold only such rows (#342).
            //
            // `updated`/`forked` below are deliberately NOT constrained this
            // way either: they are progress, and refusing them would wedge a
            // session holding two writable mounts with work in both (I19/I21).
            let elsewhere = s.overlay.iter().any(|(mp, entries)| {
                if mp.as_str() == m.path.as_str() || entries.is_empty() {
                    return false;
                }
                match s.base.get(mp) {
                    Some(c) => {
                        let base = self.model.tree_of(*c);
                        &model_apply_overlay(base, entries) != base
                    }
                    // Fails safe, exactly as production does.
                    None => true,
                }
            });
            if elsewhere {
                return Err(Refusal::StagedElsewhere);
            }
            return Ok(Predicted::Noop);
        }
        if self.model.sealed.contains(ref_name) {
            return Err(Refusal::Sealed);
        }
        if self.model.protected.contains(ref_name) {
            return Err(Refusal::Denied);
        }
        if base_cid == ref_cid {
            Ok(Predicted::Updated(new_tree))
        } else {
            Ok(Predicted::Forked(new_tree))
        }
    }
}

// ---------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------

fn spec_str_of(spec: &SpecM) -> String {
    match spec {
        SpecM::Ref(r) => format!("ref:{r}"),
        SpecM::Oid(_, id) => format!("oid:{}", id.hex()),
    }
}

const FILES: &[&str] = &["a.txt", "b.txt", "d1/c.txt", "d1/e.txt", "d2/f.txt"];
const DIRS: &[&str] = &["", "d1", "d2"];
const MOUNT_POINTS: &[&str] = &["/w1", "/w2"];

impl World {
    fn agent_cap(&self, i: usize) -> Cap {
        self.caps[i].1.clone()
    }

    fn op_open_session(&mut self, rng: &mut Rng) {
        let agent = rng.below(self.caps.len());
        let a = self.caps[agent].0.clone();
        let readable: Vec<String> = self
            .model
            .refs
            .keys()
            .filter(|r| a.allows_ref(r))
            .cloned()
            .collect();
        if readable.is_empty() {
            return;
        }
        let from = rng.pick(&readable).clone();
        let cap = self.agent_cap(agent);
        let ns = match self.forge.session_open(&cap, &from) {
            Ok(ns) => ns,
            Err(e) => {
                self.bail(&format!("session_open({from}) as {} failed: {e:?}", a.id));
            }
        };
        self.say(format!("session_open agent={} from={from} -> {ns}", a.id));

        let from_cid = self.model.refs[&from];
        let live = format!("heads/agents/{}/{ns}", a.id);
        // `session_open` publishes the private live ref at the pinned commit,
        // and the default mounts: `/` read-write on that live ref, and `/main`
        // read-only when the capability may read `main`.
        self.model.refs.insert(live.clone(), from_cid);
        let mut mounts = vec![MountM {
            path: "/".into(),
            spec: SpecM::Ref(live.clone()),
            rw: true,
        }];
        if a.allows_ref("main") {
            mounts.push(MountM {
                path: "/main".into(),
                spec: SpecM::Ref("main".into()),
                rw: false,
            });
        }
        let mut base = BTreeMap::new();
        base.insert("/".to_string(), from_cid);
        self.model.sessions.insert(
            ns,
            SessionM {
                agent,
                live_ref: live,
                mounts,
                base,
                overlay: BTreeMap::new(),
                obs: BTreeMap::new(),
            },
        );
    }

    fn pick_session(&self, rng: &mut Rng) -> Option<String> {
        let keys: Vec<&String> = self.model.sessions.keys().collect();
        if keys.is_empty() {
            None
        } else {
            Some(rng.pick(&keys).to_string())
        }
    }

    fn op_mount(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let a = self.caps[s.agent].0.clone();
        let path = rng.pick(MOUNT_POINTS).to_string();
        let rw = rng.chance(2, 3);

        // Candidate specs the model believes this capability may mount this way.
        let mut specs: Vec<SpecM> = Vec::new();
        for name in self.model.refs.keys() {
            if a.allows_ref(name) {
                specs.push(SpecM::Ref(name.clone()));
            }
        }
        if a.unrestricted && rng.chance(1, 4) {
            // A raw `oid:` mount is legal for an unrestricted capability, and
            // it is one of the shapes that can strand a session.
            let names: Vec<String> = self.model.refs.keys().cloned().collect();
            let name = rng.pick(&names).clone();
            let cid = self.model.refs[&name];
            let oid = self.oids[&cid];
            let tree_oid = self.store().get_commit(oid).expect("commit").tree;
            specs.push(SpecM::Oid(self.model.tree_of(cid).clone(), tree_oid));
        }
        if specs.is_empty() {
            return;
        }
        let spec = rng.pick(&specs).clone();
        let spec_str = spec_str_of(&spec);

        // What the model expects `mount` to refuse, decided BEFORE the call.
        //
        // I20: a read-write `oid:` spec names immutable bytes with no ref for
        // checkin to advance, so it is refused at mount time rather than
        // accepting a write that no verb and no capability could ever publish.
        let rw_oid = rw && matches!(spec, SpecM::Oid(..));
        // I19: re-mounting a path at a different spec, or demoting one to
        // read-only, while it holds staged work would send that work to a ref
        // it was never written against. Refused, never silently retargeted.
        let (retarget, demote) = match s.mounts.iter().find(|m| m.path == path) {
            Some(e) => (spec_str_of(&e.spec) != spec_str, e.rw && !rw),
            None => (false, false),
        };
        let moves_staged_work = !s.ov(&path).is_empty() && (retarget || demote);

        let cap = self.agent_cap(s.agent);
        let res = self.forge.mount(&cap, &ns, &path, &spec_str, rw);
        self.say(format!(
            "mount ns={ns} {path} {spec_str} {} -> {}",
            if rw { "rw" } else { "ro" },
            if res.is_ok() { "ok" } else { "err" }
        ));
        match (&res, rw_oid, moves_staged_work) {
            (Ok(()), false, false) => {}
            // The rw-`oid:` refusal is checked first in production too.
            (Err(Error::Denied(_)), true, _) => return,
            (Err(Error::Invalid(_)), false, true) => return,
            _ => self.bail(&format!(
                "mount {path} {spec_str} rw={rw} as {}: real {res:?}, but the model \
                 expected {}",
                a.id,
                if rw_oid {
                    "a Denied for a read-write oid: spec (I20)"
                } else if moves_staged_work {
                    "an Invalid over staged work this re-mount would move (I19)"
                } else {
                    "success"
                }
            )),
        }

        let s = self.model.sessions.get_mut(&ns).expect("session");
        s.mounts.retain(|m| m.path != path);
        s.mounts.push(MountM {
            path: path.clone(),
            spec: spec.clone(),
            rw,
        });
        // I8: a read-write mount pins the ref it names, at mount time.
        if rw {
            if let SpecM::Ref(r) = &spec {
                let cid = self.model.refs[r];
                self.model
                    .sessions
                    .get_mut(&ns)
                    .expect("session")
                    .base
                    .insert(path, cid);
            }
        }
    }

    /// A path under one of the session's mounts, plus the mount it lands in.
    fn pick_path(&self, rng: &mut Rng, s: &SessionM, want_rw: bool) -> Option<(String, String)> {
        let candidates: Vec<&MountM> = s
            .mounts
            .iter()
            .filter(|m| !want_rw || m.rw)
            .filter(|m| !matches!(m.spec, SpecM::Oid(..)) || !want_rw || true)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let m = *rng.pick(&candidates);
        let file = rng.pick(FILES);
        let abs = if m.path == "/" {
            format!("/{file}")
        } else {
            format!("{}/{file}", m.path)
        };
        Some((abs, m.path.clone()))
    }

    /// Whether the session's own capability still covers the ref the mount at
    /// `abs` names -- which decides whether ANY verb through it is accepted,
    /// reads included.
    ///
    /// A losing CAS retargets the mount at `forks/<ref>/<agent>/<ulid>` (I18),
    /// and an agent capability scoped to `heads/agents/<id>/*` does not cover
    /// `forks/**`. So an agent whose checkin lost a race can no longer read or
    /// write through its own mount -- including the session's own `/` -- and
    /// the work I18 preserved sits at a ref its own capability cannot name.
    /// Modelled rather than characterised, because a scoped capability
    /// genuinely does not match that pattern and no invariant claims it should;
    /// but see the commit message. Nothing is LOST -- the fork holds the work
    /// and the overlay was cleared, so `abandon` still succeeds and this is not
    /// F4 -- yet whether I18's "retargets the session to it" means anything
    /// when the session cannot then act on it is a real question this harness
    /// raised and does not answer.
    fn mount_ref_is_reachable(&self, s: &SessionM, abs: &str) -> bool {
        let m = longest_mount(&s.mounts, abs).expect("mount");
        match &m.spec {
            SpecM::Ref(r) => self.caps[s.agent].0.allows_ref(r),
            SpecM::Oid(..) => true,
        }
    }

    fn op_write(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let Some((abs, _)) = self.pick_path(rng, &s, true) else {
            return;
        };
        let content = format!("v{}", rng.below(6)).into_bytes();
        let exec = rng.chance(1, 5);
        let cap = self.agent_cap(s.agent);
        let unwritable = !self.mount_ref_is_reachable(&s, &abs);
        let res = self.forge.write(&cap, &ns, &abs, &content, exec);
        self.say(format!(
            "write ns={ns} {abs} = {:?} exec={exec} -> {}",
            String::from_utf8_lossy(&content),
            if res.is_ok() { "ok" } else { "err" }
        ));
        match (&res, unwritable) {
            (Ok(_), false) => {}
            (Err(Error::Denied(_)), true) => return,
            _ => self.bail(&format!(
                "write {abs}: real {res:?}, model expected {}",
                if unwritable {
                    "a Denied -- the mount names a ref this capability does not cover"
                } else {
                    "success"
                }
            )),
        }
        let m = longest_mount(&s.mounts, &abs).expect("mount").clone();
        let rel = rel_of(&m.path, &abs);
        let sm = self.model.sessions.get_mut(&ns).expect("session");
        sm.overlay
            .entry(m.path)
            .or_default()
            .insert(rel, Some((content, exec)));
    }

    fn op_delete(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let Some((abs, _)) = self.pick_path(rng, &s, true) else {
            return;
        };
        let cap = self.agent_cap(s.agent);
        let unwritable = !self.mount_ref_is_reachable(&s, &abs);
        let res = self.forge.delete(&cap, &ns, &abs);
        self.say(format!(
            "delete ns={ns} {abs} -> {}",
            if res.is_ok() { "ok" } else { "err" }
        ));
        match (&res, unwritable) {
            (Ok(()), false) => {}
            (Err(Error::Denied(_)), true) => return,
            _ => self.bail(&format!(
                "delete {abs}: real {res:?}, model expected {}",
                if unwritable {
                    "a Denied -- the mount names a ref this capability does not cover"
                } else {
                    "success"
                }
            )),
        }
        let m = longest_mount(&s.mounts, &abs).expect("mount").clone();
        let rel = rel_of(&m.path, &abs);
        let sm = self.model.sessions.get_mut(&ns).expect("session");
        sm.overlay.entry(m.path).or_default().insert(rel, None);
    }

    /// I9: record what a read or a listing saw. The model computes it from the
    /// mount's own pinned base; the catalog records what the implementation
    /// actually served. Disagreement is an I8 divergence, not an I9 one, so it
    /// is routed through the mount-view classifier and then adopted.
    fn note_observation(&mut self, ns: &str, m: &MountM, rel: &str, want: Seen, what: &str) {
        let key = (m.path.clone(), rel.to_string());
        let real = self.real_observation(ns, &m.path, rel);
        let seen = match real {
            Some(real) if real != want => {
                let d = format!("recorded observation model {want:?} vs real {real:?}");
                self.diverged_on_mount_view(ns, m, rel, what, || d.clone());
                real
            }
            _ => want,
        };
        self.model
            .sessions
            .get_mut(ns)
            .expect("session")
            .obs
            .insert(key, seen);
    }

    fn real_observation(&self, ns: &str, mount: &str, rel: &str) -> Option<Seen> {
        let store = self.store();
        let row = store
            .meta
            .observations(ns)
            .ok()?
            .into_iter()
            .find(|o| o.mount == mount && o.path == rel)?;
        Some(match row.seen {
            forge_store::Observed::Absent => Seen::Absent,
            forge_store::Observed::Blob(id) => {
                Seen::Blob(store.get_blob_data(id).expect("observed blob"))
            }
            forge_store::Observed::Tree(id) => {
                let mut t = FlatTree::new();
                flatten(&store, id, "", &mut t);
                Seen::Tree(t)
            }
        })
    }

    fn op_read(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let Some((abs, _)) = self.pick_path(rng, &s, false) else {
            return;
        };
        let cap = self.agent_cap(s.agent);
        let unreachable = !self.mount_ref_is_reachable(&s, &abs);
        let real = self.forge.read(&cap, &ns, &abs);
        if unreachable {
            self.say(format!("read ns={ns} {abs} -> {real:?} (mount ref outside the cap)"));
            match &real {
                Err(Error::Denied(_)) => return,
                _ => self.bail(&format!(
                    "read {abs}: the mount names a ref this capability does not cover, \
                     so the model expected a Denied; real {real:?}"
                )),
            }
        }
        let m = longest_mount(&s.mounts, &abs).expect("mount").clone();
        let rel = rel_of(&m.path, &abs);
        let ov = s.ov(&m.path);
        let Some(tree) = model_mount_tree(&self.model, &s, &m).cloned() else {
            return;
        };
        let want = model_observed_at(&ov, &tree, &rel);
        self.say(format!(
            "read ns={ns} {abs} -> {}",
            match &real {
                Ok(b) => format!("{:?}", String::from_utf8_lossy(b)),
                Err(e) => format!("{e:?}"),
            }
        ));
        match (&want, &real) {
            (Seen::Blob(w), Ok(got)) if w == got => {}
            (Seen::Absent, Err(Error::NotFound(_))) => {}
            (Seen::Tree(_), Err(Error::Invalid(_))) => {}
            _ => {
                self.diverged_on_mount_view(&ns, &m, &rel, &format!("read {abs}"), || {
                    format!("model {want:?} vs real {real:?}")
                });
            }
        }
        // The comparison above may have corrected the model's view of this
        // mount; record what the corrected model now says was seen.
        let (m2, ov2, tree2) = self.mount_view(&ns, &abs);
        let seen = model_observed_at(&ov2, &tree2, &rel);
        self.note_observation(&ns, &m2, &rel, seen, &format!("read {abs}"));
    }

    /// The model's current view through the mount that `abs` lands in.
    fn mount_view(&self, ns: &str, abs: &str) -> (MountM, OverlayM, FlatTree) {
        let s = &self.model.sessions[ns];
        let m = longest_mount(&s.mounts, abs).expect("mount").clone();
        let ov = s.ov(&m.path);
        let tree = model_mount_tree(&self.model, s, &m)
            .cloned()
            .unwrap_or_default();
        (m, ov, tree)
    }

    fn op_ls(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let mounts: Vec<&MountM> = s.mounts.iter().collect();
        let m = (*rng.pick(&mounts)).clone();
        let dir = rng.pick(DIRS).to_string();
        let abs = if m.path == "/" {
            if dir.is_empty() {
                "/".to_string()
            } else {
                format!("/{dir}")
            }
        } else if dir.is_empty() {
            m.path.clone()
        } else {
            format!("{}/{dir}", m.path)
        };
        let cap = self.agent_cap(s.agent);
        let unreachable = !self.mount_ref_is_reachable(&s, &abs);
        let real = self.forge.ls(&cap, &ns, &abs);
        if unreachable {
            self.say(format!("ls ns={ns} {abs} -> {real:?} (mount ref outside the cap)"));
            match &real {
                Err(Error::Denied(_)) => return,
                _ => self.bail(&format!(
                    "ls {abs}: the mount names a ref this capability does not cover, \
                     so the model expected a Denied; real {real:?}"
                )),
            }
        }
        let rel = rel_of(&m.path, &abs);
        let ov = s.ov(&m.path);
        let Some(tree) = model_mount_tree(&self.model, &s, &m).cloned() else {
            return;
        };

        let want_names = model_ls(&tree, &ov, &rel);
        self.say(format!(
            "ls ns={ns} {abs} -> {}",
            match &real {
                Ok(v) => format!("{:?}", v.iter().map(|e| e.0.clone()).collect::<Vec<_>>()),
                Err(e) => format!("{e:?}"),
            }
        ));
        match (&want_names, &real) {
            (Some(w), Ok(got)) => {
                let g: Vec<(String, String)> =
                    got.iter().map(|e| (e.0.clone(), e.1.clone())).collect();
                if w != &g {
                    self.diverged_on_mount_view(&ns, &m, &rel, &format!("ls {abs}"), || {
                        format!("model {w:?} vs real {g:?}")
                    });
                }
            }
            (None, Err(_)) => {}
            _ => {
                self.diverged_on_mount_view(&ns, &m, &rel, &format!("ls {abs}"), || {
                    format!("model {want_names:?} vs real {real:?}")
                });
            }
        }
        // I9: a listing is a read, and it records the committed subtree.
        let (m2, ov2, tree2) = self.mount_view(&ns, &abs);
        let seen = model_observed_at(&ov2, &tree2, &rel);
        self.note_observation(&ns, &m2, &rel, seen, &format!("ls {abs}"));
    }

    /// A read or listing through `m` did not match the model.
    ///
    /// This used to be the classifier for the two I8 defects: the real system
    /// served the session's ONE global pin as every read-write mount's base,
    /// so a mismatch here was usually F2 or F3 rather than news. I19 gave each
    /// read-write mount its own pin and both are gone, so there is nothing
    /// left to classify -- any mount-view divergence is now unclassified, and
    /// fails with the seed and the trace.
    fn diverged_on_mount_view(
        &mut self,
        ns: &str,
        m: &MountM,
        rel: &str,
        what: &str,
        detail: impl Fn() -> String,
    ) {
        let store = self.store();
        let rows = store.meta.list_mounts(ns).expect("mounts");
        let obs = store.meta.observations(ns).expect("obs");
        self.bail(&format!(
            "{what}: {}\n    mount = {m:?}\n    rel = {rel:?}\n    \
             real mounts = {rows:?}\n    real obs = {obs:?}",
            detail()
        ));
    }

    /// Overlay rows the repository itself still holds for `ns`, across every
    /// mount -- the same question `abandon_session` asks.
    fn staged_in_reality(&self, ns: &str) -> usize {
        let store = self.store();
        store
            .meta
            .list_mounts(ns)
            .expect("mounts")
            .iter()
            .map(|m| store.meta.overlay_list(ns, &m.path).expect("overlay").len())
            .sum()
    }

    fn op_checkin(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let mounts: Vec<String> = s.mounts.iter().map(|m| m.path.clone()).collect();
        let mount = rng.pick(&mounts).clone();
        let staged_before = s.staged_total();
        let staged_here = s.ov(&mount).len();
        let predicted = self.predict_checkin(&ns, &mount);
        let cap = self.agent_cap(s.agent);
        let real = self.forge.checkin(&cap, &ns, &mount, "model-harness");
        self.say(format!(
            "checkin ns={ns} mount={mount} (staged here={staged_here}, total={staged_before}) \
             -> model {predicted:?} / real {}",
            match &real {
                Ok(r) => format!("{r:?}"),
                Err(e) => format!("{e:?}"),
            }
        ));

        // I22, stated directly against reality rather than against the model:
        // after a checkin has said "there was nothing to do", the session must
        // hold nothing anywhere -- which is precisely what `abandon_session`
        // asks, so the two verbs cannot disagree. This is #326 as an assertion
        // rather than a characterisation.
        //
        // Row counts alone would be the wrong predicate. An overlay entry can
        // fold to no effect at all -- a delete of a path the base does not
        // have, a write of bytes already there -- and a no-op checkin consumes
        // exactly those, so "there was nothing to do" is true and nothing is
        // lost. What may never survive a `Noop` is an entry the checkin did
        // not account for.
        if let Ok(CasResult::Noop { .. }) = &real {
            let left = self.staged_in_reality(&ns);
            if left > 0 {
                self.bail(&format!(
                    "I22: checkin of mount {mount} in session {ns} returned Noop, but {left} \
                     overlay entries are still staged afterwards (staged before: \
                     {staged_before}, {staged_here} of them under {mount})"
                ));
            }
        }

        match (&predicted, &real) {
            (Ok(Predicted::Noop), Ok(CasResult::Noop { .. })) => {
                self.apply_checkin_clear(&ns, &mount, None);
            }
            (Ok(Predicted::Updated(tree)), Ok(CasResult::Updated { name, oid })) => {
                let m = longest_mount(&s.mounts, &mount).expect("mount").clone();
                let SpecM::Ref(rn) = &m.spec else {
                    unreachable!()
                };
                if rn != name {
                    self.bail(&format!("checkin updated {name}, model expected {rn}"));
                }
                let cid = self.cid_for(*oid, tree.clone());
                self.model.refs.insert(name.clone(), cid);
                self.apply_checkin_clear(&ns, &m.path, Some(cid));
            }
            (Ok(Predicted::Forked(tree)), Ok(CasResult::Forked { fork, ours, .. })) => {
                let m = longest_mount(&s.mounts, &mount).expect("mount").clone();
                let cid = self.cid_for(*ours, tree.clone());
                self.model.refs.insert(fork.clone(), cid);
                // I18: the losing CAS retargets this mount at the fork.
                let sm = self.model.sessions.get_mut(&ns).expect("session");
                for mm in sm.mounts.iter_mut() {
                    if mm.path == m.path {
                        mm.spec = SpecM::Ref(fork.clone());
                    }
                }
                self.apply_checkin_clear(&ns, &m.path, Some(cid));
            }
            (Err(Refusal::RoMount), Err(Error::Denied(_)))
            | (Err(Refusal::OidMount), Err(Error::Invalid(_)))
            | (Err(Refusal::Denied), Err(Error::Denied(_)))
            | (Err(Refusal::Sealed), Err(Error::Sealed(_)))
            | (Err(Refusal::Stale), Err(Error::StaleObservation { .. }))
            | (Err(Refusal::StagedElsewhere), Err(Error::Invalid(_))) => {}
            _ => {
                self.bail(&format!(
                    "checkin {mount}: model {predicted:?} but real {real:?}"
                ));
            }
        }
    }

    /// A successful checkin clears that mount's overlay and the whole session's
    /// observation set, and repins the mount it published.
    fn apply_checkin_clear(&mut self, ns: &str, mount: &str, new_base: Option<Cid>) {
        let sm = self.model.sessions.get_mut(ns).expect("session");
        sm.overlay.remove(mount);
        sm.obs.clear();
        if let Some(cid) = new_base {
            sm.base.insert(mount.to_string(), cid);
        }
    }

    fn resync_after_unmodelled_checkin(
        &mut self,
        ns: &str,
        mount: &str,
        real: &Result<CasResult, Error>,
    ) {
        let store = self.store();
        match real {
            Ok(CasResult::Noop { .. }) => {
                self.apply_checkin_clear(ns, mount, None);
            }
            Ok(CasResult::Updated { name, oid }) => {
                let mut t = FlatTree::new();
                flatten(
                    &store,
                    store.get_commit(*oid).expect("commit").tree,
                    "",
                    &mut t,
                );
                let cid = self.cid_for(*oid, t);
                self.model.refs.insert(name.clone(), cid);
                self.apply_checkin_clear(ns, mount, Some(cid));
            }
            Ok(CasResult::Forked { fork, ours, .. }) => {
                let mut t = FlatTree::new();
                flatten(
                    &store,
                    store.get_commit(*ours).expect("commit").tree,
                    "",
                    &mut t,
                );
                let cid = self.cid_for(*ours, t);
                self.model.refs.insert(fork.clone(), cid);
                let sm = self.model.sessions.get_mut(ns).expect("session");
                for mm in sm.mounts.iter_mut() {
                    if mm.path == mount {
                        mm.spec = SpecM::Ref(fork.clone());
                    }
                }
                self.apply_checkin_clear(ns, mount, Some(cid));
            }
            Err(_) => {}
        }
        // Whatever happened, the mount bases the model still believes in may be
        // stale now; the global pin is the only base the implementation has.
        self.adopt_global_pin(ns);
    }

    fn adopt_global_pin(&mut self, ns: &str) {
        let store = self.store();
        let Ok(row) = store.meta.get_namespace(ns) else {
            return;
        };
        let Some(pin) = row.pinned_oid else { return };
        let mut t = FlatTree::new();
        flatten(
            &store,
            store.get_commit(pin).expect("commit").tree,
            "",
            &mut t,
        );
        let s = self.model.sessions[ns].clone();
        let cid = self.cid_for(pin, t.clone());
        for m in &s.mounts {
            if m.rw && matches!(m.spec, SpecM::Ref(_)) {
                let cur = s.base.get(&m.path).map(|c| self.model.tree_of(*c).clone());
                if cur.as_ref() != Some(&t) {
                    self.model
                        .sessions
                        .get_mut(ns)
                        .expect("session")
                        .base
                        .insert(m.path.clone(), cid);
                }
            }
        }
    }

    fn op_branch(&mut self, rng: &mut Rng) {
        let agent = 0usize; // branching new top-level names needs root scope
        let names: Vec<String> = self.model.refs.keys().cloned().collect();
        let from = rng.pick(&names).clone();
        let name = format!("heads/topic{}", rng.below(3));
        if self.model.refs.contains_key(&name) {
            return;
        }
        let cap = self.agent_cap(agent);
        match self.forge.branch(&cap, &from, &name) {
            Ok(_) => {
                self.say(format!("branch {name} from {from}"));
                let cid = self.model.refs[&from];
                self.model.refs.insert(name, cid);
            }
            Err(e) => self.bail(&format!("branch {name} from {from} refused: {e:?}")),
        }
    }

    fn op_abandon_session(&mut self, rng: &mut Rng) {
        let Some(ns) = self.pick_session(rng) else {
            return;
        };
        let s = self.model.sessions[&ns].clone();
        let discard = rng.chance(1, 4);
        let cap = self.agent_cap(s.agent);
        let real = self.forge.abandon_session(&cap, &ns, discard);
        self.say(format!(
            "abandon ns={ns} discard={discard} staged={} -> {}",
            s.staged_total(),
            if real.is_ok() { "ok" } else { "refused" }
        ));
        match (&real, s.staged_total(), discard) {
            (Ok(_), _, _) => {
                // I18: the session's live ref and any fork it produced survive.
                self.model.sessions.remove(&ns);
            }
            (Err(Error::Invalid(_)), n, false) if n > 0 => {
                // Correct: refusing to destroy staged work is the point.
            }
            (Err(e), n, d) => self.bail(&format!(
                "abandon_session(discard={d}) with {n} staged failed unexpectedly: {e:?}"
            )),
        }
    }

    fn op_seal(&mut self, rng: &mut Rng) {
        let names: Vec<String> = self
            .model
            .refs
            .keys()
            .filter(|n| !self.model.sealed.contains(*n))
            .cloned()
            .collect();
        if names.is_empty() {
            return;
        }
        let name = rng.pick(&names).clone();
        let tag = format!("v0-{}", self.trace.len());
        let cap = self.agent_cap(0);
        match self.forge.seal(&cap, &name, &tag) {
            Ok(oid) => {
                self.say(format!("seal {name} as {tag}"));
                // A seal freezes `tags/<tag>`, a sealed snapshot ref. The ref it
                // was taken from stays writable, so nothing about the model's
                // commit refs changes here.
                self.model.sealed.insert(format!("tags/{tag}"));
                // I15: verification rereads durable bytes and must agree.
                let got = self.forge.verify_tag(&cap, &tag).expect("verify_tag");
                if got != oid {
                    self.bail(&format!(
                        "verify_tag({tag}) = {} != {}",
                        got.hex(),
                        oid.hex()
                    ));
                }
            }
            Err(e) => self.bail(&format!("seal {name} as {tag} refused: {e:?}")),
        }
    }
}

/// `ls` in the model: the mount tree's direct children under `rel`, with the
/// staged overlay folded over them. `None` means the listing must fail.
fn model_ls(tree: &FlatTree, ov: &OverlayM, rel: &str) -> Option<Vec<(String, String)>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    if !rel.is_empty() {
        if tree.contains_key(rel) {
            return None; // ls on a blob
        }
        if subtree(tree, rel).is_empty() {
            return None; // the directory is not in the committed tree
        }
    }
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for key in tree.keys() {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((first, _)) => {
                map.insert(first.to_string(), "tree".into());
            }
            None => {
                map.insert(rest.to_string(), "blob".into());
            }
        }
    }
    for (path, op) in ov {
        let Some(rest) = path.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((first, _)) => {
                map.entry(first.to_string())
                    .or_insert_with(|| "tree".into());
            }
            None => match op {
                Some(_) => {
                    map.insert(rest.to_string(), "blob".into());
                }
                None => {
                    map.remove(rest);
                }
            },
        }
    }
    Some(map.into_iter().collect())
}

// ---------------------------------------------------------------------------
// the drain phase: the liveness property, executed for real
// ---------------------------------------------------------------------------

impl World {
    /// Every live session must be able to finish: publish its staged work, or
    /// explicitly abandon it without a discard. Run at the end of a sequence,
    /// because it is necessarily destructive.
    fn drain(&mut self) {
        let sessions: Vec<String> = self.model.sessions.keys().cloned().collect();
        for ns in sessions {
            let s = self.model.sessions[&ns].clone();
            let cap = self.agent_cap(s.agent);
            let mut errors: Vec<String> = Vec::new();
            loop {
                let mut progressed = false;
                let mounts: Vec<String> = self.model.sessions[&ns]
                    .mounts
                    .iter()
                    .map(|m| m.path.clone())
                    .collect();
                for mp in mounts {
                    if self.model.sessions[&ns].ov(&mp).is_empty() {
                        continue;
                    }
                    let outcome = self.forge.checkin(&cap, &ns, &mp, "drain");
                    match &outcome {
                        Ok(CasResult::Noop { .. }) => {
                            // I22, same predicate as in `op_checkin`: a no-op
                            // checkin consumes the entries that folded to no
                            // effect, so what may never remain afterwards is
                            // an entry it did not account for.
                            let left = self.staged_in_reality(&ns);
                            if left > 0 {
                                self.bail(&format!(
                                    "draining session {ns}: checkin of mount {mp} returned \
                                     Noop but {left} overlay entries remain staged (I22)"
                                ));
                            }
                            progressed = true;
                            self.resync_after_unmodelled_checkin(&ns, &mp, &outcome);
                        }
                        Ok(_) => {
                            progressed = true;
                            self.resync_after_unmodelled_checkin(&ns, &mp, &outcome);
                        }
                        Err(e) => errors.push(format!("checkin {mp} -> {e:?}")),
                    }
                }
                if !progressed {
                    break;
                }
            }
            match self.forge.abandon_session(&cap, &ns, false) {
                Ok(_) => {
                    self.model.sessions.remove(&ns);
                }
                Err(e) => {
                    let staged = self.model.sessions[&ns].staged_total();
                    self.record(
                        "F4-SESSION-WEDGED-WITH-STAGED-WORK",
                        format!(
                            "session {ns} could not finish: abandon without --discard \
                             gave {e:?} and every checkin failed [{}]. {staged} entries \
                             staged; the only remaining exit destroys them.",
                            errors.join("; ")
                        ),
                    );
                    // Force it open so the next session's verification is clean.
                    let _ = self.forge.abandon_session(&cap, &ns, true);
                    self.model.sessions.remove(&ns);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the driver
// ---------------------------------------------------------------------------

fn run_sequence(seed: u64, steps: usize) -> Vec<Finding> {
    let mut w = World::new(seed);
    let mut rng = Rng::new(seed);
    // Open one session first so the early steps have something to act on.
    w.op_open_session(&mut rng);
    w.verify();
    for _ in 0..steps {
        match rng.below(100) {
            0..=6 => w.op_open_session(&mut rng),
            7..=20 => w.op_mount(&mut rng),
            21..=45 => w.op_write(&mut rng),
            46..=51 => w.op_delete(&mut rng),
            52..=63 => w.op_read(&mut rng),
            64..=75 => w.op_ls(&mut rng),
            76..=90 => w.op_checkin(&mut rng),
            91..=94 => w.op_branch(&mut rng),
            95..=97 => w.op_abandon_session(&mut rng),
            _ => w.op_seal(&mut rng),
        }
        w.verify();
    }
    w.drain();
    w.verify();
    w.findings
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Default run: 8 sequences of 60 operations, with full model comparison,
/// object rehashing and `fsck --full` after every one of them -- 488 complete
/// verification passes. Measured at 13.6s in a debug build on an 8-core
/// laptop, which is CI-appropriate next to the suite's other multi-second
/// binaries (`many_agent_soak`, `cli_shared_stampede`).
///
/// `FORGEFS_MODEL_SEQUENCES` and `FORGEFS_MODEL_STEPS` widen it for a soak;
/// `FORGEFS_MODEL_SEEDS` replays one comma-separated list of seeds.
#[test]
fn model_based_composition() {
    let sequences = env_usize("FORGEFS_MODEL_SEQUENCES", 8);
    let steps = env_usize("FORGEFS_MODEL_STEPS", 60);
    let seeds: Vec<u64> = match std::env::var("FORGEFS_MODEL_SEEDS") {
        Ok(v) => v.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => (0..sequences as u64)
            .map(|i| 0x51_7C_C1_B7_27_22_0A_95u64.wrapping_mul(i + 1) ^ (i + 1))
            .collect(),
    };

    let mut all: BTreeMap<&'static str, Vec<(u64, String)>> = BTreeMap::new();
    for seed in &seeds {
        for f in run_sequence(*seed, steps) {
            all.entry(f.kind).or_default().push((*seed, f.detail));
        }
    }

    println!("\n=== model-based composition report ===");
    println!("sequences = {}, steps each = {}", seeds.len(), steps);
    for (kind, why) in KNOWN {
        let hits = all.get(kind).map(|v| v.len()).unwrap_or(0);
        println!("\n[{kind}] {hits} occurrence(s)\n  {why}");
        if let Some(v) = all.get(kind) {
            let (seed, detail) = &v[0];
            println!("  first seen with seed {seed}:\n    {detail}");
        }
    }
    println!("\n======================================\n");

    let missing: Vec<&str> = KNOWN
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !all.contains_key(k))
        .collect();
    assert!(
        missing.is_empty(),
        "these characterised defects were NOT reproduced: {missing:?}.\n\
         Either the generator got narrower or the defect was fixed. If it was \
         fixed, delete its row from KNOWN and make the model assert the correct \
         behaviour instead of characterising the broken one."
    );
}

/// The `#326` composition, written out by hand so the behaviour is readable
/// without running the generator. `mount` is correct, `write` is correct,
/// `checkin` is correct -- and composed they used to lose the write: a checkin
/// of `/` answered `Noop` with exit 0 while the entry sat under `/work` in no
/// ref at all, and `abandon` refused the same session as holding work. I22
/// makes that sentence impossible, and I19 makes the refusal actionable.
#[test]
fn i22_checkin_refuses_a_noop_over_work_it_did_not_fold() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    forge.branch(&root, "main", "shared").unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge.mount(&root, &ns, "/work", "ref:shared", true).unwrap();
    forge
        .write(&root, &ns, "/work/a.txt", b"hello", false)
        .unwrap();

    // The write is real: the session can see it.
    let listed = forge.ls(&root, &ns, "/work").unwrap();
    assert_eq!(
        listed.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
        ["a.txt"]
    );

    let refusal = forge
        .checkin(&root, &ns, "/", "publish")
        .expect_err("I22: a noop may not be reported over work the session holds");
    assert!(
        matches!(refusal, Error::Invalid(ref m) if m.contains("/work (1 staged entry)")),
        "the refusal must name the mount that holds the work: {refusal:?}"
    );
    assert_eq!(
        staged_entries(dir.path(), &ns),
        1,
        "I18: a refused checkin never destroys staged work"
    );

    // And the escape the diagnostic advises is actually available (I19/I21):
    // naming the mount publishes it, onto that mount's own base.
    assert!(matches!(
        forge.checkin(&root, &ns, "/work", "publish").unwrap(),
        CasResult::Updated { .. } | CasResult::Forked { .. }
    ));
    assert_eq!(forge.read(&root, &ns, "/work/a.txt").unwrap(), b"hello");
    forge
        .abandon_session(&root, &ns, false)
        .expect("a session whose work is published retires without discarding anything");
}

/// The liveness hole. `main` is protected, so a read-write mount on it can be
/// written but never checked in, and `abandon` refuses over staged work: the
/// session can neither publish nor explicitly abandon.
#[test]
fn liveness_session_with_staged_work_can_be_stranded() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();
    let ns = forge.session_open(&root, "main").unwrap();
    forge.mount(&root, &ns, "/w", "ref:main", true).unwrap();
    forge
        .write(&root, &ns, "/w/x.txt", b"staged", false)
        .unwrap();

    let publish = forge.checkin(&root, &ns, "/w", "publish").unwrap_err();
    let abandon = forge.abandon_session(&root, &ns, false).unwrap_err();
    assert!(
        matches!(publish, Error::Denied(_)) && matches!(abandon, Error::Invalid(_)),
        "characterising the liveness hole: publish={publish:?} abandon={abandon:?}"
    );
    // Nothing but an explicit discard, which destroys the work, gets out.
    assert_eq!(
        forge
            .abandon_session(&root, &ns, true)
            .unwrap()
            .discarded_overlay,
        1
    );
}

/// I19, the assertion that replaced this file's I8 characterisation: a
/// read-write `ref:R` mount at a non-root path shows `R`, not the session's own
/// base. Before per-mount pins it served the session's single global pin, so
/// `ls /w` hid a file that `heads/topic` plainly held.
#[test]
fn i19_rw_mount_serves_the_ref_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Forge::init(dir.path()).unwrap();
    let root = forge.root_cap().unwrap();

    let s0 = forge.session_open(&root, "main").unwrap();
    forge.write(&root, &s0, "/a.txt", b"A", false).unwrap();
    let CasResult::Updated { name: base_ref, .. } = forge.checkin(&root, &s0, "/", "base").unwrap()
    else {
        panic!("expected the private ref to advance")
    };
    forge.branch(&root, &base_ref, "heads/topic").unwrap();

    let s1 = forge.session_open(&root, "heads/topic").unwrap();
    forge.write(&root, &s1, "/t.txt", b"T", false).unwrap();
    let CasResult::Updated {
        name: topic_work, ..
    } = forge.checkin(&root, &s1, "/", "topic work").unwrap()
    else {
        panic!("expected the private ref to advance")
    };
    forge
        .merge(&root, "heads/topic", &topic_work, None)
        .unwrap();
    // heads/topic now holds {a.txt, t.txt}; base_ref still holds {a.txt}.

    let s2 = forge.session_open(&root, &base_ref).unwrap();
    forge
        .mount(&root, &s2, "/w", "ref:heads/topic", true)
        .unwrap();
    let mut names: Vec<String> = forge
        .ls(&root, &s2, "/w")
        .unwrap()
        .into_iter()
        .map(|e| e.0)
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["a.txt", "t.txt"],
        "I19: the mount must answer out of heads/topic, not out of the session's \
         own pinned base, which holds only a.txt"
    );
    assert_eq!(forge.read(&root, &s2, "/w/t.txt").unwrap(), b"T");

    // And the session's own `/` is unaffected: one mount's pin is not another's.
    assert!(matches!(
        forge.read(&root, &s2, "/t.txt").unwrap_err(),
        Error::NotFound(_)
    ));
}

fn staged_entries(dir: &Path, ns: &str) -> usize {
    let store = Store::open_read_only(&dir.join(".forge")).unwrap();
    store
        .meta
        .list_mounts(ns)
        .unwrap()
        .iter()
        .map(|m| store.meta.overlay_list(ns, &m.path).unwrap().len())
        .sum()
}
