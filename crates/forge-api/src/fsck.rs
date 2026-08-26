use crate::Forge;
use forge_cap::{Cap, Op};
use forge_core::decode_object_type;
use forge_ns::{parse_spec, Spec};
use forge_store::{
    decode_graph_object, CatalogAudit, CatalogObjectExpectation, GraphEdge, GraphExpectation,
    GraphWorkQueue, LedgerStanding, Observed, CURRENT_SCHEMA_VERSION,
};
use forge_types::{Error, ObjectId, ObjectType, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FsckFinding {
    pub code: String,
    pub resource: String,
    pub detail: String,
}

/// Why `fsck` declined to audit at all, as a document rather than as prose on
/// stderr.
///
/// `fsck --full --json` on a catalog from an older release used to write
/// nothing to stdout: the refusal existed only as an English sentence on
/// stderr, and a `--json` consumer got an empty stream and exit 1 with no way
/// to tell "not audited" from "audit produced nothing" except by parsing prose
/// (issue #356). Issue #348 made that outcome routine rather than rare -- it is
/// what every un-migrated repository now gets -- so the refusal is part of the
/// interface and needs a shape.
///
/// It is deliberately NOT an [`FsckReport`] with `ok: false`. A report says
/// "I looked and here is what I found"; this says "I did not look". Giving the
/// refusal `ok: false` and zero findings would be the same lie in JSON that
/// the empty stdout was in prose, so the two documents share no field layout
/// and this one leads with `schema`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FsckRefusal {
    /// Always `forgefs.fsck-refusal/1`, so no consumer can mistake this for a
    /// report.
    pub schema: &'static str,
    /// Always false: nothing was audited.
    pub audited: bool,
    /// Machine-stable classification, never the prose.
    pub reason: FsckRefusalReason,
    /// The catalog's metadata schema version, as found.
    pub schema_version: i64,
    /// What this binary understands.
    pub supported_schema_version: i64,
    /// The same sentence the non-JSON path prints, so the two cannot drift.
    pub detail: String,
}

/// The classifications a [`FsckRefusal`] can carry. Named, so a consumer can
/// branch on "migrate the repository" versus "upgrade the binary" without
/// matching on English.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FsckRefusalReason {
    /// The catalog predates this binary: open it once for writing to migrate.
    SchemaNeedsMigration,
    /// The catalog postdates this binary: upgrade forge.
    SchemaNewerThanSupported,
}

pub const FSCK_REFUSAL_SCHEMA: &str = "forgefs.fsck-refusal/1";

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
    /// `heads/agents/<agent>/forks/<ref>/<ulid>` and repoints the losing session's mount at
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
        roots: &mut Vec<(ObjectId, GraphExpectation, String)>,
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
                GraphExpectation::Exact(expected),
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
                    GraphExpectation::Exact(ObjectType::Commit),
                    format!("{ns_resource}:pin"),
                ));
            } else {
                report.finding("NS_PIN", &ns_resource, "namespace has no pinned commit");
            }

            if let Some(live) = &ns.live_ref {
                match ref_types.get(live) {
                    Some((oid, ty)) if *ty == ObjectType::Commit => roots.push((
                        *oid,
                        GraphExpectation::Exact(ObjectType::Commit),
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
                            GraphExpectation::Exact(ObjectType::Commit),
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
                            GraphExpectation::Exact(*ty),
                            format!("{mount_resource}:ref:{name}"),
                        )),
                        None => match self.late_ref(&name) {
                            Some((oid, ty)) => roots.push((
                                oid,
                                GraphExpectation::Exact(ty),
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
                        roots.push((id, GraphExpectation::Any, format!("{mount_resource}:oid")));
                    }
                    Err(err) => report.finding("MOUNT_SPEC", &mount_resource, err.to_string()),
                }

                // I19: the mount's own pinned base. The ref above is rooted at
                // what it holds now; this is the commit the mount still reads
                // and still folds onto, so a full walk must reread it too.
                if let Some(base) = mount.base_oid {
                    roots.push((
                        base,
                        GraphExpectation::Any,
                        format!("{mount_resource}:base"),
                    ));
                }

                for row in self.store.meta.overlay_list(&ns.id, &mount.path)? {
                    if let Some(id) = row.blob_oid {
                        roots.push((
                            id,
                            GraphExpectation::Exact(ObjectType::Blob),
                            format!("{mount_resource}:overlay:{}", row.path),
                        ));
                    }
                }
            }

            for observation in self.store.meta.observations(&ns.id)? {
                // An absent observation names no bytes and so roots nothing;
                // a directory observation roots the tree object it named.
                let expectation = match observation.seen {
                    Observed::Blob(_) => ObjectType::Blob,
                    Observed::Tree(_) => ObjectType::Tree,
                    Observed::Absent => continue,
                };
                let Some(oid) = observation.seen.oid() else {
                    continue;
                };
                roots.push((
                    oid,
                    GraphExpectation::Exact(expectation),
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
        roots: &mut Vec<(ObjectId, GraphExpectation, String)>,
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
                        GraphExpectation::Exact(*expected),
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
                    roots.push((id, GraphExpectation::Any, format!("{mount_resource}:oid")));
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
                CatalogObjectExpectation::Any => GraphExpectation::Any,
                CatalogObjectExpectation::Exact(expected) => GraphExpectation::Exact(expected),
                CatalogObjectExpectation::TreeEntry => GraphExpectation::TreeEntry,
            };
            roots.push((root.oid, expected, root.resource));
        }
    }

    /// Verify the repository from durable bytes. `full=false` verifies all
    /// metadata roots and reachable objects; `full=true` additionally proves
    /// one defensive catalog snapshot and scans every object file, including
    /// unreachable/orphan objects.
    /// The precondition `fsck` checks before auditing anything, as a value.
    ///
    /// `Ok(None)` means the audit may proceed. [`Forge::fsck`] calls this and
    /// turns a `Some` into the `Error::Invalid` it has always returned, so the
    /// prose path and the `--json` path are the same decision worded twice
    /// rather than two decisions that can disagree.
    pub fn fsck_refusal(&self, cap: &Cap, full: bool) -> Result<Option<FsckRefusal>> {
        if self.fsck_catalog && !full {
            return Err(Error::Invalid(
                "a ledger-deferred fsck handle requires full=true".into(),
            ));
        }
        self.check(cap, Op::Read, None)?;
        if !cap.has_unrestricted_ref_scope() {
            return Err(Error::Denied(
                "fsck requires unrestricted read authority".into(),
            ));
        }
        if !full {
            return Ok(None);
        }
        // A schema this binary cannot audit is not a verdict about the
        // repository. `fsck --full` alone opens through the ledger-deferred
        // path so it can REPORT a damaged ledger, and that deferral let an
        // intact repository from an older release reach an auditor that
        // knows only the current shape: it found a short ledger and a
        // `mounts` table without the column v3 added, called both defects,
        // and the CLI rendered them as exit 2 -- the code CLI_ABI.md
        // reserves for corruption -- on bytes that were entirely healthy
        // (issue #348).
        //
        // Refusing is deliberate, and it is the same answer the read-only
        // path already gives: `verify` and reachable `fsck` exit 1 here,
        // naming the version. Migrating first would be friendlier and is
        // the wrong instinct -- `fsck` is what an operator reaches for when
        // they are already worried, most often before deciding whether to
        // trust an upgrade, and a diagnostic tool must not silently rewrite
        // the catalog it was called to diagnose. So exit 2 keeps meaning
        // corruption, and this keeps meaning "not yet migrated".
        Ok(match self.store.meta.ledger_standing()? {
            LedgerStanding::NeedsMigration(version) => Some(FsckRefusal {
                schema: FSCK_REFUSAL_SCHEMA,
                audited: false,
                reason: FsckRefusalReason::SchemaNeedsMigration,
                schema_version: version,
                supported_schema_version: CURRENT_SCHEMA_VERSION,
                detail: format!(
                    "metadata schema version {version} needs migration to \
                     {CURRENT_SCHEMA_VERSION}, which a read-only check cannot perform; \
                     fsck will not migrate a repository it was asked to diagnose. \
                     Open the repository once for writing to migrate it (for example \
                     `forge --dir <repo> --cap <cap> refs`), then re-run `forge fsck --full`"
                ),
            }),
            LedgerStanding::Newer(version) => Some(FsckRefusal {
                schema: FSCK_REFUSAL_SCHEMA,
                audited: false,
                reason: FsckRefusalReason::SchemaNewerThanSupported,
                schema_version: version,
                supported_schema_version: CURRENT_SCHEMA_VERSION,
                detail: format!(
                    "metadata schema version {version} is newer than supported \
                     {CURRENT_SCHEMA_VERSION}; this binary does not know that catalog's \
                     invariants and cannot audit it. Upgrade forge, then re-run \
                     `forge fsck --full`"
                ),
            }),
            // Damaged is the case the deferral exists for: let the audit
            // run and report SCHEMA_LEDGER as the corruption it is.
            LedgerStanding::Current | LedgerStanding::Damaged => None,
        })
    }

    pub fn fsck(&self, cap: &Cap, full: bool) -> Result<FsckReport> {
        if let Some(refusal) = self.fsck_refusal(cap, full)? {
            return Err(Error::Invalid(refusal.detail));
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
            // The ledger standing was already decided by `fsck_refusal`
            // above; a catalog this binary cannot audit never gets here.
            let catalog = self.store.meta.audit_catalog()?;
            self.collect_full_catalog(catalog, &mut report, &mut roots);
            scan_all_object_paths(&self.store.root(), &mut roots, &mut report)?;
        } else {
            self.collect_reachable_roots(cap, &mut report, &mut roots)?;
        }

        // A walk this build will not finish is a refusal, not a verdict about
        // the repository (#359): it propagates as `Error::Invalid`, exit 1,
        // rather than becoming a finding that would make `report.ok` false and
        // send the CLI to exit 2 over intact bytes. Same reasoning as the
        // unauditable-schema refusal above.
        verify_graph(&self.store, roots, &mut report)?;
        report.finish();
        self.count_fsck(&report);
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
    expected: GraphExpectation,
    resource: &str,
) {
    if !expected.accepts(actual) {
        report.finding(
            "TYPE_MISMATCH",
            resource,
            format!(
                "{id} is {}, expected {}",
                actual.as_str(),
                expected.description()
            ),
        );
    }
}

/// Walk every root, recording what is wrong with the objects as findings.
///
/// The one thing it does NOT record as a finding is running out of walk
/// budget. `GraphWorkQueue` refuses past its ceiling, and that refusal says
/// nothing about the bytes -- every object walked so far verified -- so it
/// leaves as `Error::Invalid` and never as a `report.ok = false` the CLI turns
/// into exit 2 (#359).
fn verify_graph(
    store: &forge_store::Store,
    roots: Vec<(ObjectId, GraphExpectation, String)>,
    report: &mut FsckReport,
) -> Result<()> {
    let mut queue = GraphWorkQueue::default();
    for (id, expected, resource) in roots {
        queue.schedule(GraphEdge {
            id,
            expected,
            resource,
        })?;
    }
    let mut verified: HashMap<ObjectId, ObjectType> = HashMap::new();

    while let Some(edge) = queue.pop_front() {
        let GraphEdge {
            id,
            expected,
            resource,
        } = edge;

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

        let decoded = match decode_graph_object(id, &bytes) {
            Ok(decoded) => decoded,
            Err(err) => {
                report.finding("DECODE", resource, format!("{id}: {err}"));
                continue;
            }
        };
        for edge in decoded.edges {
            queue.schedule(edge)?;
        }
    }
    Ok(())
}

fn scan_all_object_paths(
    root: &Path,
    roots: &mut Vec<(ObjectId, GraphExpectation, String)>,
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
                roots.push((id, GraphExpectation::Any, format!("object:{id}")));
            }
        }
    }
    Ok(())
}
