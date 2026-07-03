//! Synthetic fixture generation (spec §3).
//!
//! Deterministic from `(generator_version, seed, parameters)`. The graph is
//! hub-heavy by construction: exactly [`Parameters::edges`] edges are placed,
//! each endpoint drawn from a truncated-Zipf distribution over node ranks, so
//! the edge count is exact (not an approximation of per-node degree draws that
//! could never sum to the target — the trap the reference implementation fell
//! into), the mean out-degree is exactly `edges / nodes`, and both in- and
//! out-degree are skewed toward low-index hubs.
//!
//! Embeddings are approximately unit-normalized `f32` vectors with a 1% tail of
//! decimal-hostile components injected *after* normalization (spec §3.2), so the
//! byte-exact round-trip assertions at M2 have something to catch.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::params::Parameters;
use crate::rng::{self, DetRng, Phase};

use rand::Rng;

/// A generated fixture, held in memory before it is written to disk.
pub struct Fixture {
    pub params: Parameters,
    pub nodes: Vec<FixtureNode>,
    pub edges: Vec<FixtureEdge>,
    /// `(node_id, vector)` for the covered nodes, in ascending node-id order.
    pub embeddings: Vec<(u64, Vec<f32>)>,
}

/// A node as serialized to `nodes.jsonl`: `{"id","type","props"}` (spec §3.5).
#[derive(Serialize, Deserialize, Clone)]
pub struct FixtureNode {
    pub id: u64,
    #[serde(rename = "type")]
    pub node_type: String,
    pub props: Map<String, Json>,
}

/// An edge as serialized to `edges.jsonl`:
/// `{"id","from","to","type","weight","props"}` (spec §3.5).
#[derive(Serialize, Deserialize, Clone)]
pub struct FixtureEdge {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    #[serde(rename = "type")]
    pub edge_type: String,
    pub weight: f32,
    pub props: Map<String, Json>,
}

/// A cumulative-weight table over `n` ranks with weights `k^-s` (k = 1..=n),
/// truncated at `n`. Sampling returns a 0-based node index, hub-heavy toward 0.
struct ZipfTable {
    cdf: Vec<f64>,
    total: f64,
}

impl ZipfTable {
    fn new(n: usize, s: f64) -> Self {
        // `powf` is the one libm call on the generation path. It is deterministic
        // on a fixed host but not IEEE-754 correctly-rounded, so it is the reason
        // cross-host bit-identity is best-effort rather than contractual (see the
        // reproducibility contract in `crate::rng`). Same-host generation — what
        // the manifest checksums assert — is unaffected.
        let mut cdf = Vec::with_capacity(n);
        let mut cum = 0.0;
        for k in 1..=n {
            cum += (k as f64).powf(-s);
            cdf.push(cum);
        }
        ZipfTable { total: cum, cdf }
    }

    fn sample(&self, rng: &mut DetRng) -> usize {
        let target = rng.gen::<f64>() * self.total;
        // First index whose cumulative weight ≥ target.
        match self
            .cdf
            .binary_search_by(|x| x.partial_cmp(&target).unwrap())
        {
            Ok(i) => i,
            Err(i) => i.min(self.cdf.len() - 1),
        }
    }
}

/// Generate a fixture deterministically from its parameters.
pub fn generate(params: Parameters) -> Fixture {
    let nodes = generate_nodes(&params);
    let edges = generate_edges(&params);
    let embeddings = generate_embeddings(&params);
    Fixture {
        params,
        nodes,
        edges,
        embeddings,
    }
}

fn generate_nodes(params: &Parameters) -> Vec<FixtureNode> {
    let mut rng = rng::phase(params.seed, Phase::Nodes);
    let n = params.nodes as usize;
    let nt = params.node_types as usize;

    // Round-robin type assignment → near-equal, deterministic populations and a
    // clean within-type index for the p_cat selectivity bands.
    let per_type = |t: usize| -> u64 {
        let base = (n / nt) as u64;
        let rem = (n % nt) as u64;
        base + if (t as u64) < rem { 1 } else { 0 }
    };

    let mut nodes = Vec::with_capacity(n);
    for id in 0..n {
        let t = id % nt;
        let within = (id / nt) as u64; // 0-based index within this type
        let pop = per_type(t);
        let mut props = Map::new();
        // p_seq: unique, monotonic (also the recency-sequence stand-in).
        props.insert("p_seq".into(), Json::from(id as i64));
        // p_cat: disjoint selectivity bands ~0.1% / 1% / 10% of the type pop.
        props.insert("p_cat".into(), Json::from(p_cat(within, pop)));
        // p_score: uniform [0,1].
        props.insert("p_score".into(), Json::from(rng.gen::<f64>()));
        nodes.push(FixtureNode {
            id: id as u64,
            node_type: format!("nt_{t:02}"),
            props,
        });
    }
    nodes
}

/// Assign a category so that three distinguished values land at ~0.1%, ~1%,
/// and ~10% of the type population; the rest spread across filler categories.
fn p_cat(within: u64, pop: u64) -> String {
    let rare = (pop as f64 * 0.001).ceil() as u64;
    let uncommon = (pop as f64 * 0.01).ceil() as u64;
    let common = (pop as f64 * 0.10).ceil() as u64;
    if within < rare {
        "cat_rare".into()
    } else if within < rare + uncommon {
        "cat_uncommon".into()
    } else if within < rare + uncommon + common {
        "cat_common".into()
    } else {
        // Filler categories, ~1% each, so none rivals the distinguished bands.
        let filler = within % 100;
        format!("cat_f{filler:02}")
    }
}

fn generate_edges(params: &Parameters) -> Vec<FixtureEdge> {
    let mut rng = rng::phase(params.seed, Phase::Edges);
    let n = params.nodes as usize;
    let node_zipf = ZipfTable::new(n, params.degree_s);
    // Edge-type skew: a Zipf over edge types so type-filter selectivity varies.
    let et_zipf = ZipfTable::new(params.edge_types as usize, params.degree_s);

    // Truncated Zipf (spec §3.2): cap each node's total (in+out) degree at
    // max_degree. Without this the top node reaches ~50x the declared cap and the
    // manifest asserts a bound the data violates. A drawn endpoint already at the
    // cap is resampled; the spilled mass flattens onto lower ranks.
    let max_degree = params.max_degree;
    let mut degree = vec![0u32; n];
    let draw = |rng: &mut DetRng, degree: &[u32]| -> usize {
        for _ in 0..16 {
            let node = node_zipf.sample(rng);
            if degree[node] < max_degree {
                return node;
            }
        }
        degree.iter().position(|&d| d < max_degree).unwrap_or(0)
    };

    let mut edges = Vec::with_capacity(params.edges as usize);
    for id in 0..params.edges {
        // Draw endpoints hub-heavy but degree-capped; avoid self-loops.
        let from = draw(&mut rng, &degree);
        let mut to = draw(&mut rng, &degree);
        let mut guard = 0;
        while to == from && guard < 8 {
            to = draw(&mut rng, &degree);
            guard += 1;
        }
        if to == from {
            // Degree-aware deterministic fallback: the nearest node that is not
            // `from` and is still under the cap. A plain `(from + 1) % n` could
            // land on a node already at max_degree and push it over, failing the
            // self-check. Falls back to `from` only if the graph is fully
            // saturated (unreachable for valid params, where edges ≪ n·cap).
            to = (1..n)
                .map(|off| (from + off) % n)
                .find(|&cand| degree[cand] < max_degree)
                .unwrap_or(from);
        }
        degree[from] += 1;
        degree[to] += 1;
        let et = et_zipf.sample(&mut rng);
        edges.push(FixtureEdge {
            id,
            from: from as u64,
            to: to as u64,
            edge_type: format!("et_{et:02}"),
            // weight in (0, 1]: map [0,1) onto [MIN_POSITIVE, 1) so it is never
            // exactly zero.
            weight: rng.gen::<f32>() * (1.0 - f32::MIN_POSITIVE) + f32::MIN_POSITIVE,
            props: {
                let mut m = Map::new();
                m.insert("p_seq".into(), Json::from(id as i64));
                m
            },
        });
    }
    edges
}

fn generate_embeddings(params: &Parameters) -> Vec<(u64, Vec<f32>)> {
    let mut rng = rng::phase(params.seed, Phase::Embeddings);
    let dim = params.embed_dim as usize;
    let mut out = Vec::new();
    for id in 0..params.nodes {
        if rng.gen::<f64>() >= params.embed_coverage {
            continue; // not covered
        }
        // Uniform [-1,1] components, then normalize (sqrt only — no
        // transcendentals, so this is reproducible across platforms).
        let mut v: Vec<f32> = (0..dim).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        // Inject ~1% decimal-hostile components AFTER normalization (spec §3.2).
        let mut i = 0;
        while i < dim {
            match (i / 100) % 3 {
                0 => v[i] = f32::from_bits(1), // smallest denormal
                1 => v[i] = -0.0,              // negative zero
                _ => v[i] = f32::MIN_POSITIVE, // smallest normal
            }
            i += 100; // every 100th → ~1%
        }
        out.push((id, v));
    }
    out
}
