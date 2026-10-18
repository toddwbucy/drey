//! Version-one engine contracts. All limits are explicit; schemas carry no meaning.
use crate::{Direction, EdgeId, NodeId, Properties, Scalar};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ENGINE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Cosine,
    Dot,
    Euclidean,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Space {
    pub model: String,
    pub revision: String,
    pub dimensions: usize,
    pub metric: Metric,
    pub unit_normalized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vector {
    pub space: Space,
    pub values: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_edges: usize,
    /// Conservative resident-data charge, not an OS RSS guarantee.
    pub max_data_bytes: usize,
    pub max_batch_bytes: usize,
    pub max_operations: usize,
    pub max_receipts: usize,
    pub max_query_work: usize,
    pub max_candidates: usize,
    pub max_result_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 100_000,
            max_edges: 500_000,
            max_data_bytes: 512 * 1024 * 1024,
            max_batch_bytes: 8 * 1024 * 1024,
            max_operations: 1024,
            max_receipts: 100_000,
            max_query_work: 100_000,
            max_candidates: 20_000,
            max_result_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub types: BTreeMap<String, Option<Space>>,
    /// Opt-in `(node_type, property_key)` scalar indexes, restored on open.
    /// Property names have no special indexing behavior or uniqueness guarantee.
    pub indexes: Vec<(String, String)>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRef {
    Id(NodeId),
    Created(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Write {
    CreateNode {
        key: String,
        node_type: String,
        properties: Properties,
        document: Option<String>,
        embedding: Option<Vector>,
    },
    ReplaceNode {
        id: NodeId,
        properties: Properties,
        document: Option<String>,
        embedding: Option<Vector>,
    },
    /// Incident edges must be deleted explicitly earlier in this batch.
    DeleteNode {
        id: NodeId,
    },
    CreateEdge {
        key: String,
        from: NodeRef,
        to: NodeRef,
        edge_type: String,
        weight: f32,
        properties: Properties,
    },
    ReplaceEdge {
        id: EdgeId,
        weight: f32,
        properties: Properties,
    },
    DeleteEdge {
        id: EdgeId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub token: String,
    pub operations: Vec<Write>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub token: String,
    pub sequence: u64,
    pub nodes: BTreeMap<String, NodeId>,
    pub edges: BTreeMap<String, EdgeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    Eq(Scalar),
    Range {
        min: Option<Scalar>,
        max: Option<Scalar>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub key: String,
    pub predicate: Predicate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Walk {
    pub direction: Direction,
    pub edge_types: Vec<String>,
    pub max_hops: usize,
    pub min_weight: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "select", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selection {
    Type { node_type: String },
    Ids { ids: Vec<NodeId> },
    Reachable { from: NodeId, walk: Walk },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rank {
    pub vector: Vector,
    pub k: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    /// None selects all public properties; an empty list selects none.
    pub fields: Option<Vec<String>>,
    pub document: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub selection: Selection,
    pub filters: Vec<Filter>,
    pub rank: Option<Rank>,
    /// Applied to winners before expansion. Unranked order is ascending ID.
    pub limit: usize,
    pub expand: Option<Walk>,
    pub projection: Projection,
    pub related_projection: Projection,
    /// Caller can reduce, never raise, the database's work ceiling.
    pub work_limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub id: NodeId,
    pub node_type: String,
    pub properties: Properties,
    pub document: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relationship {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    pub edge_type: String,
    pub weight: f32,
    pub properties: Properties,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Related {
    pub document: Document,
    pub via: Relationship,
    pub depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub document: Document,
    pub score: Option<f64>,
    pub related: Vec<Related>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResult {
    pub sequence: u64,
    /// Logical visits: index seek, each matching range bucket, and selected
    /// node/adjacency visits. Tree comparisons and serialization are not counted.
    pub work: usize,
    pub candidates: usize,
    pub hits: Vec<Hit>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub version: u32,
    pub sequence: u64,
    pub nodes: usize,
    pub edges: usize,
    pub receipts: usize,
    pub data_charge_bytes: usize,
}
