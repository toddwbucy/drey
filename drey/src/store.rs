//! The in-memory graph and its indexes (PRD §8, §10.1).
//!
//! Memory-primary: this structure *is* the graph. All queries, traversal, and
//! similarity run against it; disk is reconstruction-only and never on the
//! query path (PRD §10.1). The store owns:
//!
//! - node and edge records keyed by durable id,
//! - the monotonic id allocators,
//! - the type interners,
//! - and the required index set of PRD §8.
//!
//! Ids are durable and explicit: allocators only ever increase, and a removed
//! id is never reused, so an `EdgeId` referencing a `NodeId` stays valid across
//! reload (PRD §7.4).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::error::{Error, Result};
use crate::interner::Interner;
use crate::types::{Edge, EdgeId, EdgeType, Node, NodeId, NodeType, Properties, ScalarKey, Value};

/// Internal node record. `node_type` is interned to a `u32`.
#[derive(Clone, Debug)]
pub(crate) struct NodeRecord {
    pub node_type: u32,
    pub properties: Properties,
    pub embedding: Option<Vec<f32>>,
}

/// Internal edge record. `from`/`to`/`edge_type` are the wiring; weight is a
/// first-class mutable field (PRD §7.2).
#[derive(Clone, Debug)]
pub(crate) struct EdgeRecord {
    pub from: u64,
    pub to: u64,
    pub edge_type: u32,
    pub weight: f32,
    pub properties: Properties,
}

/// The memory-primary store.
#[derive(Default)]
pub(crate) struct Store {
    pub(crate) nodes: HashMap<u64, NodeRecord>,
    pub(crate) edges: HashMap<u64, EdgeRecord>,

    pub(crate) next_node_id: u64,
    pub(crate) next_edge_id: u64,

    pub(crate) node_types: Interner,
    pub(crate) edge_types: Interner,

    /// Declared embedding dimension per registered node type (`None` = no
    /// embeddings for that type). Registration is required before use.
    pub(crate) embedding_dim: HashMap<u32, Option<usize>>,

    // ---- Index set (PRD §8) ----
    /// Outbound adjacency: `from_node -> edge_type -> [edge_id]`. Nested so a
    /// node's edges resolve in O(degree), and a type-filtered lookup is a single
    /// inner-map hit — never a scan of the whole index.
    pub(crate) out_adj: HashMap<u64, BTreeMap<u32, Vec<u64>>>,
    /// Inbound adjacency: `to_node -> edge_type -> [edge_id]`.
    pub(crate) in_adj: HashMap<u64, BTreeMap<u32, Vec<u64>>>,
    /// Node type → node ids.
    pub(crate) nodes_by_type: HashMap<u32, Vec<u64>>,
    /// Edge type → edge ids.
    pub(crate) edges_by_type: HashMap<u32, Vec<u64>>,
    /// Scalar property index: `(node_type, key) -> value -> [node_id]`,
    /// ordered so both equality and range resolve through it (PRD §8).
    pub(crate) prop_index: HashMap<(u32, String), BTreeMap<ScalarKey, Vec<u64>>>,

    /// Which `(node_type, key)` pairs are indexed. Interned type ids are only
    /// known after registration, so this holds label pairs and is consulted by
    /// label at mutation time.
    pub(crate) indexed: HashSet<(String, String)>,
}

impl Store {
    /// Register a node type with an optional embedding dimension. Re-registering
    /// with a different dimension is an error (PRD §7.1 dimension enforcement).
    pub(crate) fn register_node_type(
        &mut self,
        node_type: &NodeType,
        embedding_dim: Option<usize>,
    ) -> Result<()> {
        validate_label(node_type.as_str(), Error::InvalidNodeType)?;
        let id = self.node_types.intern(node_type.as_str());
        match self.embedding_dim.get(&id) {
            Some(existing) if *existing != embedding_dim => Err(Error::InvalidNodeType(format!(
                "node type {:?} already registered with embedding dim {:?}, cannot re-register with {:?}",
                node_type.as_str(),
                existing,
                embedding_dim
            ))),
            _ => {
                self.embedding_dim.insert(id, embedding_dim);
                self.nodes_by_type.entry(id).or_default();
                Ok(())
            }
        }
    }

    pub(crate) fn require_registered(&self, node_type: &NodeType) -> Result<u32> {
        self.node_types
            .get(node_type.as_str())
            .filter(|id| self.embedding_dim.contains_key(id))
            .ok_or_else(|| {
                Error::InvalidNodeType(format!("node type {:?} not registered", node_type.as_str()))
            })
    }

    pub(crate) fn add_node(
        &mut self,
        node_type: &NodeType,
        properties: Properties,
    ) -> Result<NodeId> {
        let type_id = self.require_registered(node_type)?;
        validate_properties(&properties)?;
        let id = self.next_node_id;
        self.next_node_id += 1;

        self.nodes_by_type.entry(type_id).or_default().push(id);
        self.index_node_properties(type_id, node_type, id, &properties);
        self.nodes.insert(
            id,
            NodeRecord {
                node_type: type_id,
                properties,
                embedding: None,
            },
        );
        Ok(NodeId(id))
    }

    pub(crate) fn set_node_embedding(&mut self, node: NodeId, embedding: Vec<f32>) -> Result<()> {
        // Resolve the node first: a call against a missing id must report
        // NodeNotFound, not a finiteness/dimension error attributed to a node
        // that does not exist.
        let rec = self.nodes.get(&node.0).ok_or(Error::NodeNotFound(node))?;
        let declared = self.embedding_dim.get(&rec.node_type).copied().flatten();
        // Non-finite components (NaN/±inf) poison similarity scoring — a NaN score
        // sorts to the top of a cosine/dot top-k — so reject them at the boundary.
        if let Some(bad) = embedding.iter().position(|x| !x.is_finite()) {
            return Err(Error::InvalidPropertyValue(format!(
                "embedding component {bad} is not finite ({})",
                embedding[bad]
            )));
        }
        match declared {
            None => Err(Error::InvalidNodeType(
                "node type was not registered with an embedding dimension".into(),
            )),
            Some(dim) if dim != embedding.len() => Err(Error::DimensionMismatch {
                expected: dim,
                actual: embedding.len(),
            }),
            Some(_) => {
                self.nodes.get_mut(&node.0).unwrap().embedding = Some(embedding);
                Ok(())
            }
        }
    }

    pub(crate) fn add_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        edge_type: &EdgeType,
        weight: f32,
        properties: Properties,
    ) -> Result<EdgeId> {
        if !self.nodes.contains_key(&from.0) {
            return Err(Error::NodeNotFound(from));
        }
        if !self.nodes.contains_key(&to.0) {
            return Err(Error::NodeNotFound(to));
        }
        // Enforce the value ceiling on edge properties too. `add_node` and both
        // property-update paths validate; edge creation must not be the one hole
        // through which an oversized String/Bytes/List (whose length the codec
        // prefixes with a u32) reaches the WAL/snapshot and corrupts it.
        validate_properties(&properties)?;
        // A NaN/±inf weight is malformed input: it silently becomes a zero-cost
        // edge in weighted shortest_path (f32::max(NaN, 0.0) == 0.0) and defeats
        // the min_weight filter. Validated here — not only in the public API —
        // so WAL replay and snapshot load pass through the same gate.
        validate_weight(weight)?;
        validate_label(edge_type.as_str(), Error::InvalidEdgeType)?;
        let type_id = self.edge_types.intern(edge_type.as_str());
        let id = self.next_edge_id;
        self.next_edge_id += 1;

        adj_insert(&mut self.out_adj, from.0, type_id, id);
        adj_insert(&mut self.in_adj, to.0, type_id, id);
        self.edges_by_type.entry(type_id).or_default().push(id);
        self.edges.insert(
            id,
            EdgeRecord {
                from: from.0,
                to: to.0,
                edge_type: type_id,
                weight,
                properties,
            },
        );
        Ok(EdgeId(id))
    }

    pub(crate) fn set_edge_weight(&mut self, edge: EdgeId, weight: f32) -> Result<f32> {
        // Store-side finiteness gate: `update_edge_weight` and `decay_edges`
        // validate their computed results with richer messages first, but this
        // is the backstop every path — including WAL replay — must pass.
        validate_weight(weight)?;
        let rec = self
            .edges
            .get_mut(&edge.0)
            .ok_or(Error::EdgeNotFound(edge))?;
        rec.weight = weight;
        Ok(weight)
    }

    /// Apply a property patch to a node, validating the merged map before any
    /// mutation. Shared by the public mutation API and WAL replay so both apply
    /// identical semantics — a missing node is a typed error on both paths,
    /// never a silent no-op.
    pub(crate) fn update_node_properties(
        &mut self,
        node: NodeId,
        patch: &BTreeMap<String, Option<Value>>,
    ) -> Result<()> {
        let old = self
            .nodes
            .get(&node.0)
            .ok_or(Error::NodeNotFound(node))?
            .properties
            .clone();
        let mut new = old.clone();
        apply_patch(&mut new, patch);
        validate_properties(&new)?;
        self.reindex_node(node.0, &old, &new);
        self.nodes.get_mut(&node.0).unwrap().properties = new;
        Ok(())
    }

    /// Apply a property patch to an edge. Shared by the public API and WAL
    /// replay (see [`Store::update_node_properties`]).
    pub(crate) fn update_edge_properties(
        &mut self,
        edge: EdgeId,
        patch: &BTreeMap<String, Option<Value>>,
    ) -> Result<()> {
        for (k, v) in patch {
            validate_key(k)?;
            if let Some(v) = v {
                if !v.is_valid() {
                    return Err(Error::InvalidPropertyValue(format!(
                        "property {k:?} is not a valid v0.1 value"
                    )));
                }
            }
        }
        let rec = self
            .edges
            .get_mut(&edge.0)
            .ok_or(Error::EdgeNotFound(edge))?;
        apply_patch(&mut rec.properties, patch);
        Ok(())
    }

    pub(crate) fn remove_edge(&mut self, edge: EdgeId) -> Result<()> {
        let rec = self
            .edges
            .remove(&edge.0)
            .ok_or(Error::EdgeNotFound(edge))?;
        adj_remove(&mut self.out_adj, rec.from, rec.edge_type, edge.0);
        adj_remove(&mut self.in_adj, rec.to, rec.edge_type, edge.0);
        remove_from_vec(&mut self.edges_by_type, rec.edge_type, edge.0);
        Ok(())
    }

    /// Remove a node. Callers pass `remove_incident` per `RemoveNodeMode`; when
    /// false and incident edges exist, the removal is rejected so edges cannot
    /// be orphaned (PRD §9.2 default).
    pub(crate) fn remove_node(&mut self, node: NodeId, remove_incident: bool) -> Result<()> {
        if !self.nodes.contains_key(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let incident = self.incident_edges(node.0);
        if !incident.is_empty() {
            if !remove_incident {
                return Err(Error::DanglingEdge(node));
            }
            for e in incident {
                self.remove_edge(EdgeId(e))?;
            }
        }
        let rec = self.nodes.remove(&node.0).unwrap();
        let node_type = self.node_types.label(rec.node_type).unwrap().to_string();
        remove_from_vec(&mut self.nodes_by_type, rec.node_type, node.0);
        self.unindex_node_properties(rec.node_type, &NodeType(node_type), node.0, &rec.properties);
        Ok(())
    }

    /// Every edge id incident to a node, in either direction. O(degree).
    pub(crate) fn incident_edges(&self, node: u64) -> Vec<u64> {
        let mut out = Vec::new();
        if let Some(types) = self.out_adj.get(&node) {
            for edges in types.values() {
                out.extend_from_slice(edges);
            }
        }
        if let Some(types) = self.in_adj.get(&node) {
            for edges in types.values() {
                out.extend_from_slice(edges);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    // ---- Property index maintenance ----

    fn index_node_properties(
        &mut self,
        type_id: u32,
        node_type: &NodeType,
        node: u64,
        properties: &Properties,
    ) {
        for (key, value) in properties {
            if !self.indexed.contains(&(node_type.0.clone(), key.clone())) {
                continue;
            }
            if let Some(scalar) = value.as_scalar() {
                self.prop_index
                    .entry((type_id, key.clone()))
                    .or_default()
                    .entry(ScalarKey(scalar))
                    .or_default()
                    .push(node);
            }
        }
    }

    fn unindex_node_properties(
        &mut self,
        type_id: u32,
        node_type: &NodeType,
        node: u64,
        properties: &Properties,
    ) {
        for (key, value) in properties {
            if !self.indexed.contains(&(node_type.0.clone(), key.clone())) {
                continue;
            }
            if let Some(scalar) = value.as_scalar() {
                if let Some(tree) = self.prop_index.get_mut(&(type_id, key.clone())) {
                    let key_now_empty =
                        if let Some(bucket) = tree.get_mut(&ScalarKey(scalar.clone())) {
                            bucket.retain(|n| *n != node);
                            bucket.is_empty()
                        } else {
                            false
                        };
                    // Prune emptied containers so the index does not accumulate
                    // tombstones keyed by dead values (unbounded growth + range
                    // scans over dead keys), matching remove_from_vec / adj_remove.
                    if key_now_empty {
                        tree.remove(&ScalarKey(scalar));
                    }
                    if tree.is_empty() {
                        self.prop_index.remove(&(type_id, key.clone()));
                    }
                }
            }
        }
    }

    /// Re-index a single node's properties after a property patch: the caller
    /// passes the old and new property maps so the index moves exactly.
    pub(crate) fn reindex_node(&mut self, node: u64, old: &Properties, new: &Properties) {
        let type_id = self.nodes[&node].node_type;
        let node_type = NodeType(self.node_types.label(type_id).unwrap().to_string());
        self.unindex_node_properties(type_id, &node_type, node, old);
        self.index_node_properties(type_id, &node_type, node, new);
    }

    // ---- Load-time raw inserts (persistence reconstruction, PRD §10.1) ----
    //
    // These insert records with explicit, caller-supplied ids and rebuild the
    // derived indexes, but do not allocate ids or validate. They exist so a
    // snapshot/WAL replay restores the exact id space (PRD §7.4) rather than
    // renumbering, and so indexes are rebuilt from the source-of-truth records
    // rather than persisted (guaranteeing index consistency after reload, §17).

    /// Insert a node with an explicit id during load.
    pub(crate) fn insert_node_raw(&mut self, id: u64, rec: NodeRecord) {
        let type_id = rec.node_type;
        self.nodes_by_type.entry(type_id).or_default().push(id);
        let node_type = NodeType(self.node_types.label(type_id).unwrap().to_string());
        let props = rec.properties.clone();
        self.nodes.insert(id, rec);
        self.index_node_properties(type_id, &node_type, id, &props);
    }

    /// Insert an edge with an explicit id during load.
    pub(crate) fn insert_edge_raw(&mut self, id: u64, rec: EdgeRecord) {
        adj_insert(&mut self.out_adj, rec.from, rec.edge_type, id);
        adj_insert(&mut self.in_adj, rec.to, rec.edge_type, id);
        self.edges_by_type
            .entry(rec.edge_type)
            .or_default()
            .push(id);
        self.edges.insert(id, rec);
    }

    /// After raw inserts, adjacency/type buckets are in insertion order. Sort
    /// them so load order does not affect query result order (determinism).
    pub(crate) fn sort_indexes(&mut self) {
        for types in self.out_adj.values_mut() {
            for v in types.values_mut() {
                v.sort_unstable();
            }
        }
        for types in self.in_adj.values_mut() {
            for v in types.values_mut() {
                v.sort_unstable();
            }
        }
        for v in self.nodes_by_type.values_mut() {
            v.sort_unstable();
        }
        for v in self.edges_by_type.values_mut() {
            v.sort_unstable();
        }
        for tree in self.prop_index.values_mut() {
            for bucket in tree.values_mut() {
                bucket.sort_unstable();
            }
        }
    }

    // ---- Materializers ----

    pub(crate) fn materialize_node(&self, id: NodeId) -> Option<Node> {
        let rec = self.nodes.get(&id.0)?;
        Some(Node {
            id,
            node_type: NodeType(self.node_types.label(rec.node_type).unwrap().to_string()),
            properties: rec.properties.clone(),
            embedding: rec.embedding.clone().map(crate::types::Embedding),
        })
    }

    pub(crate) fn materialize_edge(&self, id: EdgeId) -> Option<Edge> {
        let rec = self.edges.get(&id.0)?;
        Some(Edge {
            id,
            from: NodeId(rec.from),
            to: NodeId(rec.to),
            edge_type: EdgeType(self.edge_types.label(rec.edge_type).unwrap().to_string()),
            weight: rec.weight,
            properties: rec.properties.clone(),
        })
    }
}

/// Remove a value from a `HashMap<K, Vec<u64>>` bucket, dropping the bucket if
/// it empties. Used for the type indexes.
fn remove_from_vec<K: std::hash::Hash + Eq>(map: &mut HashMap<K, Vec<u64>>, key: K, value: u64) {
    if let Some(v) = map.get_mut(&key) {
        v.retain(|x| *x != value);
        if v.is_empty() {
            map.remove(&key);
        }
    }
}

/// Insert an edge id into a nested `node -> edge_type -> [edge_id]` adjacency.
fn adj_insert(adj: &mut HashMap<u64, BTreeMap<u32, Vec<u64>>>, node: u64, etype: u32, edge: u64) {
    adj.entry(node)
        .or_default()
        .entry(etype)
        .or_default()
        .push(edge);
}

/// Remove an edge id from a nested adjacency, pruning empty levels.
fn adj_remove(adj: &mut HashMap<u64, BTreeMap<u32, Vec<u64>>>, node: u64, etype: u32, edge: u64) {
    if let Some(types) = adj.get_mut(&node) {
        if let Some(v) = types.get_mut(&etype) {
            v.retain(|x| *x != edge);
            if v.is_empty() {
                types.remove(&etype);
            }
        }
        if types.is_empty() {
            adj.remove(&node);
        }
    }
}

/// Validate a whole property map: every key within the codec's u32 length
/// prefix, every value a well-formed v0.1 value. The single gate shared by
/// node/edge creation, both property-update paths, WAL replay, and snapshot
/// load — so no path can admit a value another path would reject.
pub(crate) fn validate_properties(properties: &Properties) -> Result<()> {
    for (k, v) in properties {
        validate_key(k)?;
        if !v.is_valid() {
            return Err(Error::InvalidPropertyValue(format!(
                "property {k:?} is not a valid v0.1 value"
            )));
        }
    }
    Ok(())
}

/// The on-disk codec prefixes every string with a u32 length; a key longer
/// than that would silently wrap the prefix and corrupt the WAL/snapshot.
pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.len() > u32::MAX as usize {
        return Err(Error::InvalidPropertyValue(
            "property key exceeds the u32 codec length limit".into(),
        ));
    }
    Ok(())
}

/// Same u32-length-prefix ceiling for interned type labels (node and edge
/// type names), which the snapshot persists as length-prefixed strings.
/// `err` wraps the message in the caller's error category (node vs edge type).
pub(crate) fn validate_label(label: &str, err: fn(String) -> Error) -> Result<()> {
    if label.len() > u32::MAX as usize {
        return Err(err(
            "type label exceeds the u32 codec length limit".to_string()
        ));
    }
    Ok(())
}

/// Reject a non-finite edge weight (NaN/±inf): it would poison weighted
/// shortest_path and the min_weight filter. Applied at the store so the live
/// API, WAL replay, and snapshot load share one gate.
pub(crate) fn validate_weight(weight: f32) -> Result<()> {
    if !weight.is_finite() {
        return Err(Error::InvalidPropertyValue(
            "edge weight must be finite (not NaN or infinite)".into(),
        ));
    }
    Ok(())
}

/// Helper for property-patch application shared with the public API.
pub(crate) fn apply_patch(properties: &mut Properties, patch: &BTreeMap<String, Option<Value>>) {
    for (k, v) in patch {
        match v {
            Some(value) => {
                properties.insert(k.clone(), value.clone());
            }
            None => {
                properties.remove(k);
            }
        }
    }
}
