//! Vector similarity as a composable predicate (PRD §9.4, §13).
//!
//! Similarity is not a separate search subsystem: structural and property
//! filters run *first*, and only the surviving candidates' vectors are scored,
//! by exhaustive scan (PRD §13.1). The scan is bounded by the config ceiling
//! unless the caller explicitly opts into a full scan, so `similar_nodes` can
//! never silently become a full vector-database sweep.

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::query::PropertyQuery;
use crate::traverse::DirectionOpt;
use crate::types::{EdgeType, Embedding, NodeId, NodeType};

/// Distance/similarity metric (PRD §9.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimilarityMetric {
    /// Cosine similarity; higher is more similar.
    Cosine,
    /// Dot product; higher is more similar.
    Dot,
    /// Euclidean distance; lower is more similar.
    Euclidean,
}

impl SimilarityMetric {
    /// Whether a larger score means more similar (governs ranking direction).
    fn higher_is_better(self) -> bool {
        matches!(self, SimilarityMetric::Cosine | SimilarityMetric::Dot)
    }

    fn score(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            SimilarityMetric::Dot => dot(a, b),
            SimilarityMetric::Cosine => {
                let na = dot(a, a).sqrt();
                let nb = dot(b, b).sqrt();
                if na == 0.0 || nb == 0.0 {
                    0.0
                } else {
                    dot(a, b) / (na * nb)
                }
            }
            SimilarityMetric::Euclidean => a
                .iter()
                .zip(b)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f32>()
                .sqrt(),
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The internal similarity-evaluation seam (design commitment 6 / PRD §13.2): so
/// an ANN structure can replace the scan without an API change. The evaluator
/// ranks an **already-filtered** candidate set, because PRD §13.1 fixes the order
/// as *filters first, then vector search over survivors* — an ANN impl must then
/// prove it restricts to the given candidates rather than returning global
/// top-k. [`ExhaustiveScan`] is the only implementation today (closed decision:
/// no ANN in v0.1 unless the M3 gate fails, which it does not).
pub(crate) trait SimilarityEvaluator {
    /// Rank `candidates` (each `(id, embedding)`, already structural/property
    /// filtered and dimension-checked) against `query` by `metric`; return the
    /// top `k` best-first with scores, tie-broken by node id for determinism.
    fn top_k(
        &self,
        query: &[f32],
        metric: SimilarityMetric,
        candidates: &[(NodeId, &[f32])],
        k: usize,
    ) -> Vec<(NodeId, f32)>;
}

/// Bounded exhaustive scan: score every candidate, rank, truncate. O(candidates
/// × dim); the caller bounds the candidate count against the config ceiling.
pub(crate) struct ExhaustiveScan;

impl SimilarityEvaluator for ExhaustiveScan {
    fn top_k(
        &self,
        query: &[f32],
        metric: SimilarityMetric,
        candidates: &[(NodeId, &[f32])],
        k: usize,
    ) -> Vec<(NodeId, f32)> {
        if k == 0 {
            return Vec::new(); // before the O(n·dim) scoring pass, not after
        }
        // Finite inputs do not guarantee a finite score: an f32 dot product can
        // overflow to ±inf, and cosine's inf/inf is NaN. Under the descending
        // `total_cmp` sort NaN ranks as the largest value, so an overflowed score
        // would poison the top-k exactly as a non-finite *input* would (which we
        // reject upstream). Map any non-finite score to the worst rank instead.
        let worst = if metric.higher_is_better() {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        };
        let mut scored: Vec<(NodeId, f32)> = candidates
            .iter()
            .map(|(n, emb)| {
                let s = metric.score(query, emb);
                (*n, if s.is_finite() { s } else { worst })
            })
            .collect();
        // Rank comparator: best first, id tie-break for determinism.
        let better = if metric.higher_is_better() {
            |a: &(NodeId, f32), b: &(NodeId, f32)| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
        } else {
            |a: &(NodeId, f32), b: &(NodeId, f32)| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0))
        };
        // Partial selection before the sort: O(n + k log k) instead of
        // O(n log n) on the full candidate set. The comparator is total
        // (total_cmp + id tie-break), so the k winners are exactly the k the
        // full sort would produce — determinism is preserved.
        if scored.len() > k {
            scored.select_nth_unstable_by(k - 1, better);
            scored.truncate(k);
        }
        scored.sort_by(better);
        scored
    }
}

/// A reachability constraint composed into a similarity query (PRD §9.4).
#[derive(Clone, Debug)]
pub struct ReachabilityFilter {
    pub from: NodeId,
    /// Reachability depth. Like [`crate::traverse::TraversalOptions::max_hops`],
    /// any value above the internal cap of 64 ([`crate::traverse::MAX_TRAVERSAL_HOPS`])
    /// is silently clamped to it, so nodes reachable only at hops 65.. are
    /// excluded from the candidate set.
    pub max_hops: usize,
    pub edge_types: Vec<EdgeType>,
    pub min_weight: Option<f32>,
    pub direction: DirectionOpt,
}

/// A hybrid similarity query (PRD §9.4).
#[derive(Clone, Debug)]
pub struct SimilarityQuery {
    pub vector: Embedding,
    pub metric: SimilarityMetric,
    pub k: usize,
    /// Restrict candidates to these node types. `None` means no type
    /// constraint; `Some(vec![])` names an empty allow-list and deliberately
    /// matches **nothing** — the two are not interchangeable.
    pub node_types: Option<Vec<NodeType>>,
    pub property_filter: Option<PropertyQuery>,
    pub within: Option<ReachabilityFilter>,
    /// Permit probing more than the config ceiling of candidates. Off by
    /// default so an unfiltered query is bounded (PRD §13.1). The ceiling
    /// bounds the candidates *examined* (the filter survivors the scan must
    /// probe for embeddings), not merely the vectors scored.
    pub allow_full_scan: bool,
}

impl SimilarityQuery {
    pub fn new(vector: Embedding, metric: SimilarityMetric, k: usize) -> Self {
        SimilarityQuery {
            vector,
            metric,
            k,
            node_types: None,
            property_filter: None,
            within: None,
            allow_full_scan: false,
        }
    }
}

impl Graph {
    /// Top-`k` most similar nodes to a query vector, after structural and
    /// property filtering (PRD §9.4). Returns `(node, score)` best-first; the
    /// crate never ranks beyond the raw metric score.
    ///
    /// Only candidates whose stored embedding has the same dimension as `query`
    /// are scored; others are dropped. A query vector whose dimension matches no
    /// stored embedding therefore returns `Ok(vec![])`, not an error — the graph
    /// may hold several node types at different embedding dimensions, so a
    /// dimension that misses one type is not globally invalid.
    ///
    /// The scan ceiling bounds the candidates the query may *examine* — the
    /// survivors of the structural/property filters that must each be probed
    /// for a scorable embedding — not merely the vectors ultimately scored.
    /// An unfiltered query over a graph larger than the ceiling is rejected
    /// even when few nodes carry embeddings, unless `allow_full_scan` is set.
    pub fn similar_nodes(&self, query: SimilarityQuery) -> Result<Vec<(NodeId, f32)>> {
        // A non-finite query vector degenerates every score to NaN and returns a
        // meaningless ranking; reject it (stored embeddings are already finite,
        // enforced at set_node_embedding).
        if let Some(bad) = query.vector.as_slice().iter().position(|x| !x.is_finite()) {
            return Err(Error::InvalidPropertyValue(format!(
                "query vector component {bad} is not finite"
            )));
        }
        // 1. Structural + property filters first (PRD §13.1 evaluation order).
        let candidates = self.candidate_set(&query)?;

        // 2. Bound the scan (PRD §13.1) — on the candidates the query will
        //    *probe*, before any per-candidate work. Checking only the scored
        //    (dimension-matching) survivors, as this used to, let an unfiltered
        //    query walk the entire node set as long as few nodes carried
        //    embeddings — O(V) work the ceiling exists to forbid.
        self.ensure_scan_ceiling(candidates.len(), query.allow_full_scan)?;

        // 3. Keep only same-dimension embeddings (enforces dimensionality, PRD
        //    §9.4 / §17) and materialize the `(id, embedding)` slice for the seam
        //    in one pass — one hash lookup per candidate, not two. A query vector
        //    whose dimension matches no stored embedding yields an empty set here
        //    and so an empty result (documented on `similar_nodes`).
        let qdim = query.vector.dim();
        let cand: Vec<(NodeId, &[f32])> = candidates
            .into_iter()
            .filter_map(|n| {
                let emb = self.store.nodes.get(&n)?.embedding.as_ref()?;
                (emb.len() == qdim).then_some((NodeId(n), emb.as_slice()))
            })
            .collect();

        // 4–5. Score + rank the survivors behind the evaluation seam (PRD §13.2),
        // so an ANN structure can replace the exhaustive scan without touching
        // this method. The seam ranks a pre-filtered set (PRD §13.1 order); it
        // does not own the vector space.
        let q = query.vector.as_slice();
        let evaluator: &dyn SimilarityEvaluator = &ExhaustiveScan;
        Ok(evaluator.top_k(q, query.metric, &cand, query.k))
    }

    /// The single enforcement point for the scan ceiling (PRD §13.1): the
    /// policy — what is counted (candidates *probed*) and how it is lifted
    /// (`allow_full_scan`) — lives here so the pre-materialization check in
    /// `candidate_set` and the post-filter check in `similar_nodes` cannot
    /// drift apart.
    fn ensure_scan_ceiling(&self, candidates: usize, allow_full_scan: bool) -> Result<()> {
        let ceiling = self.config.scan_ceiling.max_candidates;
        if candidates > ceiling && !allow_full_scan {
            return Err(Error::UnsupportedQuery(format!(
                "similarity candidate set {candidates} exceeds scan ceiling {ceiling}; \
                 narrow the filters or set allow_full_scan"
            )));
        }
        Ok(())
    }

    /// The candidate node set surviving all non-vector filters.
    ///
    /// Seeds from the most selective *available* constraint rather than always
    /// materializing every node id and shrinking: a selective property filter is
    /// index-backed and typically yields a few hundred nodes, so starting there
    /// avoids allocating a set of the whole graph only to `retain` it away. The
    /// whole-node-set path runs only when no filter constrains the query (and the
    /// scan ceiling then bounds it).
    fn candidate_set(&self, query: &SimilarityQuery) -> Result<HashSet<u64>> {
        let mut set: Option<HashSet<u64>> = None;

        // Property filter first: it resolves through the ordered scalar index
        // (equality/range) and is usually the tightest constraint.
        if let Some(pf) = &query.property_filter {
            set = Some(
                self.nodes_by_property(pf.clone())?
                    .into_iter()
                    .map(|n| n.0)
                    .collect(),
            );
        }

        // Node-type constraint: read the type buckets straight from the store
        // (no intermediate sorted Vec — the set discards order anyway). An
        // unregistered type in the filter errors, matching `nodes_by_type`.
        if let Some(types) = &query.node_types {
            let mut allowed: HashSet<u64> = HashSet::new();
            for t in types {
                let tid = self.resolve_registered_type(t)?;
                if let Some(ids) = self.store.nodes_by_type.get(&tid) {
                    allowed.extend(ids.iter().copied());
                }
            }
            set = Some(match set {
                Some(mut s) => {
                    s.retain(|n| allowed.contains(n));
                    s
                }
                None => allowed,
            });
        }

        // Reachability constraint.
        if let Some(r) = &query.within {
            let reachable = self.reachable_set(r)?;
            set = Some(match set {
                Some(mut s) => {
                    s.retain(|n| reachable.contains(n));
                    s
                }
                None => reachable,
            });
        }

        match set {
            Some(s) => Ok(s),
            None => {
                // No constraint named a subset → the whole node set. Enforce the
                // ceiling BEFORE materializing: the bound exists to cap work, so
                // allocating an O(V) set just to count-and-reject it (or probe
                // every node for an embedding) would defeat it on large graphs.
                self.ensure_scan_ceiling(self.store.nodes.len(), query.allow_full_scan)?;
                Ok(self.store.nodes.keys().copied().collect())
            }
        }
    }

    /// The set of nodes reachable from `filter.from` within `max_hops`,
    /// honoring direction, edge types, and min weight.
    ///
    /// A bounded node-only BFS with a visited set — never full path enumeration,
    /// which would blow up exponentially on hub-heavy graphs. Work is O(V + E)
    /// within the hop bound.
    fn reachable_set(&self, filter: &ReachabilityFilter) -> Result<HashSet<u64>> {
        crate::graph::validate_min_weight(filter.min_weight)?;
        // Validate the anchor like neighbors/traverse/shortest_path do. Without
        // this a missing `from` yields an empty reachable set (just `{from}`
        // intersected away), so `similar_nodes` returns Ok(empty) instead of the
        // NodeNotFound every other traversal anchor reports.
        if !self.store.nodes.contains_key(&filter.from.0) {
            return Err(Error::NodeNotFound(filter.from));
        }
        let type_ids = self.resolve_type_ids(&filter.edge_types);
        let max_hops = filter.max_hops.min(crate::traverse::MAX_TRAVERSAL_HOPS);

        // `from` itself is reachable at zero hops.
        let mut visited: HashSet<u64> = HashSet::from([filter.from.0]);
        let mut frontier = vec![filter.from.0];
        for _ in 0..max_hops {
            let mut next = Vec::new();
            for node in frontier.drain(..) {
                for (_edge, other) in
                    self.steps(node, filter.direction, &type_ids, filter.min_weight)
                {
                    if visited.insert(other) {
                        next.push(other);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(visited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustive_scan_ranks_through_the_trait_object() {
        // Dispatch the evaluation seam via a trait object (the swap point for a
        // future ANN): the exhaustive scan ranks the pre-filtered candidates
        // best-first with an id tie-break.
        let a = [1.0f32, 0.0];
        let b = [0.9f32, 0.1];
        let c = [0.0f32, 1.0];
        let cand = [
            (NodeId(1), &a[..]),
            (NodeId(2), &b[..]),
            (NodeId(3), &c[..]),
        ];
        let eval: &dyn SimilarityEvaluator = &ExhaustiveScan;
        let top = eval.top_k(&[1.0, 0.0], SimilarityMetric::Cosine, &cand, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, NodeId(1)); // exact match ranks first
        assert_eq!(top[1].0, NodeId(2)); // near match second
    }

    #[test]
    fn exhaustive_scan_lower_is_better_euclidean_with_tie_break() {
        // Euclidean is lower-is-better: the scan must sort ascending (nearest
        // first), and equal distances tie-break by ascending node id.
        let near = [0.1f32, 0.0]; // distance 0.1 from the origin query
        let tie = [0.1f32, 0.0]; // same distance -> id tie-break
        let far = [5.0f32, 0.0]; // distance 5.0
        let cand = [
            (NodeId(2), &tie[..]),
            (NodeId(3), &far[..]),
            (NodeId(1), &near[..]),
        ];
        let eval: &dyn SimilarityEvaluator = &ExhaustiveScan;
        let top = eval.top_k(&[0.0, 0.0], SimilarityMetric::Euclidean, &cand, 3);
        assert_eq!(top[0].0, NodeId(1)); // nearest, lower id wins the tie
        assert_eq!(top[1].0, NodeId(2)); // equally near, higher id
        assert_eq!(top[2].0, NodeId(3)); // farthest ranks last
    }
}
