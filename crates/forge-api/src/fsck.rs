use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::{decode_object_type, Blob, Commit, Conflict, Contribution, Snapshot, Tree};
use forge_ns::{parse_spec, Spec};
use forge_store::{CatalogAudit, CatalogObjectExpectation};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

const MAX_OBJECTS: usize = 1_000_000;

#[derive(Clone, Copy, Debug)]
enum ObjectExpectation {
    Any,
    Exact(ObjectType),
    TreeEntry,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FsckFinding {
    pub code: String,
    pub resource: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FsckReport {
    pub ok: bool,
    pub full: bool,
    pub checked_refs: usize,
    pub checked_objects: usize,
    pub checked_namespaces: usize,
    pub findings: Vec<FsckFinding>,
}

impl FsckReport {
    fn new(full: bool) -> Self {
        Self {
            ok: true,
            full,
            checked_refs: 0,
            checked_objects: 0,
            checked_namespaces: 0,
            findings: Vec::new(),
        }
    }

    fn finding(&mut self, code: &str, resource: impl Into<String>, detail: impl Into<String>) {
        self.ok = false;
        self.findings.push(FsckFinding {
            code: code.to_string(),
            resource: resource.into(),
            detail: detail.into(),
        });
    }

    fn finish(&mut self) {
        self.findings.sort_by(|a, b| {
            (&a.code, &a.resource, &a.detail).cmp(&(&b.code, &b.resource, &b.detail))
        });
        self.findings.dedup();
    }
}

impl Forge {
    /// Re-resolve a ref that was absent from fsck's `refs` snapshot.
    ///
    /// fsck reads refs, then namespaces, then each namespace's mounts, as
    /// separate queries. A concurrent forking checkin commits a new
    /// `forks/<ref>/<agent>/<ulid>` and repoints the losing session's mount at
    /// it atomically, so an fsck that snapshotted refs *before* that commit and
    /// read mounts *after* it would see a mount naming a ref it never loaded
    /// and report MOUNT_REF corruption -- exit 2 on a repository whose bytes
    /// are intact. Forking is the designed outcome of losing a race, so the
    /// trigger is ordinary contention, not an exotic state.
    ///
    /// This is sound rather than a mitigation: nothing in this store ever
    /// deletes from `refs`, so a name that resolves now was necessarily created
    /// after the snapshot and is not corruption. Returning it also lets the
    /// caller adopt it as a graph root, so re-resolving costs no coverage.
    fn late_ref(&self, name: &str) -> Option<(ObjectId, ObjectType)> {
        let row = self.store.meta.get_ref(name).ok().flatten()?;
        let ty = object_type_for_kind(&row.kind).ok()?;
        Some((row.oid, ty))
    }

    fn collect_reachable_roots(
        &self,
        cap: &Cap,
        report: &mut FsckReport,
        roots: &mut Vec<(ObjectId, ObjectExpectation, String)>,
    ) -> Result<()> {
        // These production readers intentionally remain strict. Reachable fsck
        // uses the normal compatible-schema open and may overlap writers, so
        // `late_ref` handles the one valid cross-query transition: a newly
        // created fork ref and the atomically retargeted mount/live ref.
        let refs = self.store.meta.list_refs()?;
        report.checked_refs = refs.len();
        let mut ref_types = HashMap::new();

        for row in &refs {
            let expected = match object_type_for_kind(&row.kind) {
                Ok(ty) => ty,
                Err(detail) => {
                    report.finding("REF_KIND", format!("ref:{}", row.name), detail);
                    continue;
                }
            };
            ref_types.insert(row.name.clone(), (row.oid, expected));
            roots.push((
                row.oid,
                ObjectExpectation::Exact(expected),
                format!("ref:{}", row.name),
            ));

            if let Some(tag) = row.name.strip_prefix("tags/") {
                if !row.protected || !row.sealed || row.kind != "snapshot" {
                    report.finding(
                        "TAG_FLAGS",
                        format!("ref:{}", row.name),
                        "tag refs must be protected+sealed snapshots",
                    );
                }
                if let Err(err) = self.verify_tag(cap, tag) {
                    report.finding("SEAL", format!("tag:{tag}"), err.to_string());
                }
            }
        }

        let namespaces = self.store.meta.list_namespaces()?;
        report.checked_namespaces = namespaces.len();
        for ns in &namespaces {
            let ns_resource = format!("namespace:{}", ns.id);
            if let Some(pin) = ns.pinned_oid {
                roots.push((
                    pin,
                    ObjectExpectation::Exact(ObjectType::Commit),
                    format!("{ns_resource}:pin"),
                ));
            } else {
                report.finding("NS_PIN", &ns_resource, "namespace has no pinned commit");
            }

            if let Some(live) = &ns.live_ref {
                match ref_types.get(live) {
                    Some((oid, ty)) if *ty == ObjectType::Commit => roots.push((
                        *oid,
                        ObjectExpectation::Exact(ObjectType::Commit),
                        format!("{ns_resource}:live_ref:{live}"),
                    )),
                    Some((_oid, ty)) => report.finding(
                        "NS_LIVE_TYPE",
                        &ns_resource,
                        format!("live ref {live} is {}, expected commit", ty.as_str()),
                    ),
                    None => match self.late_ref(live) {
                        Some((oid, ObjectType::Commit)) => roots.push((
                            oid,
                            ObjectExpectation::Exact(ObjectType::Commit),
                            format!("{ns_resource}:live_ref:{live}"),
                        )),
                        Some((_oid, ty)) => report.finding(
                            "NS_LIVE_TYPE",
                            &ns_resource,
                            format!("live ref {live} is {}, expected commit", ty.as_str()),
                        ),
                        None => report.finding(
                            "NS_LIVE_REF",
                            &ns_resource,
                            format!("missing live ref {live}"),
                        ),
                    },
                }
            }

            let mounts = self.store.meta.list_mounts(&ns.id)?;
            for mount in &mounts {
                let mount_resource = format!("{ns_resource}:mount:{}", mount.path);
                match parse_spec(&mount.spec) {
                    Ok(Spec::Ref(name)) => match ref_types.get(&name) {
                        Some((oid, ty)) => roots.push((
                            *oid,
                            ObjectExpectation::Exact(*ty),
                            format!("{mount_resource}:ref:{name}"),
                        )),
                        None => match self.late_ref(&name) {
                            Some((oid, ty)) => roots.push((
                                oid,
                                ObjectExpectation::Exact(ty),
                                format!("{mount_resource}:ref:{name}"),
                            )),
                            None => report.finding(
                                "MOUNT_REF",
                                &mount_resource,
                                format!("missing ref {name}"),
                            ),
                        },
                    },
                    Ok(Spec::Oid(id)) => {
                        if mount.mode == "rw" {
                            report.finding(
                                "MOUNT_RW_OID",
                                &mount_resource,
                                "read-write mount cannot target immutable raw OID",
                            );
                        }
                        roots.push((id, ObjectExpectation::Any, format!("{mount_resource}:oid")));
                    }
                    Err(err) => report.finding("MOUNT_SPEC", &mount_resource, err.to_string()),
                }

                for row in self.store.meta.overlay_list(&ns.id, &mount.path)? {
                    if let Some(id) = row.blob_oid {
                        roots.push((
                            id,
                            ObjectExpectation::Exact(ObjectType::Blob),
                            format!("{mount_resource}:overlay:{}", row.path),
                        ));
                    }
                }
            }

            for observation in self.store.meta.observations(&ns.id)? {
                roots.push((
                    observation.oid,
                    ObjectExpectation::Exact(ObjectType::Blob),
                    format!(
                        "{ns_resource}:observation:{}:{}",
                        observation.mount, observation.path
                    ),
                ));
            }
        }
        Ok(())
    }

    fn collect_full_catalog(
        &self,
        catalog: CatalogAudit,
        report: &mut FsckReport,
        roots: &mut Vec<(ObjectId, ObjectExpectation, String)>,
    ) {
        report.checked_refs = catalog.refs.len();
        report.checked_namespaces = catalog.namespaces.len();

        // `None` means the row exists but its kind is invalid. The catalog
        // auditor already reports REF_KIND; do not turn a mount to that row
        // into a second, false MOUNT_REF finding.
        let mut ref_types = HashMap::new();
        for row in &catalog.refs {
            ref_types.insert(
                row.name.clone(),
                object_type_for_kind(&row.kind)
                    .ok()
                    .map(|expected| (row.oid, expected)),
            );
        }

        for mount in &catalog.mounts {
            let mount_resource = format!("namespace:{}:mount:{}", mount.ns_id, mount.path);
            match parse_spec(&mount.spec) {
                Ok(Spec::Ref(_)) if !catalog.refs_complete => {}
                Ok(Spec::Ref(name)) => match ref_types.get(&name) {
                    Some(Some((oid, expected))) => roots.push((
                        *oid,
                        ObjectExpectation::Exact(*expected),
                        format!("{mount_resource}:ref:{name}"),
                    )),
                    Some(None) => {}
                    None => {
                        report.finding("MOUNT_REF", &mount_resource, format!("missing ref {name}"))
                    }
                },
                Ok(Spec::Oid(id)) => {
                    if mount.mode == "rw" {
                        report.finding(
                            "MOUNT_RW_OID",
                            &mount_resource,
                            "read-write mount cannot target immutable raw OID",
                        );
                    }
                    roots.push((id, ObjectExpectation::Any, format!("{mount_resource}:oid")));
                }
                Err(error) => report.finding("MOUNT_SPEC", &mount_resource, error.to_string()),
            }
        }

        for seal in &catalog.seals {
            if let Err(error) =
                self.verify_seal_payload(&seal.tag, seal.snap_oid, seal.commit_oid, seal.tree_oid)
            {
                report.finding(
                    "SEAL_PAYLOAD",
                    format!("catalog:seal:{}", seal.tag),
                    error.to_string(),
                );
            }
        }
        for finding in catalog.findings {
            report.finding(&finding.code, finding.resource, finding.detail);
        }
        for root in catalog.roots {
            let expected = match root.expected {
                CatalogObjectExpectation::Any => ObjectExpectation::Any,
                CatalogObjectExpectation::Exact(expected) => ObjectExpectation::Exact(expected),
                CatalogObjectExpectation::TreeEntry => ObjectExpectation::TreeEntry,
            };
            roots.push((root.oid, expected, root.resource));
        }
    }

    /// Verify the repository from durable bytes. `full=false` verifies all
    /// metadata roots and reachable objects; `full=true` additionally proves
    /// one defensive catalog snapshot and scans every object file, including
    /// unreachable/orphan objects.
    pub fn fsck(&self, cap: &Cap, full: bool) -> Result<FsckReport> {
        self.check(cap, Op::Read, None)?;
        if !cap.has_unrestricted_ref_scope() {
            return Err(Error::Denied(
                "fsck requires unrestricted read authority".into(),
            ));
        }

        let mut report = FsckReport::new(full);
        for path in crate::repository::init_staging_siblings(self.root())? {
            report.finding(
                "INIT_STAGING",
                format!("path:{}", path.display()),
                "orphaned repository-initialization staging path; rerun `forge init` to reclaim it",
            );
        }
        let mut roots = Vec::new();

        if full {
            let catalog = self.store.meta.audit_catalog()?;
            self.collect_full_catalog(catalog, &mut report, &mut roots);
            scan_all_object_paths(&self.store.root(), &mut roots, &mut report)?;
        } else {
            self.collect_reachable_roots(cap, &mut report, &mut roots)?;
        }

        verify_graph(&self.store, roots, &mut report);
        report.finish();
        Ok(report)
    }
}

fn object_type_for_kind(kind: &str) -> std::result::Result<ObjectType, String> {
    match kind {
        "blob" => Ok(ObjectType::Blob),
        "tree" => Ok(ObjectType::Tree),
        "commit" => Ok(ObjectType::Commit),
        "conflict" => Ok(ObjectType::Conflict),
        "snapshot" => Ok(ObjectType::Snapshot),
        other => Err(format!("unknown ref kind {other}")),
    }
}

fn check_object_expectation(
    report: &mut FsckReport,
    id: ObjectId,
    actual: ObjectType,
    expected: ObjectExpectation,
    resource: &str,
) {
    let expected_name = match expected {
        ObjectExpectation::Any => None,
        ObjectExpectation::Exact(expected) if actual != expected => Some(expected.as_str()),
        ObjectExpectation::TreeEntry if !matches!(actual, ObjectType::Blob | ObjectType::Tree) => {
            Some("blob or tree")
        }
        _ => None,
    };
    if let Some(expected_name) = expected_name {
        report.finding(
            "TYPE_MISMATCH",
            resource,
            format!("{id} is {}, expected {expected_name}", actual.as_str()),
        );
    }
}

fn verify_graph(
    store: &forge_store::Store,
    roots: Vec<(ObjectId, ObjectExpectation, String)>,
    report: &mut FsckReport,
) {
    let mut queue: VecDeque<_> = roots.into();
    let mut verified: HashMap<ObjectId, ObjectType> = HashMap::new();
    let mut expanded = HashSet::new();
    let mut edges = 0usize;

    while let Some((id, expected, resource)) = queue.pop_front() {
        edges += 1;
        if edges > MAX_OBJECTS.saturating_mul(8) || verified.len() > MAX_OBJECTS {
            report.finding(
                "GRAPH_LIMIT",
                "object-graph",
                format!("verification exceeded {MAX_OBJECTS} objects"),
            );
            break;
        }

        if let Some(actual) = verified.get(&id).copied() {
            check_object_expectation(report, id, actual, expected, &resource);
            continue;
        }

        let bytes = match store.get_raw_verified(id) {
            Ok(bytes) => bytes,
            Err(err) => {
                report.finding("OBJECT_READ", resource, format!("{id}: {err}"));
                continue;
            }
        };
        let actual = match decode_object_type(&bytes) {
            Ok(ty) => ty,
            Err(err) => {
                report.finding("OBJECT_TYPE", resource, format!("{id}: {err}"));
                continue;
            }
        };
        report.checked_objects += 1;
        verified.insert(id, actual);

        check_object_expectation(report, id, actual, expected, &resource);

        if !expanded.insert(id) {
            continue;
        }

        let decode_result: Result<()> = match actual {
            ObjectType::Blob => Blob::decode(&bytes).map(|_| ()),
            ObjectType::Tree => Tree::decode(&bytes).map(|tree| {
                for entry in tree.entries {
                    let expected = match entry.kind {
                        EntryKind::Blob => ObjectType::Blob,
                        EntryKind::Tree => ObjectType::Tree,
                    };
                    queue.push_back((
                        entry.id,
                        ObjectExpectation::Exact(expected),
                        format!("tree:{id}:{}", entry.name),
                    ));
                }
            }),
            ObjectType::Commit => Commit::decode(&bytes).map(|commit| {
                queue.push_back((
                    commit.tree,
                    ObjectExpectation::Exact(ObjectType::Tree),
                    format!("commit:{id}:tree"),
                ));
                for parent in commit.parents {
                    queue.push_back((
                        parent,
                        ObjectExpectation::Exact(ObjectType::Commit),
                        format!("commit:{id}:parent"),
                    ));
                }
                if let Some(contrib) = commit.contrib {
                    queue.push_back((
                        contrib,
                        ObjectExpectation::Exact(ObjectType::Contribution),
                        format!("commit:{id}:contribution"),
                    ));
                }
            }),
            ObjectType::Snapshot => Snapshot::decode(&bytes).map(|snapshot| {
                queue.push_back((
                    snapshot.tree,
                    ObjectExpectation::Exact(ObjectType::Tree),
                    format!("snapshot:{id}:tree"),
                ));
                queue.push_back((
                    snapshot.commit,
                    ObjectExpectation::Exact(ObjectType::Commit),
                    format!("snapshot:{id}:commit"),
                ));
                queue.push_back((
                    snapshot.prov,
                    ObjectExpectation::Exact(ObjectType::Blob),
                    format!("snapshot:{id}:provenance"),
                ));
            }),
            ObjectType::Contribution => Contribution::decode(&bytes).map(|contribution| {
                queue.push_back((
                    contribution.base,
                    ObjectExpectation::Exact(ObjectType::Commit),
                    format!("contribution:{id}:base"),
                ));
                queue.push_back((
                    contribution.tree,
                    ObjectExpectation::Exact(ObjectType::Tree),
                    format!("contribution:{id}:tree"),
                ));
                for parent in contribution.parents {
                    queue.push_back((
                        parent,
                        ObjectExpectation::Exact(ObjectType::Commit),
                        format!("contribution:{id}:parent"),
                    ));
                }
                for read in contribution.reads {
                    queue.push_back((
                        read.id,
                        ObjectExpectation::Exact(ObjectType::Blob),
                        format!("contribution:{id}:read:{}", read.path),
                    ));
                }
            }),
            ObjectType::Conflict => Conflict::decode(&bytes).map(|conflict| {
                for base in conflict.bases {
                    queue.push_back((
                        base,
                        ObjectExpectation::Exact(ObjectType::Tree),
                        format!("conflict:{id}:base"),
                    ));
                }
                queue.push_back((
                    conflict.ours,
                    ObjectExpectation::Exact(ObjectType::Tree),
                    format!("conflict:{id}:ours"),
                ));
                queue.push_back((
                    conflict.theirs,
                    ObjectExpectation::Exact(ObjectType::Tree),
                    format!("conflict:{id}:theirs"),
                ));
                for causal in conflict.causal {
                    queue.push_back((
                        causal,
                        ObjectExpectation::Exact(ObjectType::Commit),
                        format!("conflict:{id}:causal"),
                    ));
                }
                for path in conflict.paths {
                    for edge in [path.a, path.b, path.base].into_iter().flatten() {
                        queue.push_back((
                            edge,
                            ObjectExpectation::Any,
                            format!("conflict:{id}:path:{}", path.path),
                        ));
                    }
                }
            }),
        };

        if let Err(err) = decode_result {
            report.finding("DECODE", resource, format!("{id}: {err}"));
        }
    }
}

fn scan_all_object_paths(
    root: &Path,
    roots: &mut Vec<(ObjectId, ObjectExpectation, String)>,
    report: &mut FsckReport,
) -> Result<()> {
    let objects = root.join("objects");
    for a in fs::read_dir(&objects)? {
        let a = a?;
        if !a.file_type()?.is_dir() {
            report.finding(
                "OBJECT_LAYOUT",
                a.path().display().to_string(),
                "expected first-level shard directory",
            );
            continue;
        }
        let a_name = a.file_name().to_string_lossy().into_owned();
        for b in fs::read_dir(a.path())? {
            let b = b?;
            if !b.file_type()?.is_dir() {
                report.finding(
                    "OBJECT_LAYOUT",
                    b.path().display().to_string(),
                    "expected second-level shard directory",
                );
                continue;
            }
            let b_name = b.file_name().to_string_lossy().into_owned();
            for file in fs::read_dir(b.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    report.finding(
                        "OBJECT_LAYOUT",
                        file.path().display().to_string(),
                        "object entry is not a regular file",
                    );
                    continue;
                }
                let name = file.file_name().to_string_lossy().into_owned();
                let id = match ObjectId::from_hex(&name) {
                    Ok(id) => id,
                    Err(err) => {
                        report.finding(
                            "OBJECT_NAME",
                            file.path().display().to_string(),
                            err.to_string(),
                        );
                        continue;
                    }
                };
                let (expected_a, expected_b) = id.shard_dirs();
                if a_name != expected_a || b_name != expected_b {
                    report.finding(
                        "OBJECT_SHARD",
                        file.path().display().to_string(),
                        format!(
                            "object {id} is under {a_name}/{b_name}, expected {expected_a}/{expected_b}"
                        ),
                    );
                }
                roots.push((id, ObjectExpectation::Any, format!("object:{id}")));
            }
        }
    }
    Ok(())
}
