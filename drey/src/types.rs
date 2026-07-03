//! Core data model (PRD §7, §9.1).
//!
//! Caller-facing types are strings (`NodeType`, `EdgeType`); the store interns
//! them internally (PRD §9.2). IDs are opaque `u64` newtypes, monotonic and
//! durable across reload (PRD §7.4). Weights are `f32` — the precision named in
//! the §9.2 signatures and carried through to export (open question 2 resolved
//! to `f32` for v0.1).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Stable node identity within a graph instance (PRD §7.4).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub u64);

/// Stable edge identity within a graph instance (PRD §7.4).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EdgeId(pub u64);

/// Caller-defined node type label. Not interpreted by the crate (PRD §7.1).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeType(pub String);

/// Caller-defined edge type label. Not interpreted by the crate (PRD §7.2).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EdgeType(pub String);

impl NodeType {
    pub fn new(s: impl Into<String>) -> Self {
        NodeType(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl EdgeType {
    pub fn new(s: impl Into<String>) -> Self {
        EdgeType(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Traversal / adjacency direction (PRD §9.1).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Outbound,
    Inbound,
    Both,
}

/// A scalar property value — the leaf type. Lists hold these (PRD §7.3).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Scalar {
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

/// A property value over the v0.1 value set (PRD §7.3). No nested `Map`, no
/// list-of-list — hierarchy is composed via a metadata subgraph, not nesting.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<Scalar>),
}

/// A node's or edge's property map. Ordered so serialization is deterministic.
pub type Properties = BTreeMap<String, Value>;

/// A fixed-dimension embedding vector (PRD §7.1). Dimensionality is enforced
/// against the node type's declaration, never interpreted otherwise.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Embedding(pub Vec<f32>);

impl Embedding {
    pub fn new(v: Vec<f32>) -> Self {
        Embedding(v)
    }
    pub fn dim(&self) -> usize {
        self.0.len()
    }
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }
}

/// A materialized node returned from a query (PRD §9.3).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Node {
    pub id: NodeId,
    pub node_type: NodeType,
    pub properties: Properties,
    pub embedding: Option<Embedding>,
}

/// A materialized edge returned from a query (PRD §9.3).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Edge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    pub edge_type: EdgeType,
    pub weight: f32,
    pub properties: Properties,
}

impl Value {
    /// The scalar view of a value, for the scalar property index (PRD §8).
    /// Only the scalar variants are indexable; `Null`, `Bytes`, and `List`
    /// return `None` and are never index keys.
    pub fn as_scalar(&self) -> Option<Scalar> {
        match self {
            Value::Bool(b) => Some(Scalar::Bool(*b)),
            Value::I64(i) => Some(Scalar::I64(*i)),
            Value::F64(x) => Some(Scalar::F64(*x)),
            Value::String(s) => Some(Scalar::String(s.clone())),
            Value::Null | Value::Bytes(_) | Value::List(_) => None,
        }
    }

    /// Whether a value is well-formed for v0.1. The variants are non-nesting by
    /// construction (no `Map`, `List` holds only [`Scalar`]). The one real check
    /// is a length ceiling: the on-disk codec prefixes byte/string/list lengths
    /// with a `u32`, so a value whose length does not fit in `u32` would be
    /// silently truncated on write and corrupt the WAL/snapshot. Reject it here,
    /// at the API boundary, so oversized data never reaches the codec.
    pub fn is_valid(&self) -> bool {
        const MAX: usize = u32::MAX as usize;
        match self {
            Value::String(s) => s.len() <= MAX,
            Value::Bytes(b) => b.len() <= MAX,
            Value::List(items) => {
                items.len() <= MAX
                    && items.iter().all(|s| match s {
                        Scalar::String(s) => s.len() <= MAX,
                        _ => true,
                    })
            }
            Value::Null | Value::Bool(_) | Value::I64(_) | Value::F64(_) => true,
        }
    }
}

impl Scalar {
    /// A total order over scalars, needed for the range-capable property index
    /// (PRD §8) and deterministic results.
    ///
    /// Two sharp edges the PRD leaves to the implementation, pinned here:
    /// - **Cross-type**: values of different scalar variants are ordered by a
    ///   fixed variant rank (`Bool < I64 < F64 < String`) rather than
    ///   panicking. A well-formed consumer keys a property to one type, so this
    ///   only governs pathological mixed columns, but it must be total.
    /// - **`F64` NaN**: ordered by [`f64::total_cmp`], which places `NaN` at the
    ///   ends deterministically instead of making comparisons non-total.
    pub fn total_order(&self, other: &Scalar) -> Ordering {
        fn rank(s: &Scalar) -> u8 {
            match s {
                Scalar::Bool(_) => 0,
                Scalar::I64(_) => 1,
                Scalar::F64(_) => 2,
                Scalar::String(_) => 3,
            }
        }
        match (self, other) {
            (Scalar::Bool(a), Scalar::Bool(b)) => a.cmp(b),
            (Scalar::I64(a), Scalar::I64(b)) => a.cmp(b),
            (Scalar::F64(a), Scalar::F64(b)) => a.total_cmp(b),
            (Scalar::String(a), Scalar::String(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

/// A wrapper giving [`Scalar`] a total `Ord` for use as a `BTreeMap` key in the
/// property index. Keeps the total-order rules in [`Scalar::total_order`].
#[derive(Clone, Debug)]
pub struct ScalarKey(pub Scalar);

impl PartialEq for ScalarKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_order(&other.0) == Ordering::Equal
    }
}
impl Eq for ScalarKey {}
impl PartialOrd for ScalarKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScalarKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_order(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_total_order_is_total_across_types_and_nan() {
        let nan = Scalar::F64(f64::NAN);
        // total_cmp makes NaN comparable to itself.
        assert_eq!(nan.total_order(&nan), Ordering::Equal);
        // Cross-type never panics and is consistent.
        let b = Scalar::Bool(true);
        let i = Scalar::I64(0);
        assert_eq!(b.total_order(&i), Ordering::Less);
        assert_eq!(i.total_order(&b), Ordering::Greater);
    }

    #[test]
    fn scalar_key_orders_in_btreemap() {
        use std::collections::BTreeMap;
        let mut m: BTreeMap<ScalarKey, u32> = BTreeMap::new();
        m.insert(ScalarKey(Scalar::I64(3)), 3);
        m.insert(ScalarKey(Scalar::I64(1)), 1);
        m.insert(ScalarKey(Scalar::I64(2)), 2);
        let ordered: Vec<u32> = m.values().copied().collect();
        assert_eq!(ordered, vec![1, 2, 3]);
    }
}
