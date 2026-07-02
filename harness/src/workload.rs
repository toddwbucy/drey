//! Deterministic workload plans (spec §4).
//!
//! Two plan shapes: [`mix_plan`] realizes one of the four named mixes as
//! op-class proportions over a fixed op count (spec §4.2), and
//! [`measurement_plan`] guarantees the §4.2.1 per-bucket sample floor for the
//! M3 budget gate by emitting a fixed number of instances of every budget-table
//! op class at representative parameters. Both are generated deterministically
//! from the fixture seed, so a run is reproducible; the plan is data, not code.

use serde::{Deserialize, Serialize};

use crate::generator::Fixture;
use crate::rng::{self, DetRng, Phase};
use rand::seq::SliceRandom;
use rand::Rng;

/// Decision-point exploration budget for `shortest_path` (M3 finding F1): a
/// consumer at a decision point bounds the search to keep worst-case latency
/// under control, accepting `None` when the target is farther than this many
/// node expansions. A representative decision-point value; it does not by itself
/// guarantee any particular p95 (a single mega-hub expansion can still be
/// expensive — see `specs/shortest-path-bound.md`).
pub const SHORTEST_PATH_MAX_STEPS: usize = 512;

/// Direction for neighbor/traversal ops.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Outbound,
    Inbound,
    Both,
}

/// One workload operation. `bucket()` names its (op class × parameter class) for
/// measurement grouping.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkloadOp {
    Neighbors { start: u64, dir: Dir, edge_type: Option<String>, min_weight: Option<f32> },
    Traverse { start: u64, max_hops: usize },
    ShortestPath { from: u64, to: u64, weighted: bool, max_steps: Option<usize> },
    PropertyEq { node_type: String, key: String, ivalue: i64 },
    PropertyRange { node_type: String, key: String, min: i64, max: i64 },
    Similar { seed_node: u64, k: usize, node_type: String },
    UpdateEdgeWeight { edge: u64, factor: f32, bounded: bool },
    DecayEdges { edge_type: Option<String>, factor: f32, batch: usize },
    Commit,
}

impl WorkloadOp {
    /// The measurement bucket key: op class plus the discriminating parameter.
    pub fn bucket(&self) -> String {
        match self {
            WorkloadOp::Neighbors { .. } => "neighbors".into(),
            WorkloadOp::Traverse { max_hops, .. } => format!("traverse:max_hops={max_hops}"),
            WorkloadOp::ShortestPath { weighted, .. } => {
                format!("shortest_path:{}", if *weighted { "weighted" } else { "hops" })
            }
            WorkloadOp::PropertyEq { .. } => "property_eq".into(),
            WorkloadOp::PropertyRange { .. } => "property_range".into(),
            WorkloadOp::Similar { .. } => "similar_nodes".into(),
            WorkloadOp::UpdateEdgeWeight { .. } => "update_edge_weight".into(),
            WorkloadOp::DecayEdges { batch, .. } => format!("decay_edges:batch={batch}"),
            WorkloadOp::Commit => "commit".into(),
        }
    }

    /// Whether this op mutates (used for commit cadence and mix classing).
    fn is_mutation(&self) -> bool {
        matches!(
            self,
            WorkloadOp::UpdateEdgeWeight { .. } | WorkloadOp::DecayEdges { .. }
        )
    }
}

/// The four named mixes (spec §4.2).
#[derive(Clone, Copy, Debug)]
pub enum Mix {
    TraversalHeavy,
    SimilarityHeavy,
    UpdateHeavy,
    Mixed,
}

impl Mix {
    /// `(reads, similarity, mutations)` percentages.
    fn shares(self) -> (u32, u32, u32) {
        match self {
            Mix::TraversalHeavy => (80, 10, 10),
            Mix::SimilarityHeavy => (30, 60, 10),
            Mix::UpdateHeavy => (20, 5, 75),
            Mix::Mixed => (50, 25, 25),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mix::TraversalHeavy => "traversal_heavy",
            Mix::SimilarityHeavy => "similarity_heavy",
            Mix::UpdateHeavy => "update_heavy",
            Mix::Mixed => "mixed",
        }
    }
}

/// Precomputed structural facts about a fixture, for drawing valid op targets.
struct FixtureIndex {
    node_ids: Vec<u64>,
    edge_ids: Vec<u64>,
    /// Node ids sorted by out-degree, for degree-stratified starts.
    by_out_degree: Vec<u64>,
    edge_types: Vec<String>,
}

impl FixtureIndex {
    fn build(fx: &Fixture) -> Self {
        let node_ids: Vec<u64> = fx.nodes.iter().map(|n| n.id).collect();
        let edge_ids: Vec<u64> = fx.edges.iter().map(|e| e.id).collect();
        let mut out_deg: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let mut edge_types = std::collections::BTreeSet::new();
        for e in &fx.edges {
            *out_deg.entry(e.from).or_default() += 1;
            edge_types.insert(e.edge_type.clone());
        }
        let mut by_out_degree = node_ids.clone();
        by_out_degree.sort_by_key(|n| *out_deg.get(n).unwrap_or(&0));
        FixtureIndex {
            node_ids,
            edge_ids,
            by_out_degree,
            edge_types: edge_types.into_iter().collect(),
        }
    }

    /// A degree-stratified start node: `stratum` 0=low, 1=median, 2=hub.
    fn stratified_start(&self, stratum: usize) -> u64 {
        let n = self.by_out_degree.len();
        if n == 0 {
            return 0;
        }
        let idx = match stratum {
            0 => n / 10,          // low-degree
            1 => n / 2,           // median
            _ => n - 1,           // hub
        };
        self.by_out_degree[idx.min(n - 1)]
    }
}

/// A plan that meets the §4.2.1 per-bucket sample floor for the M3 gate: it
/// emits `per_bucket` instances of every budget-table op class, at
/// representative parameters, drawing valid targets from the fixture. Batch
/// decay ops appear at all three batch sizes.
pub fn measurement_plan(fx: &Fixture, per_bucket: usize) -> Vec<WorkloadOp> {
    let idx = FixtureIndex::build(fx);
    let mut rng = rng::phase(fx.params.seed, Phase::Workload);
    let mut plan = Vec::new();

    for i in 0..per_bucket {
        let stratum = i % 3;
        let start = idx.stratified_start(stratum);
        // neighbors
        plan.push(WorkloadOp::Neighbors {
            start,
            dir: Dir::Outbound,
            edge_type: None,
            min_weight: None,
        });
        // traverse at both hop classes
        plan.push(WorkloadOp::Traverse { start, max_hops: 2 });
        plan.push(WorkloadOp::Traverse { start, max_hops: 5 });
        // shortest path, both modes, random pairs
        let from = *idx.node_ids.choose(&mut rng).unwrap();
        let to = *idx.node_ids.choose(&mut rng).unwrap();
        plan.push(WorkloadOp::ShortestPath { from, to, weighted: false, max_steps: Some(SHORTEST_PATH_MAX_STEPS) });
        plan.push(WorkloadOp::ShortestPath { from, to, weighted: true, max_steps: Some(SHORTEST_PATH_MAX_STEPS) });
        // property eq / range across the selectivity bands
        plan.push(WorkloadOp::PropertyEq {
            node_type: "nt_00".into(),
            key: "p_seq".into(),
            ivalue: (i as i64) % (fx.params.nodes as i64),
        });
        plan.push(WorkloadOp::PropertyRange {
            node_type: "nt_00".into(),
            key: "p_seq".into(),
            min: 0,
            max: (fx.params.nodes as i64) / 10,
        });
        // similarity, seeded from a covered node
        if let Some((seed_node, _)) = fx.embeddings.choose(&mut rng) {
            // Compose a node-type filter so the candidate set is bounded, per
            // the "filtered ≤10k candidates" budget (spec §4.4) rather than an
            // unfiltered full-graph vector scan.
            plan.push(WorkloadOp::Similar {
                seed_node: *seed_node,
                k: 10,
                node_type: "nt_00".into(),
            });
        }
        // update edge weight, half bounded
        if let Some(edge) = idx.edge_ids.choose(&mut rng) {
            plan.push(WorkloadOp::UpdateEdgeWeight {
                edge: *edge,
                factor: 0.99,
                bounded: i % 2 == 0,
            });
        }
    }

    // Batch decay ops at all three sizes (fewer instances — heavyweight).
    for &batch in &[1_000usize, 10_000, 100_000] {
        for _ in 0..per_bucket.min(50) {
            let edge_type = idx.edge_types.choose(&mut rng).cloned();
            plan.push(WorkloadOp::DecayEdges { edge_type, factor: 0.9, batch });
        }
    }

    plan
}

/// A named-mix plan: op classes in the mix's proportions over `ops_total`
/// operations, with a `Commit` inserted after every 1,000 mutations as plan
/// structure (spec §4.1, §4.2 — commits are not counted in the percentages).
pub fn mix_plan(fx: &Fixture, mix: Mix, ops_total: usize) -> Vec<WorkloadOp> {
    let idx = FixtureIndex::build(fx);
    let mut rng = rng::phase(fx.params.seed ^ mix_salt(mix), Phase::Workload);
    let (reads, sim, _muts) = mix.shares();
    let mut plan = Vec::with_capacity(ops_total);
    let mut mutations_since_commit = 0;

    for i in 0..ops_total {
        let roll = rng.gen_range(0..100);
        let op = if roll < reads {
            read_op(&idx, &mut rng, i)
        } else if roll < reads + sim {
            similarity_op(fx, &mut rng)
        } else {
            mutation_op(&idx, &mut rng)
        };
        if op.is_mutation() {
            mutations_since_commit += 1;
        }
        plan.push(op);
        if mutations_since_commit >= 1_000 {
            plan.push(WorkloadOp::Commit);
            mutations_since_commit = 0;
        }
    }
    plan
}

fn mix_salt(mix: Mix) -> u64 {
    match mix {
        Mix::TraversalHeavy => 0x11,
        Mix::SimilarityHeavy => 0x22,
        Mix::UpdateHeavy => 0x33,
        Mix::Mixed => 0x44,
    }
}

fn read_op(idx: &FixtureIndex, rng: &mut DetRng, i: usize) -> WorkloadOp {
    let start = idx.stratified_start(i % 3);
    match rng.gen_range(0..4) {
        0 => WorkloadOp::Neighbors { start, dir: Dir::Outbound, edge_type: None, min_weight: None },
        1 => WorkloadOp::Traverse { start, max_hops: 2 },
        2 => WorkloadOp::PropertyEq {
            node_type: "nt_00".into(),
            key: "p_seq".into(),
            ivalue: rng.gen_range(0..idx.node_ids.len() as i64),
        },
        _ => {
            let from = *idx.node_ids.choose(rng).unwrap();
            let to = *idx.node_ids.choose(rng).unwrap();
            WorkloadOp::ShortestPath { from, to, weighted: false, max_steps: Some(SHORTEST_PATH_MAX_STEPS) }
        }
    }
}

fn similarity_op(fx: &Fixture, rng: &mut DetRng) -> WorkloadOp {
    let seed_node = fx.embeddings.choose(rng).map(|(id, _)| *id).unwrap_or(0);
    WorkloadOp::Similar { seed_node, k: 10, node_type: "nt_00".into() }
}

fn mutation_op(idx: &FixtureIndex, rng: &mut DetRng) -> WorkloadOp {
    if rng.gen_bool(0.9) {
        let edge = idx.edge_ids.choose(rng).copied().unwrap_or(0);
        WorkloadOp::UpdateEdgeWeight { edge, factor: 0.99, bounded: rng.gen_bool(0.5) }
    } else {
        WorkloadOp::DecayEdges {
            edge_type: idx.edge_types.choose(rng).cloned(),
            factor: 0.9,
            batch: 1_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate;
    use crate::params::{Fanout, Parameters, SizeClass};

    #[test]
    fn measurement_plan_covers_every_budget_bucket() {
        let fx = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 1));
        let plan = measurement_plan(&fx, 100);
        let buckets: std::collections::BTreeSet<String> =
            plan.iter().map(|o| o.bucket()).collect();
        for expected in [
            "neighbors",
            "traverse:max_hops=2",
            "traverse:max_hops=5",
            "shortest_path:hops",
            "shortest_path:weighted",
            "property_eq",
            "property_range",
            "similar_nodes",
            "update_edge_weight",
            "decay_edges:batch=1000",
            "decay_edges:batch=100000",
        ] {
            assert!(buckets.contains(expected), "missing bucket {expected}");
        }
    }

    #[test]
    fn mix_plan_is_deterministic_and_inserts_commits() {
        let fx = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 1));
        let a = mix_plan(&fx, Mix::Mixed, 5_000);
        let b = mix_plan(&fx, Mix::Mixed, 5_000);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().any(|o| matches!(o, WorkloadOp::Commit)));
    }
}
