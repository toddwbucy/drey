//! Deterministic workload plans (spec §4).
//!
//! The workload is **data, not code** (spec §3.6, checklist trap 16): a plan is
//! a `Vec<WorkloadOp>` generated deterministically from the fixture seed and
//! materialized to `workload.*.jsonl`; the runner replays exactly that sequence.
//!
//! Both plan shapes are built from the same per-bucket *makers* so they stay
//! consistent: [`measurement_plan`] emits a fixed number of instances of every
//! budget-table bucket (the M3 gate), and [`mix_plan`] realizes one of the four
//! named mixes (spec §4.2) by allocating each op-class share across its
//! parameter-class buckets so no exercised bucket falls below the §5.2 floor
//! (spec §4.2.1). [`plan_self_check`] enforces that floor before a plan is
//! written (spec §3.7).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::generator::Fixture;
use crate::params::SizeClass;
use crate::rng::{self, DetRng, Phase};
use rand::seq::SliceRandom;
use rand::Rng;

/// Decision-point exploration budget for `shortest_path` (M3 finding F1): a
/// consumer at a decision point bounds the search to keep worst-case latency
/// under control, accepting `None` when the target is farther than this many
/// node expansions. A representative decision-point value; it does not by itself
/// guarantee any particular p95 (a single mega-hub expansion can still be
/// expensive — see `docs/specs/shortest-path-bound.md`).
pub const SHORTEST_PATH_MAX_STEPS: usize = 512;

/// Warmup samples discarded per bucket before retention (spec §5.2).
pub const WARMUP: usize = 100;

/// The retained-sample floor per (op class × parameter class) bucket (spec §5.2):
/// ≥1,000 at `small`/`representative`, ≥100 at `stress`.
pub fn sample_floor(size: SizeClass) -> usize {
    match size {
        SizeClass::Stress => 100,
        _ => 1_000,
    }
}

/// Direction for neighbor/traversal ops.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Outbound,
    Inbound,
    Both,
}

/// A typed scalar for a property predicate, so the plan can exercise equality on
/// the categorical `p_cat` (String) selectivity bands and ranges on the numeric
/// `p_seq`/`p_score` — not just a single integer shape (spec §4.1, trap 22).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PropVal {
    I64(i64),
    F64(f64),
    Str(String),
}

/// The weight-update op variant (spec §4.1, trap 24): the plan must exercise all
/// three, not only `Multiply`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WeightOp {
    Set,
    Add,
    Multiply,
}

/// One workload operation. `bucket()` names its (op class × parameter class) for
/// measurement grouping; `params()` reports the bucket-defining parameters for
/// the result row.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkloadOp {
    Neighbors {
        start: u64,
        dir: Dir,
        /// 0–2 edge-type filters (trap 19: not just unfiltered).
        edge_types: Vec<String>,
        min_weight: Option<f32>,
    },
    Traverse {
        start: u64,
        max_hops: usize,
    },
    ShortestPath {
        from: u64,
        to: u64,
        weighted: bool,
        max_steps: Option<usize>,
    },
    PropertyEq {
        node_type: String,
        key: String,
        value: PropVal,
        /// Selectivity class label (`rare`/`uncommon`/`common`), the parameter
        /// class for this bucket.
        sel: String,
    },
    PropertyRange {
        node_type: String,
        key: String,
        min: PropVal,
        max: PropVal,
        sel: String,
    },
    Similar {
        seed_node: u64,
        k: usize,
        /// Node-type filter (one or more types) bounding the candidate set.
        node_types: Vec<String>,
        /// Optional `(key, value)` property filter composed with the type filter.
        prop_filter: Option<(String, PropVal)>,
        /// The candidate-sweep target (100/1k/10k) — the parameter class.
        cand_target: u64,
        /// Realized candidate count for this filter, precomputed from the fixture
        /// so it can be reported in counters without polluting the timed op
        /// (spec §4.1: "actual candidate count recorded in counters").
        candidates: u64,
    },
    UpdateEdgeWeight {
        edge: u64,
        weight_op: WeightOp,
        operand: f32,
        bounded: bool,
    },
    DecayEdges {
        edge_type: Option<String>,
        factor: f32,
        batch: usize,
    },
    Commit,
}

impl WorkloadOp {
    /// The measurement bucket key: op class plus the discriminating parameter
    /// class. Buckets are never aggregated across parameter classes (spec §5.2).
    pub fn bucket(&self) -> String {
        match self {
            WorkloadOp::Neighbors { .. } => "neighbors".into(),
            WorkloadOp::Traverse { max_hops, .. } => format!("traverse:max_hops={max_hops}"),
            WorkloadOp::ShortestPath { weighted, .. } => {
                format!(
                    "shortest_path:{}",
                    if *weighted { "weighted" } else { "hops" }
                )
            }
            WorkloadOp::PropertyEq { sel, .. } => format!("property_eq:sel={sel}"),
            WorkloadOp::PropertyRange { sel, .. } => format!("property_range:sel={sel}"),
            WorkloadOp::Similar { cand_target, .. } => format!("similar_nodes:cand={cand_target}"),
            WorkloadOp::UpdateEdgeWeight { .. } => "update_edge_weight".into(),
            WorkloadOp::DecayEdges { batch, .. } => format!("decay_edges:batch={batch}"),
            WorkloadOp::Commit => "commit".into(),
        }
    }

    /// Bucket-defining parameters for the result row (constant within a bucket).
    /// Instance-varying knobs (neighbors direction/filters, weight-op variant)
    /// are deliberately omitted: they vary within the bucket by design.
    pub fn params(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        match self {
            WorkloadOp::Traverse { max_hops, .. } => {
                m.insert("max_hops".into(), max_hops.to_string());
            }
            WorkloadOp::ShortestPath { weighted, .. } => {
                m.insert(
                    "cost_mode".into(),
                    if *weighted { "weighted" } else { "unweighted" }.into(),
                );
            }
            WorkloadOp::PropertyEq { sel, key, .. }
            | WorkloadOp::PropertyRange { sel, key, .. } => {
                m.insert("sel".into(), sel.clone());
                m.insert("key".into(), key.clone());
            }
            WorkloadOp::Similar { cand_target, .. } => {
                m.insert("cand_target".into(), cand_target.to_string());
            }
            WorkloadOp::DecayEdges { batch, .. } => {
                m.insert("batch".into(), batch.to_string());
            }
            _ => {}
        }
        m
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

    /// All four mixes, for materializing every plan (spec §7 exit item).
    pub fn all() -> [Mix; 4] {
        [
            Mix::TraversalHeavy,
            Mix::SimilarityHeavy,
            Mix::UpdateHeavy,
            Mix::Mixed,
        ]
    }
}

/// Precomputed structural facts about a fixture, for drawing valid op targets.
struct FixtureIndex {
    node_ids: Vec<u64>,
    edge_ids: Vec<u64>,
    /// Node ids sorted ascending by out-degree, for degree-stratified starts.
    by_out_degree: Vec<u64>,
    edge_types: Vec<String>,
    /// Embedded node ids per node type (similarity seeds must have a vector).
    emb_ids_by_type: HashMap<String, Vec<u64>>,
    /// Embedded-node count per node type (similarity candidate accounting).
    emb_count_by_type: HashMap<String, u64>,
    /// Embedded-node count per (node type, p_cat band).
    emb_count_by_type_cat: HashMap<(String, String), u64>,
    nodes: u64,
    node_type_count: u32,
}

impl FixtureIndex {
    fn build(fx: &Fixture) -> Self {
        let node_ids: Vec<u64> = fx.nodes.iter().map(|n| n.id).collect();
        let edge_ids: Vec<u64> = fx.edges.iter().map(|e| e.id).collect();
        let mut out_deg: HashMap<u64, u32> = HashMap::new();
        let mut edge_types = std::collections::BTreeSet::new();
        for e in &fx.edges {
            *out_deg.entry(e.from).or_default() += 1;
            edge_types.insert(e.edge_type.clone());
        }
        let mut by_out_degree = node_ids.clone();
        by_out_degree.sort_by_key(|n| *out_deg.get(n).unwrap_or(&0));

        let embedded: HashSet<u64> = fx.embeddings.iter().map(|(id, _)| *id).collect();
        let mut emb_ids_by_type: HashMap<String, Vec<u64>> = HashMap::new();
        let mut emb_count_by_type: HashMap<String, u64> = HashMap::new();
        let mut emb_count_by_type_cat: HashMap<(String, String), u64> = HashMap::new();
        for n in &fx.nodes {
            if embedded.contains(&n.id) {
                emb_ids_by_type
                    .entry(n.node_type.clone())
                    .or_default()
                    .push(n.id);
                *emb_count_by_type.entry(n.node_type.clone()).or_default() += 1;
                if let Some(cat) = n.props.get("p_cat").and_then(|v| v.as_str()) {
                    *emb_count_by_type_cat
                        .entry((n.node_type.clone(), cat.to_string()))
                        .or_default() += 1;
                }
            }
        }

        FixtureIndex {
            node_ids,
            edge_ids,
            by_out_degree,
            edge_types: edge_types.into_iter().collect(),
            emb_ids_by_type,
            emb_count_by_type,
            emb_count_by_type_cat,
            nodes: fx.params.nodes,
            node_type_count: fx.params.node_types,
        }
    }

    /// A degree-stratified start node drawn *randomly within* its band, so a
    /// bucket samples many nodes rather than re-running one fixed id (trap 26).
    /// `stratum` 0=low third, 1=median third, 2=hub decile.
    fn stratified_draw(&self, stratum: usize, rng: &mut DetRng) -> u64 {
        let n = self.by_out_degree.len();
        if n == 0 {
            return 0;
        }
        let (lo, hi) = match stratum {
            0 => (0, n / 3),
            1 => (n / 3, 2 * n / 3),
            _ => (9 * n / 10, n), // hub decile
        };
        let hi = hi.max(lo + 1).min(n);
        let idx = lo + rng.gen_range(0..(hi - lo));
        self.by_out_degree[idx.min(n - 1)]
    }

    fn any_node(&self, rng: &mut DetRng) -> u64 {
        self.node_ids.choose(rng).copied().unwrap_or(0)
    }

    /// An embedded seed node of `nt`, falling back to any embedded node.
    fn embedded_seed(&self, nt: &str, rng: &mut DetRng) -> Option<u64> {
        if let Some(ids) = self.emb_ids_by_type.get(nt) {
            if let Some(id) = ids.choose(rng) {
                return Some(*id);
            }
        }
        self.emb_ids_by_type
            .values()
            .flat_map(|v| v.iter())
            .next()
            .copied()
    }
}

// ---- per-bucket op makers ----
//
// A maker builds one instance of a single measurement bucket. Both plan shapes
// draw from the same makers, so a bucket is defined once. Makers draw fresh
// targets/parameters each call, which is what widens per-bucket sampling.

type Maker = Box<dyn Fn(&FixtureIndex, &mut DetRng) -> WorkloadOp>;

fn neighbors_op(idx: &FixtureIndex, rng: &mut DetRng) -> WorkloadOp {
    let start = idx.stratified_draw(rng.gen_range(0..3), rng);
    let dir = match rng.gen_range(0..3) {
        0 => Dir::Outbound,
        1 => Dir::Inbound,
        _ => Dir::Both,
    };
    // 0, 1, or 2 edge-type filters.
    let nfilters = rng.gen_range(0..3usize).min(idx.edge_types.len());
    let mut edge_types: Vec<String> = idx
        .edge_types
        .choose_multiple(rng, nfilters)
        .cloned()
        .collect();
    edge_types.sort(); // deterministic ordering within the op
    let min_weight = if rng.gen_bool(0.5) {
        Some(rng.gen::<f32>())
    } else {
        None
    };
    WorkloadOp::Neighbors {
        start,
        dir,
        edge_types,
        min_weight,
    }
}

fn shortest_path_op(idx: &FixtureIndex, rng: &mut DetRng, weighted: bool) -> WorkloadOp {
    // A mix of connected and disconnected pairs falls out of drawing both
    // endpoints independently across strata (spec §4.1, trap 21).
    let from = idx.stratified_draw(rng.gen_range(0..3), rng);
    let to = idx.any_node(rng);
    WorkloadOp::ShortestPath {
        from,
        to,
        weighted,
        max_steps: Some(SHORTEST_PATH_MAX_STEPS),
    }
}

/// (selectivity label, `p_cat` band value) for the three equality classes.
const CAT_BANDS: [(&str, &str); 3] = [
    ("rare", "cat_rare"),
    ("uncommon", "cat_uncommon"),
    ("common", "cat_common"),
];

/// (selectivity label, fraction of a type's population) for range classes.
const RANGE_BANDS: [(&str, f64); 3] = [("rare", 0.001), ("uncommon", 0.01), ("common", 0.10)];

fn property_eq_op(idx: &FixtureIndex, sel: &str, cat: &str) -> WorkloadOp {
    let _ = idx;
    WorkloadOp::PropertyEq {
        node_type: "nt_00".into(),
        key: "p_cat".into(),
        value: PropVal::Str(cat.into()),
        sel: sel.into(),
    }
}

fn property_range_op(idx: &FixtureIndex, sel: &str, frac: f64) -> WorkloadOp {
    // Range over the unique, monotonic `p_seq`. Because ids are round-robin over
    // node types, an id range `[0, nodes*frac)` selects ~`frac` of each type's
    // population — a deterministic selectivity sweep that hits the p_seq index.
    let max = (idx.nodes as f64 * frac) as i64;
    WorkloadOp::PropertyRange {
        node_type: "nt_00".into(),
        key: "p_seq".into(),
        min: PropVal::I64(0),
        max: PropVal::I64(max),
        sel: sel.into(),
    }
}

/// Build a similarity op targeting ~`target` candidates by composing filters,
/// recording the realized candidate count from the fixture.
fn similar_op(idx: &FixtureIndex, rng: &mut DetRng, target: u64) -> WorkloadOp {
    let seed = idx.embedded_seed("nt_00", rng).unwrap_or(0);
    let (node_types, prop_filter, candidates) = match target {
        100 => {
            // One type + the ~10% p_cat band → the smallest sweep point.
            let c = idx
                .emb_count_by_type_cat
                .get(&("nt_00".to_string(), "cat_common".to_string()))
                .copied()
                .unwrap_or(0);
            (
                vec!["nt_00".to_string()],
                Some(("p_cat".to_string(), PropVal::Str("cat_common".into()))),
                c,
            )
        }
        1_000 => {
            // One whole type.
            let c = idx.emb_count_by_type.get("nt_00").copied().unwrap_or(0);
            (vec!["nt_00".to_string()], None, c)
        }
        _ => {
            // ~10k: the first five types (or as many as exist).
            let ntypes = (idx.node_type_count as usize).clamp(1, 5);
            let types: Vec<String> = (0..ntypes).map(|t| format!("nt_{t:02}")).collect();
            let c: u64 = types
                .iter()
                .map(|t| idx.emb_count_by_type.get(t).copied().unwrap_or(0))
                .sum();
            (types, None, c)
        }
    };
    WorkloadOp::Similar {
        seed_node: seed,
        k: 10,
        node_types,
        prop_filter,
        cand_target: target,
        candidates,
    }
}

fn update_edge_weight_op(idx: &FixtureIndex, rng: &mut DetRng) -> WorkloadOp {
    let edge = idx.edge_ids.choose(rng).copied().unwrap_or(0);
    let (op, operand) = match rng.gen_range(0..3) {
        0 => (WeightOp::Set, 0.5),
        1 => (WeightOp::Add, 0.1),
        _ => (WeightOp::Multiply, 0.99),
    };
    WorkloadOp::UpdateEdgeWeight {
        edge,
        weight_op: op,
        operand,
        bounded: rng.gen_bool(0.5),
    }
}

/// A decay maker that pairs every decay (factor 0.9) with a restore of the
/// SAME edge type at the reciprocal factor on its next call. Without the
/// pairing, a measurement plan's thousands of decay instances compound —
/// weights collapse toward zero, so every later min_weight-filtered read and
/// weighted shortest path measures a degenerate graph instead of the fixture
/// (2026-07 repo review). The factor is not part of the bucket key, so both
/// halves land in the same `decay_edges:batch=` bucket and exercise the same
/// batch-sized write; the per-type net exposure never exceeds one 0.9 step.
fn decay_maker(batch: usize) -> Maker {
    let pending: std::cell::RefCell<Option<Option<String>>> = std::cell::RefCell::new(None);
    Box::new(move |idx, rng| {
        let mut pending = pending.borrow_mut();
        let (edge_type, factor) = match pending.take() {
            Some(t) => (t, 1.0 / 0.9_f32),
            None => {
                let t = idx.edge_types.choose(rng).cloned();
                *pending = Some(t.clone());
                (t, 0.9)
            }
        };
        WorkloadOp::DecayEdges {
            edge_type,
            factor,
            batch,
        }
    })
}

/// The read-class makers (one per budget-table read bucket).
fn read_makers() -> Vec<Maker> {
    let mut v: Vec<Maker> = vec![
        Box::new(neighbors_op),
        Box::new(|idx, rng| WorkloadOp::Traverse {
            start: idx.stratified_draw(rng.gen_range(0..3), rng),
            max_hops: 2,
        }),
        Box::new(|idx, rng| WorkloadOp::Traverse {
            start: idx.stratified_draw(rng.gen_range(0..3), rng),
            max_hops: 5,
        }),
        Box::new(|idx, rng| shortest_path_op(idx, rng, false)),
        Box::new(|idx, rng| shortest_path_op(idx, rng, true)),
    ];
    for (sel, cat) in CAT_BANDS {
        v.push(Box::new(move |idx, _rng| property_eq_op(idx, sel, cat)));
    }
    for (sel, frac) in RANGE_BANDS {
        v.push(Box::new(move |idx, _rng| property_range_op(idx, sel, frac)));
    }
    v
}

/// The similarity-class makers (candidate sweep 100/1k/10k).
fn sim_makers() -> Vec<Maker> {
    vec![
        Box::new(|idx, rng| similar_op(idx, rng, 100)),
        Box::new(|idx, rng| similar_op(idx, rng, 1_000)),
        Box::new(|idx, rng| similar_op(idx, rng, 10_000)),
    ]
}

/// The mutation-class makers (update + decay at three batch sizes).
fn mut_makers() -> Vec<Maker> {
    vec![
        Box::new(update_edge_weight_op),
        decay_maker(1_000),
        decay_maker(10_000),
        decay_maker(100_000),
    ]
}

/// A plan that emits `samples_per_bucket` instances of every budget-table
/// bucket at representative parameters (spec §4.4). Sized so that, after the
/// [`WARMUP`] discard, each bucket retains the [`sample_floor`]; the exact
/// sizing is the caller's (see [`measurement_samples`]). Batch decay buckets
/// appear at all three sizes.
pub fn measurement_plan(fx: &Fixture, samples_per_bucket: usize) -> Vec<WorkloadOp> {
    let idx = FixtureIndex::build(fx);
    let mut rng = rng::phase(fx.params.seed, Phase::Workload);
    let makers: Vec<Maker> = read_makers()
        .into_iter()
        .chain(sim_makers())
        .chain(mut_makers())
        .collect();

    let mut plan = Vec::with_capacity(samples_per_bucket * makers.len());
    for _ in 0..samples_per_bucket {
        for make in &makers {
            plan.push(make(&idx, &mut rng));
        }
    }
    plan
}

/// The instances-per-bucket a measurement plan needs to retain the floor after
/// warmup, at a given size class.
pub fn measurement_samples(size: SizeClass) -> usize {
    sample_floor(size) + WARMUP
}

/// A named-mix plan (spec §4.2): op classes in the mix's proportions over
/// `ops_total` operations. Each op-class share is allocated across its
/// parameter-class buckets by round-robin so no exercised bucket is starved
/// (spec §4.2.1), then the three class streams are interleaved deterministically
/// and a `Commit` is inserted after every 1,000 mutations as plan structure
/// (commits are outside the mix percentages).
pub fn mix_plan(fx: &Fixture, mix: Mix, ops_total: usize) -> Vec<WorkloadOp> {
    let idx = FixtureIndex::build(fx);
    let mut rng = rng::phase(fx.params.seed ^ mix_salt(mix), Phase::Workload);
    let (reads, sim, _muts) = mix.shares();

    let n_reads = ops_total * reads as usize / 100;
    let n_sim = ops_total * sim as usize / 100;
    let n_muts = ops_total.saturating_sub(n_reads + n_sim); // remainder → mutations

    // Build each class stream by cycling its makers, so param classes are evenly
    // covered rather than sampled i.i.d. (which would starve tail buckets).
    let mut reads_q = build_stream(&idx, &mut rng, &read_makers(), n_reads);
    let mut sim_q = build_stream(&idx, &mut rng, &sim_makers(), n_sim);
    let mut muts_q = build_stream(&idx, &mut rng, &mut_makers(), n_muts);

    // Interleave by weighted draw from whichever streams remain.
    let mut plan = Vec::with_capacity(ops_total + ops_total / 1_000 + 4);
    let mut mutations_since_commit = 0usize;
    let mut ri = 0;
    let mut si = 0;
    let mut mi = 0;
    while ri < reads_q.len() || si < sim_q.len() || mi < muts_q.len() {
        let rem_r = (reads_q.len() - ri) as u32;
        let rem_s = (sim_q.len() - si) as u32;
        let rem_m = (muts_q.len() - mi) as u32;
        let total = rem_r + rem_s + rem_m;
        let roll = rng.gen_range(0..total);
        let op = if roll < rem_r {
            ri += 1;
            std::mem::replace(&mut reads_q[ri - 1], WorkloadOp::Commit)
        } else if roll < rem_r + rem_s {
            si += 1;
            std::mem::replace(&mut sim_q[si - 1], WorkloadOp::Commit)
        } else {
            mi += 1;
            std::mem::replace(&mut muts_q[mi - 1], WorkloadOp::Commit)
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

/// Build `n` ops by cycling through `makers` (bucket k gets every k-th slot), so
/// each parameter-class bucket receives ⌈n/|makers|⌉ or ⌊n/|makers|⌋ instances.
fn build_stream(
    idx: &FixtureIndex,
    rng: &mut DetRng,
    makers: &[Maker],
    n: usize,
) -> Vec<WorkloadOp> {
    let mut out = Vec::with_capacity(n);
    if makers.is_empty() {
        return out;
    }
    for i in 0..n {
        out.push(makers[i % makers.len()](idx, rng));
    }
    out
}

fn mix_salt(mix: Mix) -> u64 {
    match mix {
        Mix::TraversalHeavy => 0x11,
        Mix::SimilarityHeavy => 0x22,
        Mix::UpdateHeavy => 0x33,
        Mix::Mixed => 0x44,
    }
}

/// Verify a plan meets the §4.2.1 per-bucket sample floor: every exercised,
/// budgeted bucket must carry at least `floor + WARMUP` instances so that, after
/// the warmup discard, the retained set meets `floor` (spec §3.7). A plan that
/// starves a bucket is never written — the failure mode this catches
/// (plausible-looking numbers from 2–3 order statistics) is exactly what §3.7
/// exists to prevent.
pub fn plan_self_check(plan: &[WorkloadOp], floor: usize) -> Result<(), String> {
    let need = floor + WARMUP;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for op in plan {
        if matches!(op, WorkloadOp::Commit) {
            continue; // commit is plan structure, not a measured bucket
        }
        *counts.entry(op.bucket()).or_default() += 1;
    }
    for (bucket, n) in &counts {
        // Only budgeted buckets carry the floor; `commit`/`open` are n/a in M0.
        if crate::output::budget_for(bucket).p95_us.is_none() {
            continue;
        }
        if *n < need {
            return Err(format!(
                "bucket {bucket} has {n} instances; the §4.2.1 floor needs \
                 >= {need} (floor {floor} + warmup {WARMUP})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate;
    use crate::params::{Fanout, Parameters, SizeClass};

    fn small() -> Fixture {
        generate(Parameters::new(SizeClass::Small, Fanout::Medium, 1))
    }

    #[test]
    fn measurement_plan_covers_every_budget_bucket() {
        let fx = small();
        let plan = measurement_plan(&fx, 100);
        let buckets: std::collections::BTreeSet<String> = plan.iter().map(|o| o.bucket()).collect();
        for expected in [
            "neighbors",
            "traverse:max_hops=2",
            "traverse:max_hops=5",
            "shortest_path:hops",
            "shortest_path:weighted",
            "property_eq:sel=rare",
            "property_eq:sel=uncommon",
            "property_eq:sel=common",
            "property_range:sel=rare",
            "property_range:sel=uncommon",
            "property_range:sel=common",
            "similar_nodes:cand=100",
            "similar_nodes:cand=1000",
            "similar_nodes:cand=10000",
            "update_edge_weight",
            "decay_edges:batch=1000",
            "decay_edges:batch=10000",
            "decay_edges:batch=100000",
        ] {
            assert!(buckets.contains(expected), "missing bucket {expected}");
        }
    }

    #[test]
    fn measurement_plan_meets_the_floor_and_self_checks() {
        let fx = small();
        let floor = 50; // small test floor
        let plan = measurement_plan(&fx, floor + WARMUP);
        assert!(plan_self_check(&plan, floor).is_ok());
        // One short of the floor must be rejected.
        let short = measurement_plan(&fx, floor + WARMUP - 1);
        assert!(plan_self_check(&short, floor).is_err());
    }

    #[test]
    fn neighbors_varies_direction_and_filters() {
        let fx = small();
        let plan = measurement_plan(&fx, 200);
        let mut dirs = std::collections::BTreeSet::new();
        let mut filter_counts = std::collections::BTreeSet::new();
        for op in &plan {
            if let WorkloadOp::Neighbors {
                dir, edge_types, ..
            } = op
            {
                dirs.insert(*dir);
                filter_counts.insert(edge_types.len());
            }
        }
        assert!(dirs.len() > 1, "neighbors direction never varied");
        assert!(
            filter_counts.len() > 1,
            "edge-type filter count never varied"
        );
    }

    #[test]
    fn start_nodes_are_not_three_fixed_ids() {
        let fx = small();
        let plan = measurement_plan(&fx, 200);
        let starts: std::collections::BTreeSet<u64> = plan
            .iter()
            .filter_map(|o| match o {
                WorkloadOp::Neighbors { start, .. } | WorkloadOp::Traverse { start, .. } => {
                    Some(*start)
                }
                _ => None,
            })
            .collect();
        assert!(
            starts.len() > 10,
            "start nodes collapsed to {} ids",
            starts.len()
        );
    }

    #[test]
    fn update_exercises_all_three_weight_ops() {
        let fx = small();
        let plan = measurement_plan(&fx, 200);
        let mut ops = std::collections::BTreeSet::new();
        for o in &plan {
            if let WorkloadOp::UpdateEdgeWeight { weight_op, .. } = o {
                ops.insert(*weight_op);
            }
        }
        assert_eq!(ops.len(), 3, "not all Set/Add/Multiply exercised");
    }

    #[test]
    fn mix_plan_is_deterministic_and_meets_floor() {
        let fx = small();
        let a = mix_plan(&fx, Mix::Mixed, 60_000);
        let b = mix_plan(&fx, Mix::Mixed, 60_000);
        assert_eq!(a.len(), b.len());
        // Same sequence of buckets → deterministic.
        let ba: Vec<String> = a.iter().map(|o| o.bucket()).collect();
        let bb: Vec<String> = b.iter().map(|o| o.bucket()).collect();
        assert_eq!(ba, bb);
        assert!(a.iter().any(|o| matches!(o, WorkloadOp::Commit)));
        // Every budgeted bucket the mix exercises meets a small floor.
        assert!(plan_self_check(&a, 500).is_ok(), "mix starved a bucket");
    }
}
