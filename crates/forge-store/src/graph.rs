//! Durable, type-aware immutable object-graph traversal.

use crate::Store;
use forge_core::{
    decode_object_type, Blob, Commit, Conflict, Contribution, Snapshot, Tree,
};
use forge_types::{EntryKind, Error, ObjectId, ObjectType, Result};
use std::collections::{BTreeMap, VecDeque};

pub const MAX_GRAPH_OBJECTS: usize = 1_000_000;
const MAX_GRAPH_EDGES: usize = MAX_GRAPH_OBJECTS * 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphExpectation {
    Any,
    Exact(ObjectType),
}

impl GraphExpectation {
    fn verify(self, actual: ObjectType, id: ObjectId, resource: &str) -> Result<()> {
        match self {
            Self::Exact(expected) if actual != expected => Err(Error::Corrupt(format!(
                "typed edge {resource} expected {}, found {} at {id}",
                expected.as_str(),
                actual.as_str()
            ))),
            _ => Ok(()),
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
                edges.push(exact(
                    base,
                    ObjectType::Tree,
                    format!("conflict:{id}:base"),
                ));
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
        let mut queue = VecDeque::from([(
            root,
            GraphExpectation::Exact(expected),
            format!("root:{root}"),
        )]);
        let mut verified = BTreeMap::new();
        let mut edge_count = 0usize;

        while let Some((id, expectation, resource)) = queue.pop_front() {
            edge_count = edge_count
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("object graph edge count overflow".into()))?;
            if edge_count > MAX_GRAPH_EDGES {
                return Err(Error::Corrupt(format!(
                    "object graph exceeded {MAX_GRAPH_EDGES} edges"
                )));
            }
            if let Some(actual) = verified.get(&id).copied() {
                expectation.verify(actual, id, &resource)?;
                continue;
            }
            if verified.len() >= MAX_GRAPH_OBJECTS {
                return Err(Error::Corrupt(format!(
                    "object graph exceeded {MAX_GRAPH_OBJECTS} objects"
                )));
            }

            let bytes = self.get_raw_verified(id)?;
            let decoded = decode_graph_object(id, &bytes)?;
            expectation.verify(decoded.object_type, id, &resource)?;
            verified.insert(id, decoded.object_type);
            queue.extend(
                decoded
                    .edges
                    .into_iter()
                    .map(|edge| (edge.id, edge.expected, edge.resource)),
            );
        }

        Ok(verified
            .into_iter()
            .map(|(id, object_type)| VerifiedGraphObject { id, object_type })
            .collect())
    }
}
