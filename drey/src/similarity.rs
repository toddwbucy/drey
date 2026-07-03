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
        let mut scored: Vec<(NodeId, f32)> = candidates
            .iter()
            .map(|(n, emb)| (*n, metric.score(query, emb)))
            .collect();
        if metric.higher_is_better() {
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        } else {
            scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        }
        scored.truncate(k);
        scored
    }
}

/// A reachability constraint composed into a similarity query (PRD §9.4).
#[derive(Clone, Debug)]
pub struct ReachabilityFilter {
    pub from: NodeId,
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
    pub node_types: Option<Vec<NodeType>>,
    pub property_filter: Option<PropertyQuery>,
    pub within: Option<ReachabilityFilter>,
    /// Permit scanning more than the config ceiling of candidates. Off by
    /// default so an unfiltered query is bounded (PRD §13.1).
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
        let mut candidates = self.candidate_set(&query)?;

        // 2. Keep only nodes with a same-dimension embedding (enforces
        //    dimensionality, PRD §9.4 / §17).
        let qdim = query.vector.dim();
        candidates.retain(|n| {
            self.store
                .nodes
                .get(n)
                .and_then(|r| r.embedding.as_ref())
                .map(|e| e.len() == qdim)
                .unwrap_or(false)
        });

        // 3. Bound the scan (PRD §13.1).
        let ceiling = self.config.scan_ceiling.max_candidates;
        if candidates.len() > ceiling && !query.allow_full_scan {
            return Err(Error::UnsupportedQuery(format!(
                "similarity candidate set {} exceeds scan ceiling {}; narrow the filters or set allow_full_scan",
                candidates.len(),
                ceiling
            )));
        }

        // 4–5. Score + rank the survivors behind the evaluation seam (PRD §13.2),
        // so an ANN structure can replace the exhaustive scan without touching
        // this method. Materialize the candidate `(id, embedding)` slice here —
        // the seam ranks a pre-filtered set (PRD §13.1 order), it does not own the
        // vector space.
        let q = query.vector.as_slice();
        let cand: Vec<(NodeId, &[f32])> = candidates
            .into_iter()
            .map(|n| {
                let emb = self.store.nodes[&n].embedding.as_ref().unwrap();
                (NodeId(n), emb.as_slice())
            })
            .collect();
        let evaluator: &dyn SimilarityEvaluator = &ExhaustiveScan;
        Ok(evaluator.top_k(q, query.metric, &cand, query.k))
    }

    /// The candidate node set surviving all non-vector filters.
    fn candidate_set(&self, query: &SimilarityQuery) -> Result<HashSet<u64>> {
        // Start from the node-type filter, or all nodes.
        let mut set: HashSet<u64> = match &query.node_types {
            Some(types) => {
                let mut s = HashSet::new();
                for t in types {
                    for id in self.nodes_by_type(t)? {
                        s.insert(id.0);
                    }
                }
                s
            }
            None => self.store.nodes.keys().copied().collect(),
        };

        // Intersect with the property filter.
        if let Some(pf) = &query.property_filter {
            let allowed: HashSet<u64> = self
                .nodes_by_property(pf.clone())?
                .into_iter()
                .map(|n| n.0)
                .collect();
            set.retain(|n| allowed.contains(n));
        }

        // Intersect with reachability.
        if let Some(r) = &query.within {
            let reachable = self.reachable_set(r)?;
            set.retain(|n| reachable.contains(n));
        }

        Ok(set)
    }

    /// The set of nodes reachable from `filter.from` within `max_hops`,
    /// honoring direction, edge types, and min weight.
    ///
    /// A bounded node-only BFS with a visited set — never full path enumeration,
    /// which would blow up exponentially on hub-heavy graphs. Work is O(V + E)
    /// within the hop bound.
    fn reachable_set(&self, filter: &ReachabilityFilter) -> Result<HashSet<u64>> {
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
