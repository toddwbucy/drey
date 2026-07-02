//! The `Graph` handle and its mutation API (PRD §9.2).
//!
//! `Graph` owns the memory-primary [`Store`] and the [`GraphConfig`]. Query,
//! traversal, similarity, and export live in sibling modules as further `impl
//! Graph` blocks. Persistence is layered in at M2 behind the same `commit`
//! verb; until then an in-memory graph commits to nothing.

use std::collections::HashSet;

use crate::config::GraphConfig;
use crate::error::{Error, Result};
use crate::mutation::{
    DecayReport, EdgeFilter, PropertyPatch, RemoveNodeMode, WeightUpdate,
};
use crate::store::{apply_patch, Store};
use crate::types::{
    Edge, EdgeId, EdgeType, Embedding, Node, NodeId, NodeType, Properties,
};

/// An embedded property graph (PRD §9). Single writer: mutations take `&mut
/// self`, reads take `&self`, so the borrow checker forbids a read overlapping
/// a write without consumer-side synchronization (PRD §11).
pub struct Graph {
    pub(crate) store: Store,
    pub(crate) config: GraphConfig,
    /// The persistence backend, present when the graph is file-backed (M2).
    pub(crate) persist: Option<crate::persist::Persister>,
}

impl Graph {
    /// Create an in-memory graph with no persistence. Useful for tests and for
    /// consumers that want an ephemeral graph. File-backed graphs come from
    /// [`Graph::create`] / [`Graph::open`].
    pub fn in_memory(config: GraphConfig) -> Self {
        let mut store = Store::default();
        for p in &config.indexed_properties {
            store.indexed.insert((p.node_type.0.clone(), p.key.clone()));
        }
        Graph {
            store,
            config,
            persist: None,
        }
    }

    /// Whether the graph rejects mutations (opened read-only, PRD §9.1).
    fn ensure_writable(&self) -> Result<()> {
        if self.config.read_only {
            return Err(Error::Storage("graph is opened read-only".into()));
        }
        Ok(())
    }

    /// Register a node type with an optional embedding dimension (PRD §9.2).
    pub fn register_node_type(
        &mut self,
        node_type: NodeType,
        embedding_dim: Option<usize>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if let (Some(max), Some(dim)) = (self.config.max_embedding_dim, embedding_dim) {
            if dim > max {
                return Err(Error::InvalidNodeType(format!(
                    "embedding dim {dim} exceeds configured max {max}"
                )));
            }
        }
        self.store.register_node_type(&node_type, embedding_dim)?;
        self.log(Mutation::RegisterNodeType {
            node_type,
            embedding_dim,
        })
    }

    pub fn add_node(&mut self, node_type: NodeType, properties: Properties) -> Result<NodeId> {
        self.ensure_writable()?;
        let id = self.store.add_node(&node_type, properties.clone())?;
        self.log(Mutation::AddNode {
            id,
            node_type,
            properties,
        })?;
        Ok(id)
    }

    pub fn set_node_embedding(&mut self, node: NodeId, embedding: Embedding) -> Result<()> {
        self.ensure_writable()?;
        self.store.set_node_embedding(node, embedding.0.clone())?;
        self.log(Mutation::SetNodeEmbedding {
            node,
            embedding: embedding.0,
        })
    }

    pub fn update_node_properties(&mut self, node: NodeId, patch: PropertyPatch) -> Result<()> {
        self.ensure_writable()?;
        let old = self
            .store
            .nodes
            .get(&node.0)
            .ok_or(Error::NodeNotFound(node))?
            .properties
            .clone();
        let mut new = old.clone();
        apply_patch(&mut new, &patch.0);
        for (k, v) in &new {
            if !v.is_valid() {
                return Err(Error::InvalidPropertyValue(format!("property {k:?}")));
            }
        }
        self.store.reindex_node(node.0, &old, &new);
        self.store.nodes.get_mut(&node.0).unwrap().properties = new;
        self.log(Mutation::UpdateNodeProperties { node, patch: patch.0 })
    }

    pub fn remove_node(&mut self, node: NodeId, mode: RemoveNodeMode) -> Result<()> {
        self.ensure_writable()?;
        let remove_incident = mode == RemoveNodeMode::RemoveIncidentEdges;
        self.store.remove_node(node, remove_incident)?;
        self.log(Mutation::RemoveNode { node, mode })
    }

    pub fn add_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        edge_type: EdgeType,
        weight: f32,
        properties: Properties,
    ) -> Result<EdgeId> {
        self.ensure_writable()?;
        let id = self
            .store
            .add_edge(from, to, &edge_type, weight, properties.clone())?;
        self.log(Mutation::AddEdge {
            id,
            from,
            to,
            edge_type,
            weight,
            properties,
        })?;
        Ok(id)
    }

    pub fn update_edge_weight(&mut self, edge: EdgeId, update: WeightUpdate) -> Result<f32> {
        self.ensure_writable()?;
        let current = self
            .store
            .edges
            .get(&edge.0)
            .ok_or(Error::EdgeNotFound(edge))?
            .weight;
        let new = update.apply(current);
        self.store.set_edge_weight(edge, new)?;
        self.log(Mutation::SetEdgeWeight { edge, weight: new })?;
        Ok(new)
    }

    pub fn update_edge_properties(&mut self, edge: EdgeId, patch: PropertyPatch) -> Result<()> {
        self.ensure_writable()?;
        let rec = self.store.edges.get_mut(&edge.0).ok_or(Error::EdgeNotFound(edge))?;
        apply_patch(&mut rec.properties, &patch.0);
        self.log(Mutation::UpdateEdgeProperties { edge, patch: patch.0 })
    }

    pub fn remove_edge(&mut self, edge: EdgeId) -> Result<()> {
        self.ensure_writable()?;
        self.store.remove_edge(edge)?;
        self.log(Mutation::RemoveEdge { edge })
    }

    /// Multiply the weight of every edge matching `filter` by `factor`
    /// (PRD §9.2). Scheduling and the choice of factor are consumer policy;
    /// the crate only applies the batch.
    pub fn decay_edges(&mut self, filter: EdgeFilter, factor: f32) -> Result<DecayReport> {
        self.ensure_writable()?;
        let ids = self.edges_matching(&filter);
        let update = WeightUpdate::multiply(factor);
        for id in &ids {
            let current = self.store.edges[id].weight;
            let new = update.apply(current);
            self.store.edges.get_mut(id).unwrap().weight = new;
        }
        // A single log record captures the batch for replay.
        self.log(Mutation::DecayEdges {
            filter: filter.clone(),
            factor,
        })?;
        Ok(DecayReport { edges_decayed: ids.len() })
    }

    /// Make all prior mutations durable (PRD §9.2). In-memory graphs have
    /// nothing to persist; file-backed graphs flush the WAL (M2).
    pub fn commit(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if let Some(p) = self.persist.as_mut() {
            p.commit()?;
        }
        Ok(())
    }

    // ---- shared internal helpers ----

    /// Edge ids matching an [`EdgeFilter`], resolved through the edge-type index
    /// when the filter names types, else over all edges.
    pub(crate) fn edges_matching(&self, filter: &EdgeFilter) -> Vec<u64> {
        let candidates: Vec<u64> = if filter.edge_types.is_empty() {
            self.store.edges.keys().copied().collect()
        } else {
            let mut set = HashSet::new();
            for t in &filter.edge_types {
                if let Some(tid) = self.store.edge_types.get(t.as_str()) {
                    if let Some(ids) = self.store.edges_by_type.get(&tid) {
                        set.extend(ids.iter().copied());
                    }
                }
            }
            set.into_iter().collect()
        };
        let mut out: Vec<u64> = candidates
            .into_iter()
            .filter(|id| {
                let w = self.store.edges[id].weight;
                filter.min_weight.is_none_or(|min| w >= min)
            })
            .collect();
        out.sort_unstable(); // deterministic order (spec §5.4 / §4.1)
        out
    }

    // ---- simple lookups (fuller query API in query.rs) ----

    pub fn node(&self, id: NodeId) -> Result<Option<Node>> {
        Ok(self.store.materialize_node(id))
    }

    /// All node ids in the graph (unordered). Callers that need determinism
    /// sort the result.
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.store.nodes.keys().map(|n| NodeId(*n)).collect()
    }

    /// Node and edge counts, overall (PRD §15 observability).
    pub fn counts(&self) -> (usize, usize) {
        (self.store.nodes.len(), self.store.edges.len())
    }

    pub fn edge(&self, id: EdgeId) -> Result<Option<Edge>> {
        Ok(self.store.materialize_edge(id))
    }

    /// Append a mutation to the write-ahead log if persistence is active. For
    /// in-memory graphs this is a no-op. Kept private; every public mutation
    /// routes through it so the log is the exact mutation history (M2).
    fn log(&mut self, mutation: Mutation) -> Result<()> {
        if let Some(p) = self.persist.as_mut() {
            p.append(&mutation)?;
        }
        Ok(())
    }
}

/// The replayable unit of mutation. Every public mutation records exactly one
/// of these to the WAL, and open replays them (M2). Defined here so the
/// mutation API and the persistence layer share one vocabulary.
#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    RegisterNodeType {
        node_type: NodeType,
        embedding_dim: Option<usize>,
    },
    AddNode {
        id: NodeId,
        node_type: NodeType,
        properties: Properties,
    },
    SetNodeEmbedding {
        node: NodeId,
        embedding: Vec<f32>,
    },
    UpdateNodeProperties {
        node: NodeId,
        patch: std::collections::BTreeMap<String, Option<crate::types::Value>>,
    },
    RemoveNode {
        node: NodeId,
        mode: RemoveNodeMode,
    },
    AddEdge {
        id: EdgeId,
        from: NodeId,
        to: NodeId,
        edge_type: EdgeType,
        weight: f32,
        properties: Properties,
    },
    SetEdgeWeight {
        edge: EdgeId,
        weight: f32,
    },
    UpdateEdgeProperties {
        edge: EdgeId,
        patch: std::collections::BTreeMap<String, Option<crate::types::Value>>,
    },
    RemoveEdge {
        edge: EdgeId,
    },
    DecayEdges {
        filter: EdgeFilter,
        factor: f32,
    },
}
