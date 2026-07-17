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
use drey::{EdgeId, EdgeType, Graph, NodeId};

use crate::generator::Fixture;
use crate::workload::{Dir, PropVal, WeightOp, WorkloadOp};

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
        OpOutcome {
            status: OpStatus::Ok,
            counters,
        }
    }
    fn na() -> Self {
        OpOutcome {
            status: OpStatus::NotApplicable,
            counters: BTreeMap::new(),
        }
    }
}

fn c(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// The measured surface.
pub trait GraphDriver {
    fn name(&self) -> String;
    fn load_fixture(&mut self, fx: &Fixture) -> Result<LoadStats, String>;
    /// Pre-timing setup for the immediately following `run_op` on the same
    /// op. The runner calls this *before* starting the timer, so a driver can
    /// resolve inputs that are workload-plan material rather than measured
    /// query work — e.g. materializing the `Similar` seed's embedding, which
    /// a real consumer would already hold (2026-07 repo review: doing it
    /// inside the timed region charged a full node materialization — property
    /// map + embedding clone — to every similarity sample). Default: nothing.
    fn prepare_op(&mut self, _op: &WorkloadOp) -> Result<(), String> {
        Ok(())
    }
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

/// A workload predicate value → a drey `Scalar`.
fn scalar(v: &PropVal) -> Scalar {
    match v {
        PropVal::I64(i) => Scalar::I64(*i),
        PropVal::F64(f) => Scalar::F64(*f),
        PropVal::Str(s) => Scalar::String(s.clone()),
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

/// Wraps a real in-memory `drey::Graph`.
///
/// Fixture ids are **not** assumed to equal drey ids: `load_fixture` records the
/// `NodeId`/`EdgeId` that `add_node`/`add_edge` actually return and every op
/// target is translated through those maps (checklist trap 6 — never infer
/// identity from insertion position). Today the fixture is dense-from-zero so
/// the maps are identity, but a captured fixture with id gaps would otherwise
/// silently attach edges to the wrong nodes.
pub struct DreyDriver {
    graph: Option<Graph>,
    node_map: HashMap<u64, NodeId>,
    edge_map: HashMap<u64, EdgeId>,
    /// Seed embedding resolved by `prepare_op` for the next `Similar` op,
    /// keyed by the fixture seed id it was resolved for.
    seed_cache: Option<(u64, Embedding)>,
}

impl DreyDriver {
    pub fn new() -> Self {
        DreyDriver {
            graph: None,
            node_map: HashMap::new(),
            edge_map: HashMap::new(),
            seed_cache: None,
        }
    }
    fn g(&self) -> &Graph {
        self.graph.as_ref().expect("fixture not loaded")
    }
    fn g_mut(&mut self) -> &mut Graph {
        self.graph.as_mut().expect("fixture not loaded")
    }
    fn node(&self, fixture_id: u64) -> Result<NodeId, String> {
        self.node_map
            .get(&fixture_id)
            .copied()
            .ok_or_else(|| format!("unknown fixture node id {fixture_id}"))
    }
    fn edge(&self, fixture_id: u64) -> Result<EdgeId, String> {
        self.edge_map
            .get(&fixture_id)
            .copied()
            .ok_or_else(|| format!("unknown fixture edge id {fixture_id}"))
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
        // Index p_seq (I64 ranges) and p_cat (String equality) on every node type
        // so property_range/property_eq hit the index rather than scanning.
        let mut config = GraphConfig::default();
        for t in 0..fx.params.node_types {
            let nt = NodeType::new(format!("nt_{t:02}"));
            config = config
                .with_indexed_property(nt.clone(), "p_seq")
                .with_indexed_property(nt, "p_cat");
        }
        let mut g = Graph::in_memory(config);
        for t in 0..fx.params.node_types {
            g.register_node_type(
                NodeType::new(format!("nt_{t:02}")),
                Some(fx.params.embed_dim as usize),
            )
            .map_err(|e| e.to_string())?;
        }

        // Record the id drey actually assigns — do not assume it equals n.id.
        let mut node_map = HashMap::with_capacity(fx.nodes.len());
        for n in &fx.nodes {
            let props: BTreeMap<String, Value> = n
                .props
                .iter()
                .map(|(k, v)| (k.clone(), to_value(v)))
                .collect();
            let id = g
                .add_node(NodeType::new(n.node_type.clone()), props)
                .map_err(|e| e.to_string())?;
            node_map.insert(n.id, id);
        }
        for (id, v) in &fx.embeddings {
            let nid = *node_map
                .get(id)
                .ok_or_else(|| format!("embedding for unknown fixture node {id}"))?;
            g.set_node_embedding(nid, Embedding::new(v.clone()))
                .map_err(|e| e.to_string())?;
        }
        let mut edge_map = HashMap::with_capacity(fx.edges.len());
        for e in &fx.edges {
            let props: BTreeMap<String, Value> = e
                .props
                .iter()
                .map(|(k, v)| (k.clone(), to_value(v)))
                .collect();
            let from = *node_map
                .get(&e.from)
                .ok_or_else(|| format!("edge {} from unknown node {}", e.id, e.from))?;
            let to = *node_map
                .get(&e.to)
                .ok_or_else(|| format!("edge {} to unknown node {}", e.id, e.to))?;
            let eid = g
                .add_edge(
                    from,
                    to,
                    EdgeType::new(e.edge_type.clone()),
                    e.weight,
                    props,
                )
                .map_err(|e| e.to_string())?;
            edge_map.insert(e.id, eid);
        }
        let stats = LoadStats {
            nodes: fx.nodes.len(),
            edges: fx.edges.len(),
        };
        self.graph = Some(g);
        self.node_map = node_map;
        self.edge_map = edge_map;
        Ok(stats)
    }

    fn prepare_op(&mut self, op: &WorkloadOp) -> Result<(), String> {
        self.seed_cache = None;
        if let WorkloadOp::Similar { seed_node, .. } = op {
            let seed = self.node(*seed_node)?;
            let emb = self
                .g()
                .node(seed)
                .map_err(|e| e.to_string())?
                .and_then(|n| n.embedding)
                .ok_or("seed node has no embedding")?;
            self.seed_cache = Some((*seed_node, emb));
        }
        Ok(())
    }

    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome, String> {
        match op {
            WorkloadOp::Neighbors {
                start,
                dir: d,
                edge_types,
                min_weight,
            } => {
                let start = self.node(*start)?;
                let opts = NeighborOptions {
                    direction: dir(*d),
                    edge_types: edge_types
                        .iter()
                        .map(|t| EdgeType::new(t.clone()))
                        .collect(),
                    min_weight: *min_weight,
                };
                let ns = self.g().neighbors(start, opts).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("neighbors", ns.len() as u64)])))
            }
            WorkloadOp::Traverse { start, max_hops } => {
                let start = self.node(*start)?;
                let paths = self
                    .g()
                    .traverse(
                        start,
                        TraversalOptions {
                            max_hops: Some(*max_hops),
                            max_paths: 1000,
                            ..Default::default()
                        },
                    )
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("paths_returned", paths.len() as u64)])))
            }
            WorkloadOp::ShortestPath {
                from,
                to,
                weighted,
                max_steps,
            } => {
                let from = self.node(*from)?;
                let to = self.node(*to)?;
                let opts = ShortestPathOptions {
                    cost_mode: if *weighted {
                        CostMode::WeightedCost
                    } else {
                        CostMode::UnweightedHops
                    },
                    max_steps: *max_steps,
                    ..Default::default()
                };
                let path = self
                    .g()
                    .shortest_path(from, to, opts)
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("found", path.is_some() as u64)])))
            }
            WorkloadOp::PropertyEq {
                node_type,
                key,
                value,
                ..
            } => {
                let hits = self
                    .g()
                    .nodes_by_property(PropertyQuery {
                        node_type: NodeType::new(node_type.clone()),
                        key: key.clone(),
                        predicate: ScalarPredicate::Eq(scalar(value)),
                    })
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("hits", hits.len() as u64)])))
            }
            WorkloadOp::PropertyRange {
                node_type,
                key,
                min,
                max,
                ..
            } => {
                let hits = self
                    .g()
                    .nodes_by_property(PropertyQuery {
                        node_type: NodeType::new(node_type.clone()),
                        key: key.clone(),
                        predicate: ScalarPredicate::Range {
                            min: Some(scalar(min)),
                            max: Some(scalar(max)),
                        },
                    })
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("hits", hits.len() as u64)])))
            }
            WorkloadOp::Similar {
                seed_node,
                k,
                node_types,
                prop_filter,
                candidates,
                ..
            } => {
                // The seed embedding is resolved by `prepare_op`, outside the
                // timed region — a consumer issuing this query already holds
                // its vector; materializing the whole seed node here charged
                // that overhead to every similarity sample. The inline
                // fallback keeps `run_op` correct if called without prepare.
                let emb = match self.seed_cache.take() {
                    Some((cached_for, emb)) if cached_for == *seed_node => emb,
                    _ => {
                        let seed = self.node(*seed_node)?;
                        self.g()
                            .node(seed)
                            .map_err(|e| e.to_string())?
                            .and_then(|n| n.embedding)
                            .ok_or("seed node has no embedding")?
                    }
                };
                // Compose the type filter (one or more types) and an optional
                // property filter so the scan is bounded to the sweep target
                // (spec §4.1). The realized candidate count is carried in the op
                // and reported here without re-scanning inside the timed region.
                let property_filter = prop_filter.as_ref().map(|(pk, pv)| PropertyQuery {
                    node_type: NodeType::new(node_types.first().cloned().unwrap_or_default()),
                    key: pk.clone(),
                    predicate: ScalarPredicate::Eq(scalar(pv)),
                });
                let query = SimilarityQuery {
                    node_types: Some(
                        node_types
                            .iter()
                            .map(|t| NodeType::new(t.clone()))
                            .collect(),
                    ),
                    property_filter,
                    // The plan's cand_target sweep defines the scan size being
                    // measured; the crate's scan ceiling (which now bounds
                    // candidates PROBED, not vectors scored) must not clip it.
                    // At stress scale the 10k sweep point composes ~104k
                    // probed candidates — over the default 100k ceiling — and
                    // without this the whole bench run aborts.
                    allow_full_scan: true,
                    ..SimilarityQuery::new(emb, SimilarityMetric::Cosine, *k)
                };
                let hits = self.g().similar_nodes(query).map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[
                    ("results", hits.len() as u64),
                    ("candidates", *candidates),
                ])))
            }
            WorkloadOp::UpdateEdgeWeight {
                edge,
                weight_op,
                operand,
                bounded,
            } => {
                let eid = self.edge(*edge)?;
                let update = match weight_op {
                    WeightOp::Set => WeightUpdate::set(*operand),
                    WeightOp::Add => WeightUpdate::add(*operand),
                    WeightOp::Multiply => WeightUpdate::multiply(*operand),
                };
                let update = if *bounded {
                    update.with_bounds(0.0, 1.0)
                } else {
                    update
                };
                self.g_mut()
                    .update_edge_weight(eid, update)
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[("updated", 1)])))
            }
            WorkloadOp::DecayEdges {
                edge_type, factor, ..
            } => {
                let mut filter = EdgeFilter::new();
                if let Some(t) = edge_type {
                    filter = filter.with_edge_type(EdgeType::new(t.clone()));
                }
                let report = self
                    .g_mut()
                    .decay_edges(filter, *factor)
                    .map_err(|e| e.to_string())?;
                Ok(OpOutcome::ok(c(&[(
                    "edges_decayed",
                    report.edges_decayed as u64,
                )])))
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

struct NaiveEdge {
    to: u64,
    edge_type: String,
    weight: f32,
}

/// Throwaway mechanics validation only (spec §5.1). Linear scans everywhere; its
/// timings are never a baseline. For the op classes it *does* implement it must
/// stay semantically equivalent to `DreyDriver` (same node-type filtering, same
/// bounded-weight semantics), so counter cross-checks are meaningful.
#[derive(Default)]
pub struct NaiveDriver {
    nodes: HashMap<u64, NaiveNode>,
    edges: HashMap<u64, NaiveEdge>,
    out_adj: HashMap<u64, Vec<u64>>, // from -> edge ids
    in_adj: HashMap<u64, Vec<u64>>,  // to -> edge ids
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
                NaiveNode {
                    node_type: n.node_type.clone(),
                    props: n.props.clone().into_iter().collect(),
                    embedding: None,
                },
            );
        }
        for (id, v) in &fx.embeddings {
            if let Some(n) = self.nodes.get_mut(id) {
                n.embedding = Some(v.clone());
            }
        }
        for e in &fx.edges {
            self.edges.insert(
                e.id,
                NaiveEdge {
                    to: e.to,
                    edge_type: e.edge_type.clone(),
                    weight: e.weight,
                },
            );
            self.out_adj.entry(e.from).or_default().push(e.id);
            self.in_adj.entry(e.to).or_default().push(e.id);
        }
        Ok(LoadStats {
            nodes: self.nodes.len(),
            edges: self.edges.len(),
        })
    }

    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome, String> {
        match op {
            WorkloadOp::Neighbors {
                start,
                dir: d,
                edge_types,
                min_weight,
            } => {
                // drey returns one Neighbor per matching edge (direction +
                // edge-type + min_weight filtered), so count matching edges the
                // same way — otherwise the cross-driver counter check diverges
                // now that the plan varies these parameters (audit #5).
                let mut candidates: Vec<u64> = Vec::new();
                if matches!(d, Dir::Outbound | Dir::Both) {
                    if let Some(es) = self.out_adj.get(start) {
                        candidates.extend(es);
                    }
                }
                if matches!(d, Dir::Inbound | Dir::Both) {
                    if let Some(es) = self.in_adj.get(start) {
                        candidates.extend(es);
                    }
                }
                let mut n = 0u64;
                for eid in candidates {
                    let e = &self.edges[&eid];
                    if !edge_types.is_empty() && !edge_types.contains(&e.edge_type) {
                        continue;
                    }
                    if let Some(w) = min_weight {
                        if e.weight < *w {
                            continue;
                        }
                    }
                    n += 1;
                }
                Ok(OpOutcome::ok(c(&[("neighbors", n)])))
            }
            WorkloadOp::Traverse { start, max_hops } => {
                // Bounded BFS with a visited set and an edge-hop cap. This is a
                // different computation from DreyDriver's DFS path enumeration
                // (matching that here would reintroduce the geometric frontier
                // blowup the cap exists to prevent — audit #5), so the counter is
                // named `edge_hops`, not `paths_returned`: the two traverse
                // counters measure different things and must not be cross-checked.
                use std::collections::HashSet;
                const MAX_EDGE_HOPS: u64 = 1000;
                let mut frontier = vec![*start];
                let mut visited_set: HashSet<u64> = HashSet::from([*start]);
                let mut edge_hops = 0u64;
                'outer: for _ in 0..*max_hops {
                    let mut next = Vec::new();
                    for node in frontier.drain(..) {
                        if let Some(edges) = self.out_adj.get(&node) {
                            for e in edges {
                                let to = self.edges[e].to;
                                edge_hops += 1;
                                if edge_hops >= MAX_EDGE_HOPS {
                                    break 'outer;
                                }
                                if visited_set.insert(to) {
                                    next.push(to);
                                }
                            }
                        }
                    }
                    frontier = next;
                }
                Ok(OpOutcome::ok(c(&[("edge_hops", edge_hops)])))
            }
            WorkloadOp::PropertyEq {
                node_type,
                key,
                value,
                ..
            } => {
                // Filter by node_type (as DreyDriver does) and match the typed
                // value; otherwise the hit counter would disagree with drey and
                // the cross-driver counter check would be meaningless (audit #5).
                let mut hits = 0u64;
                for n in self.nodes.values() {
                    if &n.node_type != node_type {
                        continue;
                    }
                    let m = match value {
                        PropVal::I64(i) => n.props.get(key).and_then(|v| v.as_i64()) == Some(*i),
                        PropVal::F64(f) => n.props.get(key).and_then(|v| v.as_f64()) == Some(*f),
                        PropVal::Str(s) => {
                            n.props.get(key).and_then(|v| v.as_str()) == Some(s.as_str())
                        }
                    };
                    if m {
                        hits += 1;
                    }
                }
                Ok(OpOutcome::ok(c(&[("hits", hits)])))
            }
            WorkloadOp::UpdateEdgeWeight {
                edge,
                weight_op,
                operand,
                bounded,
            } => {
                // Apply the op then clamp when bounded (the §4.1 stopgap
                // semantics DreyDriver uses), and report updated=0 for a missing
                // edge rather than a false 1 (audit #5).
                let updated = if let Some(e) = self.edges.get_mut(edge) {
                    let w = match weight_op {
                        WeightOp::Set => *operand,
                        WeightOp::Add => e.weight + *operand,
                        WeightOp::Multiply => e.weight * *operand,
                    };
                    e.weight = if *bounded { w.clamp(0.0, 1.0) } else { w };
                    1
                } else {
                    0
                };
                Ok(OpOutcome::ok(c(&[("updated", updated)])))
            }
            WorkloadOp::Similar {
                seed_node,
                k,
                node_types,
                prop_filter,
                candidates,
                ..
            } => {
                // Semantically mirrors DreyDriver's query composition: the
                // type allow-list, an optional Eq property filter (which drey
                // resolves against the FIRST listed type), same-dimension
                // candidates only, cosine, top-k. Linear scan by design.
                let emb = self
                    .nodes
                    .get(seed_node)
                    .ok_or("unknown seed node")?
                    .embedding
                    .clone()
                    .ok_or("seed node has no embedding")?;
                let cosine = |a: &[f32], b: &[f32]| -> f32 {
                    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if na == 0.0 || nb == 0.0 {
                        0.0
                    } else {
                        dot / (na * nb)
                    }
                };
                let mut scored: Vec<(u64, f32)> = Vec::new();
                for (id, n) in &self.nodes {
                    if !node_types.contains(&n.node_type) {
                        continue;
                    }
                    if let Some((pk, pv)) = prop_filter {
                        if Some(&n.node_type) != node_types.first() {
                            continue;
                        }
                        let matches = match (n.props.get(pk), pv) {
                            (Some(Json::String(s)), PropVal::Str(t)) => s == t,
                            (Some(Json::Number(num)), PropVal::I64(i)) => num.as_i64() == Some(*i),
                            (Some(Json::Number(num)), PropVal::F64(f)) => num.as_f64() == Some(*f),
                            _ => false,
                        };
                        if !matches {
                            continue;
                        }
                    }
                    let Some(e) = &n.embedding else { continue };
                    if e.len() != emb.len() {
                        continue;
                    }
                    scored.push((*id, cosine(&emb, e)));
                }
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                scored.truncate(*k);
                Ok(OpOutcome::ok(c(&[
                    ("results", scored.len() as u64),
                    ("candidates", *candidates),
                ])))
            }
            // NaiveDriver only implements enough op classes to prove mechanics;
            // the rest report n/a rather than fabricate a measurement.
            _ => Ok(OpOutcome::na()),
        }
    }
}
