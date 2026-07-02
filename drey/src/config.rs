//! Graph construction and operating policy (PRD §9.1).
//!
//! `GraphConfig` names the responsibilities the PRD §9.1 list assigns it. The
//! concrete field set is allowed to grow through M1; this is the v0.1 shape.

use crate::types::NodeType;

/// Which similarity scans are bounded, and by how much (PRD §13.1). Every
/// similarity query obeys this ceiling unless the caller explicitly opts into
/// an unbounded scan.
#[derive(Clone, Copy, Debug)]
pub struct ScanCeiling {
    /// Maximum candidate vectors scored in one similarity query.
    pub max_candidates: usize,
}

impl Default for ScanCeiling {
    fn default() -> Self {
        // Provisional; the M0 fixture/M3 gate tune this. Large enough that
        // agent-scale filtered candidate sets pass, small enough that an
        // accidental unfiltered scan is capped rather than turning into a full
        // vector-database sweep (PRD §13.1).
        ScanCeiling { max_candidates: 100_000 }
    }
}

/// A property selected for indexing (PRD §8). The crate does not index every
/// property by default; consumers opt in per `(node_type, key)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexedProperty {
    pub node_type: NodeType,
    pub key: String,
}

/// Graph construction and operating policy.
#[derive(Clone, Debug)]
pub struct GraphConfig {
    /// Properties to maintain a scalar index over (PRD §8).
    pub indexed_properties: Vec<IndexedProperty>,
    /// Similarity scan bound (PRD §13.1).
    pub scan_ceiling: ScanCeiling,
    /// Default maximum traversal step budget when a query does not set one.
    pub default_max_hops: usize,
    /// Open the graph read-only — contractual for inspection consumers
    /// (PRD §9.1, §18).
    pub read_only: bool,
    /// Maximum embedding dimensionality accepted at type registration.
    pub max_embedding_dim: Option<usize>,
    /// Acquire an advisory file lock against concurrent process writers
    /// (PRD §9.1 optional).
    pub file_lock: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        GraphConfig {
            indexed_properties: Vec::new(),
            scan_ceiling: ScanCeiling::default(),
            default_max_hops: 5,
            read_only: false,
            max_embedding_dim: None,
            file_lock: false,
        }
    }
}

impl GraphConfig {
    /// Builder helper: add an indexed property.
    pub fn with_indexed_property(mut self, node_type: NodeType, key: impl Into<String>) -> Self {
        self.indexed_properties.push(IndexedProperty {
            node_type,
            key: key.into(),
        });
        self
    }

    /// Builder helper: open read-only.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub(crate) fn is_indexed(&self, node_type: &NodeType, key: &str) -> bool {
        self.indexed_properties
            .iter()
            .any(|p| &p.node_type == node_type && p.key == key)
    }
}
