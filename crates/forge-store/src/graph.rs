//! Durable, type-aware immutable object-graph traversal.

use crate::Store;
use forge_core::{decode_object_type, Blob, Commit, Conflict, Contribution, Snapshot, Tree};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use std::collections::{HashMap, HashSet, VecDeque};

pub const MAX_GRAPH_OBJECTS: usize = 1_000_000;

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
#[derive(Debug, Default)]
pub struct GraphWorkQueue {
    queue: VecDeque<GraphEdge>,
    scheduled: HashSet<(ObjectId, u8)>,
    scheduled_ids: HashSet<ObjectId>,
}

impl GraphWorkQueue {
    pub fn schedule(&mut self, edge: GraphEdge) -> Result<bool> {
        let constraint = (edge.id, expectation_key(edge.expected));
        if self.scheduled.contains(&constraint) {
            return Ok(false);
        }
        if !self.scheduled_ids.contains(&edge.id)
            && self.scheduled_ids.len() >= MAX_GRAPH_OBJECTS
        {
            return Err(Error::Corrupt(format!(
                "object graph exceeded {MAX_GRAPH_OBJECTS} objects"
            )));
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

impl Store {
    /// Rehash and canonically decode a complete typed object graph.
    pub fn reachable_graph_verified(
        &self,
        root: ObjectId,
        expected: ObjectType,
    ) -> Result<Vec<VerifiedGraphObject>> {
        let mut queue = GraphWorkQueue::default();
        queue.schedule(exact(
            root,
            expected,
            format!("root:{root}"),
        ))?;
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
            if verified.len() >= MAX_GRAPH_OBJECTS {
                return Err(Error::Corrupt(format!(
                    "object graph exceeded {MAX_GRAPH_OBJECTS} objects"
                )));
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
}
