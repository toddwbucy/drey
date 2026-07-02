//! Property queries and type lookups (PRD §9.3).
//!
//! The core property query supports equality and range over scalars — the two
//! shapes the required scalar index (PRD §8) resolves without a scan. Richer
//! predicates (set exclusion, compound sorts, arbitrary boolean combos) are the
//! consumer's to compose over the returned set. When a queried property is not
//! indexed, the query still answers correctly by falling back to a filtered
//! scan over the type; the index is an optimization, not a correctness
//! requirement.

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::types::{NodeId, NodeType, Scalar, ScalarKey};

/// A scalar predicate (PRD §9.3).
#[derive(Clone, Debug)]
pub enum ScalarPredicate {
    Eq(Scalar),
    /// Half-open bounds allowed: either side may be `None`.
    Range {
        min: Option<Scalar>,
        max: Option<Scalar>,
    },
}

/// A property query over one `(node_type, key)` (PRD §9.3).
#[derive(Clone, Debug)]
pub struct PropertyQuery {
    pub node_type: NodeType,
    pub key: String,
    pub predicate: ScalarPredicate,
}

impl ScalarPredicate {
    fn matches(&self, value: &Scalar) -> bool {
        match self {
            ScalarPredicate::Eq(target) => value.total_order(target) == std::cmp::Ordering::Equal,
            ScalarPredicate::Range { min, max } => {
                let ge = min
                    .as_ref()
                    .is_none_or(|m| value.total_order(m) != std::cmp::Ordering::Less);
                let le = max
                    .as_ref()
                    .is_none_or(|m| value.total_order(m) != std::cmp::Ordering::Greater);
                ge && le
            }
        }
    }
}

impl Graph {
    /// Resolve a node type to its interned id, or error if it is not registered.
    /// Shared by the type and property queries so both stay in sync.
    fn resolve_registered_type(&self, node_type: &NodeType) -> Result<u32> {
        self.store
            .node_types
            .get(node_type.as_str())
            .filter(|id| self.store.embedding_dim.contains_key(id))
            .ok_or_else(|| {
                Error::InvalidNodeType(format!("node type {:?} not registered", node_type.as_str()))
            })
    }

    /// All node ids of a registered type (PRD §9.3). Returns an empty vector,
    /// never an error, for a registered type with zero members; querying an
    /// unregistered type errors.
    pub fn nodes_by_type(&self, node_type: &NodeType) -> Result<Vec<NodeId>> {
        let tid = self.resolve_registered_type(node_type)?;
        let mut ids: Vec<NodeId> = self
            .store
            .nodes_by_type
            .get(&tid)
            .map(|v| v.iter().map(|n| NodeId(*n)).collect())
            .unwrap_or_default();
        ids.sort_unstable(); // deterministic
        Ok(ids)
    }

    /// Nodes matching a property query (PRD §9.3). Empty vector, never an error,
    /// for a registered type with no matches; unregistered type errors.
    pub fn nodes_by_property(&self, query: PropertyQuery) -> Result<Vec<NodeId>> {
        let tid = self.resolve_registered_type(&query.node_type)?;

        let indexed = self.config.is_indexed(&query.node_type, &query.key);
        let mut out: Vec<NodeId> = if indexed {
            self.query_via_index(tid, &query)
        } else {
            self.query_via_scan(tid, &query)
        };
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Resolve through the ordered scalar index (PRD §8): equality is a point
    /// lookup, range is a bounded sub-map iteration — no full scan.
    fn query_via_index(&self, tid: u32, query: &PropertyQuery) -> Vec<NodeId> {
        let Some(tree) = self.store.prop_index.get(&(tid, query.key.clone())) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match &query.predicate {
            ScalarPredicate::Eq(target) => {
                if let Some(bucket) = tree.get(&ScalarKey(target.clone())) {
                    out.extend(bucket.iter().map(|n| NodeId(*n)));
                }
            }
            ScalarPredicate::Range { min, max } => {
                use std::ops::Bound;
                // Inverted bounds (min > max) match nothing. `BTreeMap::range`
                // panics on inverted bounds, so return empty early — matching the
                // scan path's behavior.
                if let (Some(lo), Some(hi)) = (min, max) {
                    if lo.total_order(hi) == std::cmp::Ordering::Greater {
                        return out;
                    }
                }
                let lower = min
                    .as_ref()
                    .map(|m| Bound::Included(ScalarKey(m.clone())))
                    .unwrap_or(Bound::Unbounded);
                let upper = max
                    .as_ref()
                    .map(|m| Bound::Included(ScalarKey(m.clone())))
                    .unwrap_or(Bound::Unbounded);
                for (_k, bucket) in tree.range((lower, upper)) {
                    out.extend(bucket.iter().map(|n| NodeId(*n)));
                }
            }
        }
        out
    }

    /// Correct fallback for unindexed properties: scan the type's members and
    /// filter. The index changes speed, not answers.
    fn query_via_scan(&self, tid: u32, query: &PropertyQuery) -> Vec<NodeId> {
        let Some(members) = self.store.nodes_by_type.get(&tid) else {
            return Vec::new();
        };
        members
            .iter()
            .filter_map(|n| {
                let rec = &self.store.nodes[n];
                let value = rec.properties.get(&query.key)?;
                let scalar = value.as_scalar()?;
                query.predicate.matches(&scalar).then_some(NodeId(*n))
            })
            .collect()
    }
}
