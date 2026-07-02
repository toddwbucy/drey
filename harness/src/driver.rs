//! Drivers (spec §5.1).
//!
//! [`GraphDriver`] is the seam the runner measures against. [`NaiveDriver`]
//! (HashMaps + linear scans) exists only to prove the harness mechanics
//! end-to-end; its numbers are never a comparison baseline. [`DreyDriver`] wraps
//! the real crate and is what the M3 budget gate measures.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value as Json;

use drey::config::GraphConfig;
use drey::mutation::{EdgeFilter, WeightUpdate};
use drey::query::{PropertyQuery, ScalarPredicate};
use drey::similarity::{SimilarityMetric, SimilarityQuery};
use drey::traverse::{
    CostMode, DirectionOpt, NeighborOptions, ShortestPathOptions, TraversalOptions,
};
use drey::types::{Embedding, NodeType, Scalar, Value};
use drey::{EdgeType, Graph, NodeId, EdgeId};

use crate::generator::Fixture;
use crate::workload::{Dir, WorkloadOp};

/// Load-time facts a driver reports back.
#[derive(Clone, Copy, Debug)]
pub struct LoadStats {
    pub nodes: usize,
    pub edges: usize,
}

/// Whether an op was measured, is not applicable yet, or errored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpStatus {
    Ok,
    NotApplicable,
    Error,
}

/// The result of running one op: status plus counters that ride along with
/// timing for correctness spot-checks (spec §5.1).
#[derive(Clone, Debug)]
pub struct OpOutcome {
    pub status: OpStatus,
    pub counters: BTreeMap<String, u64>,
}

impl OpOutcome {
    fn ok(counters: BTreeMap<String, u64>) -> Self {
        OpOutcome { status: OpStatus::Ok, counters }
    }
    fn na() -> Self {
        OpOutcome { status: OpStatus::NotApplicable, counters: BTreeMap::new() }
    }
}

fn c(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// The measured surface.
pub trait GraphDriver {
    fn name(&self) -> String;
    fn load_fixture(&mut self, fx: &Fixture) -> Result<LoadStats, String>;
    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome, String>;
}

/// Convert a fixture JSON property value to a drey `Value`. Fixtures only ever
/// carry the scalar shapes below; anything else (array, object, JSON null) means
/// the generator produced something unexpected, so fail loudly rather than
/// silently coercing it to `Null` and hiding the problem.
fn to_value(j: &Json) -> Value {
    match j {
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) if n.is_i64() => Value::I64(n.as_i64().unwrap()),
        Json::Number(n) => Value::F64(n.as_f64().unwrap()),
        Json::String(s) => Value::String(s.clone()),
        other => panic!("unexpected fixture property value (not a supported scalar): {other}"),
    }
}

fn dir(d: Dir) -> DirectionOpt {
    match d {
        Dir::Outbound => DirectionOpt::Outbound,
        Dir::Inbound => DirectionOpt::Inbound,
        Dir::Both => DirectionOpt::Both,
    }
}

// ---- DreyDriver ----

/// Wraps a real in-memory `drey::Graph`. Fixture ids map 1:1 onto drey ids
/// because nodes and edges are added in ascending id order with no removals, so
/// drey's monotonic allocator assigns the same ids.
pub struct DreyDriver {
    graph: Option<Graph>,
}

impl DreyDriver {
    pub fn new() -> Self {
        DreyDriver { graph: None }
    }
    fn g(&self) -> &Graph {
        self.graph.as_ref().expect("fixture not loaded")
    }
    fn g_mut(&mut self) -> &mut Graph {
        self.graph.as_mut().expect("fixture not loaded")
    }
}

impl Default for DreyDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphDriver for DreyDriver {
    fn name(&self) -> String {
        format!("drey@{}", env!("CARGO_PKG_VERSION"))
    }

    fn load_fixture(&mut self, fx: &Fixture) -> Result<LoadStats, String> {
        // Index p_seq (I64) on every node type so property_eq/range hit the index.
        let mut config = GraphConfig::default();
        for t in 0..fx.params.node_types {
            config = config.with_indexed_property(NodeType::new(format!("nt_{t:02}")), "p_seq");
        }
        let mut g = Graph::in_memory(config);
        for t in 0..fx.params.node_types {
            g.register_node_type(NodeType::new(format!("nt_{t:02}")), Some(fx.params.embed_dim as usize))
                .map_err(|e| e.to_string())?;
        }
        for n in &fx.nodes {
            let props: BTreeMap<String, Value> =
                n.props.iter().map(|(k, v)| (k.clone(), to_value(v))).collect();
            g.add_node(NodeType::new(n.node_type.clone()), props)
                .map_err(|e| e.to_string())?;
        }
        for (id, v) in &fx.embeddings {
            g.set_node_embedding(NodeId(*id), Embedding::new(v.clone()))
                .map_err(|e| e.to_string())?;
        }
        for e in &fx.edges {
            let props: BTreeMap<String, Value> =
                e.props.iter().map(|(k, v)| (k.clone(), to_value(v))).collect();
            g.add_edge(NodeId(e.from), NodeId(e.to), EdgeType::new(e.edge_type.clone()), e.weight, props)
                .map_err(|e| e.to_string())?;
        }
        let stats = LoadStats { nodes: fx.nodes.len(), edges: fx.edges.len() };
        self.graph = Some(g);
        Ok(stats)
    }

    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome, String> {
        match op {
            WorkloadOp::Neighbors { start, dir: d, edge_type, min_weight } => {
                let opts = NeighborOptions {
                    direction: dir(*d),
                    edge_types: edge_type.iter().map(|t| EdgeType::new(t.clone())).collect(),
                    min_weight: *min_weight,
                };
                let ns = self.g().neighbors(NodeId(*start), opts).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("neighbors", ns.len() as u64)])))
            }
            WorkloadOp::Traverse { start, max_hops } => {
                let paths = self
                    .g()
                    .traverse(NodeId(*start), TraversalOptions { max_hops: *max_hops, max_paths: 1000, ..Default::default() })
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("paths_returned", paths.len() as u64)])))
            }
            WorkloadOp::ShortestPath { from, to, weighted, max_steps } => {
                let opts = ShortestPathOptions {
                    cost_mode: if *weighted { CostMode::WeightedCost } else { CostMode::UnweightedHops },
                    max_steps: *max_steps,
                    ..Default::default()
                };
                let path = self.g().shortest_path(NodeId(*from), NodeId(*to), opts).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("found", path.is_some() as u64)])))
            }
            WorkloadOp::PropertyEq { node_type, key, ivalue } => {
                let hits = self
                    .g()
                    .nodes_by_property(PropertyQuery {
                        node_type: NodeType::new(node_type.clone()),
                        key: key.clone(),
                        predicate: ScalarPredicate::Eq(Scalar::I64(*ivalue)),
                    })
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("hits", hits.len() as u64)])))
            }
            WorkloadOp::PropertyRange { node_type, key, min, max } => {
                let hits = self
                    .g()
                    .nodes_by_property(PropertyQuery {
                        node_type: NodeType::new(node_type.clone()),
                        key: key.clone(),
                        predicate: ScalarPredicate::Range {
                            min: Some(Scalar::I64(*min)),
                            max: Some(Scalar::I64(*max)),
                        },
                    })
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("hits", hits.len() as u64)])))
            }
            WorkloadOp::Similar { seed_node, k, node_type } => {
                let emb = self
                    .g()
                    .node(NodeId(*seed_node))
                    .map_err(|e| e.to_string())?
                    .and_then(|n| n.embedding)
                    .ok_or("seed node has no embedding")?;
                // Compose a node-type filter so the scan is bounded to the
                // budget's ≤10k-candidate shape (spec §4.4).
                let query = SimilarityQuery {
                    node_types: Some(vec![NodeType::new(node_type.clone())]),
                    ..SimilarityQuery::new(emb, SimilarityMetric::Cosine, *k)
                };
                let hits = self.g().similar_nodes(query).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("results", hits.len() as u64)])))
            }
            WorkloadOp::UpdateEdgeWeight { edge, factor, bounded } => {
                let update = if *bounded {
                    WeightUpdate::multiply(*factor).with_bounds(0.0, 1.0)
                } else {
                    WeightUpdate::multiply(*factor)
                };
                self.g_mut().update_edge_weight(EdgeId(*edge), update).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("updated", 1)])))
            }
            WorkloadOp::DecayEdges { edge_type, factor, .. } => {
                let mut filter = EdgeFilter::new();
                if let Some(t) = edge_type {
                    filter = filter.with_edge_type(EdgeType::new(t.clone()));
                }
                let report = self.g_mut().decay_edges(filter, *factor).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("edges_decayed", report.edges_decayed as u64)])))
            }
            // In-memory graph: commit is not a measured persistence op here (M2
            // measures file-backed commit); report n/a per spec.
            WorkloadOp::Commit => Ok(OpOutcome::na()),
        }
    }
}

// ---- NaiveDriver ----

// NaiveDriver stores the full record for fidelity but only implements a subset
// of op classes (it exists to validate harness mechanics, not to be complete),
// so some fields are loaded but unread.
#[allow(dead_code)]
struct NaiveNode {
    node_type: String,
    props: BTreeMap<String, Json>,
    embedding: Option<Vec<f32>>,
}

#[allow(dead_code)]
struct NaiveEdge {
    from: u64,
    to: u64,
    edge_type: String,
    weight: f32,
}

/// Throwaway mechanics validation only (spec §5.1). Linear scans everywhere; its
/// timings are never a baseline.
#[derive(Default)]
pub struct NaiveDriver {
    nodes: HashMap<u64, NaiveNode>,
    edges: HashMap<u64, NaiveEdge>,
    adjacency: HashMap<u64, Vec<u64>>, // from -> edge ids
}

impl NaiveDriver {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GraphDriver for NaiveDriver {
    fn name(&self) -> String {
        "naive".into()
    }

    fn load_fixture(&mut self, fx: &Fixture) -> Result<LoadStats, String> {
        for n in &fx.nodes {
            self.nodes.insert(
                n.id,
                NaiveNode { node_type: n.node_type.clone(), props: n.props.clone().into_iter().collect(), embedding: None },
            );
        }
        for (id, v) in &fx.embeddings {
            if let Some(n) = self.nodes.get_mut(id) {
                n.embedding = Some(v.clone());
            }
        }
        for e in &fx.edges {
            self.edges.insert(e.id, NaiveEdge { from: e.from, to: e.to, edge_type: e.edge_type.clone(), weight: e.weight });
            self.adjacency.entry(e.from).or_default().push(e.id);
        }
        Ok(LoadStats { nodes: self.nodes.len(), edges: self.edges.len() })
    }

    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome, String> {
        match op {
            WorkloadOp::Neighbors { start, .. } => {
                let n = self.adjacency.get(start).map(|v| v.len()).unwrap_or(0);
                Ok(OpOutcome::ok(c(&[("neighbors", n as u64)])))
            }
            WorkloadOp::Traverse { start, max_hops } => {
                // Naive BFS frontier count.
                let mut frontier = vec![*start];
                let mut visited = 0u64;
                for _ in 0..*max_hops {
                    let mut next = Vec::new();
                    for node in frontier.drain(..) {
                        if let Some(edges) = self.adjacency.get(&node) {
                            for e in edges {
                                next.push(self.edges[e].to);
                                visited += 1;
                            }
                        }
                    }
                    frontier = next;
                }
                Ok(OpOutcome::ok(c(&[("visited", visited)])))
            }
            WorkloadOp::PropertyEq { key, ivalue, .. } => {
                let mut hits = 0u64;
                for n in self.nodes.values() {
                    if n.props.get(key).and_then(|v| v.as_i64()) == Some(*ivalue) {
                        hits += 1;
                    }
                }
                Ok(OpOutcome::ok(c(&[("hits", hits)])))
            }
            WorkloadOp::UpdateEdgeWeight { edge, factor, .. } => {
                if let Some(e) = self.edges.get_mut(edge) {
                    e.weight *= *factor;
                }
                Ok(OpOutcome::ok(c(&[("updated", 1)])))
            }
            // NaiveDriver only implements enough op classes to prove mechanics;
            // the rest report n/a rather than fabricate a measurement.
            _ => Ok(OpOutcome::na()),
        }
    }
}
