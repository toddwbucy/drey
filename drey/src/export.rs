//! Graph-feature export for structural-embedding pipelines (PRD §9.5, §14).
//!
//! The crate exposes topology and feature arrays; it does not choose or depend
//! on a training library (PRD §14). Export is deterministic (for
//! reproducibility), filterable by edge type and minimum weight, and
//! framework-agnostic. GraphSAGE and RGCN are example consumers only.

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::mutation::EdgeFilter;
use crate::types::NodeId;

/// What node properties/embeddings to project into a feature matrix (PRD §9.5).
#[derive(Clone, Default, Debug)]
pub struct FeatureSpec {
    /// Include the stored embedding as leading feature columns.
    pub include_embedding: bool,
    /// Scalar numeric property keys to append as feature columns, in order.
    /// Non-numeric or missing values contribute `0.0`.
    pub numeric_properties: Vec<String>,
}

/// A deterministic, contiguous mapping between `NodeId` and a dense `0..n`
/// index space, plus its inverse (PRD §14 required export forms).
#[derive(Clone, Debug)]
pub struct NodeIndexMap {
    /// `dense index -> NodeId`, sorted by `NodeId` for determinism.
    pub to_node: Vec<NodeId>,
}

impl NodeIndexMap {
    pub fn len(&self) -> usize {
        self.to_node.len()
    }
    pub fn is_empty(&self) -> bool {
        self.to_node.is_empty()
    }
    /// Dense index of a node, if present.
    pub fn index_of(&self, node: NodeId) -> Option<usize> {
        self.to_node.binary_search(&node).ok()
    }
}

/// Stable graph-feature export surface (PRD §9.5). Implemented for [`Graph`].
pub trait GraphFeatureExport {
    fn node_count(&self) -> usize;
    fn edge_count(&self) -> usize;

    /// Deterministic dense node-index mapping.
    fn node_index_map(&self) -> NodeIndexMap;

    /// Node feature matrix, one row per dense index, per the spec.
    fn node_features(&self, map: &NodeIndexMap, spec: &FeatureSpec) -> Result<Vec<Vec<f32>>>;

    /// Edge index as `(src_dense, dst_dense)` pairs, filtered and deterministic.
    fn edge_index(&self, map: &NodeIndexMap, filter: &EdgeFilter) -> Result<Vec<(usize, usize)>>;

    /// Edge weights aligned to [`GraphFeatureExport::edge_index`].
    fn edge_weights(&self, filter: &EdgeFilter) -> Result<Vec<f32>>;

    /// Edge type ids aligned to [`GraphFeatureExport::edge_index`].
    fn edge_types(&self, filter: &EdgeFilter) -> Result<Vec<u32>>;
}

impl GraphFeatureExport for Graph {
    fn node_count(&self) -> usize {
        self.store.nodes.len()
    }

    fn edge_count(&self) -> usize {
        self.store.edges.len()
    }

    fn node_index_map(&self) -> NodeIndexMap {
        let mut ids: Vec<NodeId> = self.store.nodes.keys().map(|n| NodeId(*n)).collect();
        ids.sort_unstable();
        NodeIndexMap { to_node: ids }
    }

    fn node_features(&self, map: &NodeIndexMap, spec: &FeatureSpec) -> Result<Vec<Vec<f32>>> {
        let mut rows = Vec::with_capacity(map.len());
        for node in &map.to_node {
            let rec = &self.store.nodes[&node.0];
            let mut row = Vec::new();
            if spec.include_embedding {
                if let Some(emb) = &rec.embedding {
                    row.extend_from_slice(emb);
                }
            }
            for key in &spec.numeric_properties {
                let v = rec.properties.get(key).and_then(numeric_of).unwrap_or(0.0);
                row.push(v);
            }
            rows.push(row);
        }
        Ok(rows)
    }

    fn edge_index(&self, map: &NodeIndexMap, filter: &EdgeFilter) -> Result<Vec<(usize, usize)>> {
        let ids = self.edges_matching(filter);
        let mut out = Vec::with_capacity(ids.len());
        for e in ids {
            let rec = &self.store.edges[&e];
            // An edge whose endpoint is absent from the index means the loaded
            // graph is inconsistent; surface a recoverable error, never panic.
            let src = map.index_of(NodeId(rec.from)).ok_or_else(|| {
                Error::IndexCorruption(format!(
                    "edge {e} references missing source node {}",
                    rec.from
                ))
            })?;
            let dst = map.index_of(NodeId(rec.to)).ok_or_else(|| {
                Error::IndexCorruption(format!(
                    "edge {e} references missing target node {}",
                    rec.to
                ))
            })?;
            out.push((src, dst));
        }
        Ok(out)
    }

    fn edge_weights(&self, filter: &EdgeFilter) -> Result<Vec<f32>> {
        Ok(self
            .edges_matching(filter)
            .into_iter()
            .map(|e| self.store.edges[&e].weight)
            .collect())
    }

    fn edge_types(&self, filter: &EdgeFilter) -> Result<Vec<u32>> {
        Ok(self
            .edges_matching(filter)
            .into_iter()
            .map(|e| self.store.edges[&e].edge_type)
            .collect())
    }
}

fn numeric_of(v: &crate::types::Value) -> Option<f32> {
    match v {
        crate::types::Value::I64(i) => Some(*i as f32),
        crate::types::Value::F64(x) => Some(*x as f32),
        crate::types::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}
