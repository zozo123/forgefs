//! Durable, type-aware immutable object-graph traversal.

use crate::Store;
use forge_core::{decode_object_type, Blob, Commit, Conflict, Contribution, Snapshot, Tree};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use std::collections::{HashMap, HashSet, VecDeque};

/// Most distinct objects one graph WALK will hold in memory at a time.
///
/// This is a bound on a TRAVERSAL, not on an object (#359). `MAX_TREE_ENTRIES`
/// says "no VERSION 1 Tree may hold more than N entries", so bytes that exceed
/// it cannot have come from any encoder this binary contains and reading them
/// back IS damage. Nothing of the kind is true here: a repository grows past a
/// million objects through ordinary `import` and `checkin`, every object file
/// still rehashes to its own name and every typed edge still resolves. So
/// exceeding this bound is `Error::Invalid` -- exit 1, this build will not walk
/// that much in one pass -- and never `Error::Corrupt`, exit 2, which
/// CLI_ABI.md reserves for damaged bytes. `fsck` and `gc` are exactly what an
/// operator reaches for when a repository has grown large, and they were the
/// two commands that answered "corrupt".
///
/// The bound is not decorative and is not removed. Every walk in the tree is a
/// whole-set walk: `reachable_graph_verified` returns one `Vec` of everything
/// reachable, `gc::walk` fills one `HashSet` of it, and `fsck::verify_graph`
/// one `HashMap`. None is incremental, resumable or spillable, so removing the
/// bound converts a refusal that names itself into an OOM kill that reports no
/// exit code at all. Raising it is the operator's call, because only the
/// operator knows the machine: see `MAX_GRAPH_OBJECTS_ENV`.
pub const DEFAULT_MAX_GRAPH_OBJECTS: usize = 1_000_000;

/// Overrides `DEFAULT_MAX_GRAPH_OBJECTS` for one process.
///
/// Read once per `GraphWorkQueue`, which is once per walk, and never on a hot
/// path. A value that is absent, unparseable or zero takes the default rather
/// than failing the command: this variable can only make a walk that already
/// refuses succeed, so a typo must not be able to break a walk that was fine.
///
/// Walk state costs roughly a hundred bytes per distinct object, so the
/// default ceiling is worth ~100 MB of resident memory and raising it is a
/// statement about available RAM. Tests use it to exercise the classification
/// without a million real objects.
pub const MAX_GRAPH_OBJECTS_ENV: &str = "FORGEFS_MAX_GRAPH_OBJECTS";

/// The walk ceiling in force for this process.
pub fn max_graph_objects() -> usize {
    std::env::var(MAX_GRAPH_OBJECTS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_GRAPH_OBJECTS)
}

/// The one wording for "this build will not walk a graph this large".
///
/// `Invalid`, so it is exit 1 and not exit 2: the request is refused, nothing
/// is being reported about the repository's bytes. It names the ceiling, says
/// the bytes are intact, and names the two things an operator can do.
fn walk_limit_exceeded(limit: usize) -> Error {
    Error::Invalid(format!(
        "object graph walk reached this build's ceiling of {limit} objects. \
         The repository is not corrupt; this is a memory bound on the WALK, not \
         a bound on any object, and no object was found damaged. Re-run with \
         {MAX_GRAPH_OBJECTS_ENV}=<n> above {limit} (the walk holds roughly 100 \
         bytes per object, so budget that much RAM), or reduce what is reachable \
         -- `forge gc --dry-run` reports it -- and re-run. See docs/GC.md."
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphExpectation {
    Any,
    Exact(ObjectType),
    TreeEntry,
}

impl GraphExpectation {
    pub fn accepts(self, actual: ObjectType) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => actual == expected,
            Self::TreeEntry => matches!(actual, ObjectType::Blob | ObjectType::Tree),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Any => "any object",
            Self::Exact(expected) => expected.as_str(),
            Self::TreeEntry => "blob or tree",
        }
    }

    fn verify(self, actual: ObjectType, id: ObjectId, resource: &str) -> Result<()> {
        if self.accepts(actual) {
            Ok(())
        } else {
            Err(Error::Corrupt(format!(
                "typed edge {resource} expected {}, found {} at {id}",
                self.description(),
                actual.as_str()
            )))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    pub id: ObjectId,
    pub expected: GraphExpectation,
    pub resource: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedGraphObject {
    pub object_type: ObjectType,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedGraphObject {
    pub id: ObjectId,
    pub object_type: ObjectType,
}

fn exact(id: ObjectId, expected: ObjectType, resource: String) -> GraphEdge {
    GraphEdge {
        id,
        expected: GraphExpectation::Exact(expected),
        resource,
    }
}

fn any(id: ObjectId, resource: String) -> GraphEdge {
    GraphEdge {
        id,
        expected: GraphExpectation::Any,
        resource,
    }
}

fn expectation_key(expectation: GraphExpectation) -> u8 {
    match expectation {
        GraphExpectation::Any => 0,
        GraphExpectation::Exact(object_type) => object_type as u8,
        GraphExpectation::TreeEntry => u8::MAX,
    }
}

/// Bounded queue of distinct `(ObjectId, expected type)` graph constraints.
///
/// Repeated edges do not consume work, while incompatible expectations for
/// one ObjectId remain independent constraints and are all verified.
#[derive(Debug)]
pub struct GraphWorkQueue {
    queue: VecDeque<GraphEdge>,
    scheduled: HashSet<(ObjectId, u8)>,
    scheduled_ids: HashSet<ObjectId>,
    limit: usize,
}

impl Default for GraphWorkQueue {
    fn default() -> Self {
        Self::with_limit(max_graph_objects())
    }
}

impl GraphWorkQueue {
    /// A queue with an explicit ceiling. `Default` reads it from the
    /// environment once per walk; this is how a test drives the refusal
    /// without a million real objects.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            scheduled: HashSet::new(),
            scheduled_ids: HashSet::new(),
            limit,
        }
    }

    /// This walk's ceiling, so a caller that owns its own accumulator can
    /// enforce the same one.
    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn schedule(&mut self, edge: GraphEdge) -> Result<bool> {
        let constraint = (edge.id, expectation_key(edge.expected));
        if self.scheduled.contains(&constraint) {
            return Ok(false);
        }
        if !self.scheduled_ids.contains(&edge.id) && self.scheduled_ids.len() >= self.limit {
            return Err(walk_limit_exceeded(self.limit));
        }
        self.scheduled.insert(constraint);
        self.scheduled_ids.insert(edge.id);
        self.queue.push_back(edge);
        Ok(true)
    }

    pub fn pop_front(&mut self) -> Option<GraphEdge> {
        self.queue.pop_front()
    }
}

/// Decode one canonical object and return every typed outbound edge.
///
/// This is the single immutable graph-shape owner used by fail-fast seal
/// verification and finding-collecting fsck traversal.
pub fn decode_graph_object(id: ObjectId, bytes: &[u8]) -> Result<DecodedGraphObject> {
    let object_type = decode_object_type(bytes)?;
    let mut edges = Vec::new();
    match object_type {
        ObjectType::Blob => {
            Blob::decode(bytes)?;
        }
        ObjectType::Tree => {
            let tree = Tree::decode(bytes)?;
            for entry in tree.entries {
                let expected = match entry.kind {
                    EntryKind::Blob => ObjectType::Blob,
                    EntryKind::Tree => ObjectType::Tree,
                };
                edges.push(exact(
                    entry.id,
                    expected,
                    format!("tree:{id}:{}", entry.name),
                ));
            }
        }
        ObjectType::Commit => {
            let commit = Commit::decode(bytes)?;
            edges.push(exact(
                commit.tree,
                ObjectType::Tree,
                format!("commit:{id}:tree"),
            ));
            for parent in commit.parents {
                edges.push(exact(
                    parent,
                    ObjectType::Commit,
                    format!("commit:{id}:parent"),
                ));
            }
            if let Some(contribution) = commit.contrib {
                edges.push(exact(
                    contribution,
                    ObjectType::Contribution,
                    format!("commit:{id}:contribution"),
                ));
            }
        }
        ObjectType::Conflict => {
            let conflict = Conflict::decode(bytes)?;
            for base in conflict.bases {
                edges.push(exact(base, ObjectType::Tree, format!("conflict:{id}:base")));
            }
            edges.push(exact(
                conflict.ours,
                ObjectType::Tree,
                format!("conflict:{id}:ours"),
            ));
            edges.push(exact(
                conflict.theirs,
                ObjectType::Tree,
                format!("conflict:{id}:theirs"),
            ));
            for causal in conflict.causal {
                edges.push(exact(
                    causal,
                    ObjectType::Commit,
                    format!("conflict:{id}:causal"),
                ));
            }
            for path in conflict.paths {
                for edge in [path.a, path.b, path.base].into_iter().flatten() {
                    edges.push(any(edge, format!("conflict:{id}:path:{}", path.path)));
                }
            }
        }
        ObjectType::Snapshot => {
            let snapshot = Snapshot::decode(bytes)?;
            edges.push(exact(
                snapshot.tree,
                ObjectType::Tree,
                format!("snapshot:{id}:tree"),
            ));
            edges.push(exact(
                snapshot.commit,
                ObjectType::Commit,
                format!("snapshot:{id}:commit"),
            ));
            edges.push(exact(
                snapshot.prov,
                ObjectType::Blob,
                format!("snapshot:{id}:provenance"),
            ));
        }
        ObjectType::Contribution => {
            let contribution = Contribution::decode(bytes)?;
            edges.push(exact(
                contribution.base,
                ObjectType::Commit,
                format!("contribution:{id}:base"),
            ));
            edges.push(exact(
                contribution.tree,
                ObjectType::Tree,
                format!("contribution:{id}:tree"),
            ));
            for parent in contribution.parents {
                edges.push(exact(
                    parent,
                    ObjectType::Commit,
                    format!("contribution:{id}:parent"),
                ));
            }
            for read in contribution.reads {
                edges.push(exact(
                    read.id,
                    ObjectType::Blob,
                    format!("contribution:{id}:read:{}", read.path),
                ));
            }
        }
    }
    Ok(DecodedGraphObject { object_type, edges })
}

impl<O: crate::ObjectStore> Store<O> {
    /// Rehash and canonically decode a complete typed object graph.
    pub fn reachable_graph_verified(
        &self,
        root: ObjectId,
        expected: ObjectType,
    ) -> Result<Vec<VerifiedGraphObject>> {
        let mut queue = GraphWorkQueue::default();
        queue.schedule(exact(root, expected, format!("root:{root}")))?;
        let mut verified = HashMap::new();

        while let Some(edge) = queue.pop_front() {
            let GraphEdge {
                id,
                expected,
                resource,
            } = edge;
            if let Some(actual) = verified.get(&id).copied() {
                expected.verify(actual, id, &resource)?;
                continue;
            }
            if verified.len() >= queue.limit() {
                return Err(walk_limit_exceeded(queue.limit()));
            }

            let bytes = self.get_raw_verified(id)?;
            let decoded = decode_graph_object(id, &bytes)?;
            expected.verify(decoded.object_type, id, &resource)?;
            verified.insert(id, decoded.object_type);
            for edge in decoded.edges {
                queue.schedule(edge)?;
            }
        }

        let mut objects = verified
            .into_iter()
            .map(|(id, object_type)| VerifiedGraphObject { id, object_type })
            .collect::<Vec<_>>();
        objects.sort_by_key(|object| object.id);
        Ok(objects)
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    #[test]
    fn repeated_edges_cost_one_constraint_but_conflicting_types_are_preserved() {
        let id = ObjectId([7; 32]);
        let mut queue = GraphWorkQueue::default();
        for n in 0..100_000 {
            queue
                .schedule(exact(id, ObjectType::Blob, format!("read:{n}")))
                .unwrap();
        }
        queue
            .schedule(exact(id, ObjectType::Tree, "conflicting-edge".into()))
            .unwrap();

        assert_eq!(queue.scheduled_ids.len(), 1);
        assert_eq!(queue.scheduled.len(), 2);
        assert_eq!(queue.queue.len(), 2);
    }

    /// #359: the ceiling is a resource bound on a WALK, so exceeding it is
    /// `Invalid` (exit 1) and never `Corrupt` (exit 2). It used to be `Corrupt`,
    /// which told an operator whose bytes were entirely intact that their
    /// repository was damaged, from `fsck` and `gc` -- the two commands they
    /// run when a repository has grown large.
    #[test]
    fn exceeding_the_walk_ceiling_is_a_refusal_not_a_corruption_report() {
        let mut queue = GraphWorkQueue::with_limit(2);
        queue
            .schedule(any(ObjectId([1; 32]), "root".into()))
            .unwrap();
        queue
            .schedule(any(ObjectId([2; 32]), "second".into()))
            .unwrap();
        // A THIRD expectation on an id already scheduled costs no new object.
        queue
            .schedule(exact(ObjectId([2; 32]), ObjectType::Tree, "again".into()))
            .unwrap();

        let error = queue
            .schedule(any(ObjectId([3; 32]), "third".into()))
            .unwrap_err();
        let Error::Invalid(message) = &error else {
            panic!("the walk ceiling must not be reported as corruption: {error:?}");
        };
        assert!(message.contains("not corrupt"), "{message}");
        assert!(message.contains(MAX_GRAPH_OBJECTS_ENV), "{message}");
    }

    /// The env override may only ever raise a walk that already refuses, so an
    /// unusable value takes the default rather than failing a walk that was
    /// fine. Read here rather than in a `#[test]` that sets the variable,
    /// because tests share one process and `set_var` is not thread-safe; the
    /// CLI side is covered end to end in
    /// `forge-cli/tests/cli_graph_walk_bound.rs`.
    #[test]
    fn the_default_ceiling_is_unchanged_when_nothing_overrides_it() {
        assert_eq!(DEFAULT_MAX_GRAPH_OBJECTS, 1_000_000);
        if std::env::var(MAX_GRAPH_OBJECTS_ENV).is_err() {
            assert_eq!(max_graph_objects(), DEFAULT_MAX_GRAPH_OBJECTS);
            assert_eq!(GraphWorkQueue::default().limit(), DEFAULT_MAX_GRAPH_OBJECTS);
        }
    }
}
