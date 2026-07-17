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

/// Traversal / adjacency direction (PRD §9.1). `Outbound` is the default so the
/// options structs that carry a direction can derive `Default`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Direction {
    #[default]
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
    /// Three sharp edges the PRD leaves to the implementation, pinned here:
    /// - **Numeric cross-variant**: `I64` and `F64` compare **by numeric
    ///   value**, exactly across the whole `i64` range (no lossy cast), so
    ///   `Eq(F64(5.0))` matches a stored `I64(5)` and a `Range` may mix
    ///   variants in its bounds. Before this, mixed numeric comparisons were
    ///   decided by variant rank — a `Range{min: I64(0), ..}` silently admitted
    ///   every `F64`, including negatives.
    /// - **Non-numeric cross-type**: ordered by a fixed variant rank
    ///   (`Bool < numeric < String`) rather than panicking. A well-formed
    ///   consumer keys a property to one type, so this only governs
    ///   pathological mixed columns, but it must be total.
    /// - **`F64` NaN**: every NaN bit pattern compares equal, ranked above all
    ///   numeric values (and still below `String`). Bit-pattern order
    ///   (`total_cmp` alone) made `Eq(F64(NAN))` matches platform-dependent —
    ///   a runtime `0.0/0.0` is a negative-quiet NaN on x86 and would not match
    ///   the positive `f64::NAN` constant.
    pub fn total_order(&self, other: &Scalar) -> Ordering {
        fn rank(s: &Scalar) -> u8 {
            match s {
                Scalar::Bool(_) => 0,
                Scalar::I64(_) | Scalar::F64(_) => 1,
                Scalar::String(_) => 2,
            }
        }
        match (self, other) {
            (Scalar::Bool(a), Scalar::Bool(b)) => a.cmp(b),
            (Scalar::I64(a), Scalar::I64(b)) => a.cmp(b),
            (Scalar::F64(a), Scalar::F64(b)) => cmp_f64(*a, *b),
            (Scalar::I64(a), Scalar::F64(b)) => cmp_i64_f64(*a, *b),
            (Scalar::F64(a), Scalar::I64(b)) => cmp_i64_f64(*b, *a).reverse(),
            (Scalar::String(a), Scalar::String(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

/// Total order over `f64` for the property index: signed zeros collapse (an
/// `Eq(F64(0.0))` query must match a stored `-0.0`, which are IEEE-equal), and
/// every NaN bit pattern is one value ranked above all numbers, so NaN handling
/// is deterministic across platforms and payloads.
fn cmp_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let norm = |x: f64| if x == 0.0 { 0.0 } else { x };
            norm(a).total_cmp(&norm(b))
        }
    }
}

/// Exact numeric comparison of an `i64` against an `f64` — no cast of `i` to
/// `f64` (lossy above 2^53) and no cast of `f` to `i64` (saturating/UB-prone).
/// NaN ranks above every integer, matching [`cmp_f64`].
fn cmp_i64_f64(i: i64, f: f64) -> Ordering {
    if f.is_nan() {
        return Ordering::Less; // numbers sort below NaN
    }
    // 2^63 and -2^63 are exactly representable in f64; outside that window the
    // float is strictly beyond the whole i64 range.
    if f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    // f is finite and floor(f) fits in i64 (floor(f) ≤ f < 2^63, and
    // f ≥ -2^63 so floor(f) ≥ -2^63). Comparing i against floor(f) is exact;
    // a leftover fractional part only matters on equality.
    let floor = f.floor();
    match i.cmp(&(floor as i64)) {
        Ordering::Equal if f > floor => Ordering::Less, // i == floor(f) < f
        ord => ord,
    }
}

/// A wrapper giving [`Scalar`] a total `Ord` for use as a `BTreeMap` key in the
/// property index. Keeps the total-order rules in [`Scalar::total_order`].
///
/// Internal: this is the index's key encoding, not part of the public API (the
/// public surface takes/returns [`Scalar`], never `ScalarKey`), so keeping it
/// `pub(crate)` leaves the index representation free to change.
#[derive(Clone, Debug)]
pub(crate) struct ScalarKey(pub(crate) Scalar);

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
    fn numeric_cross_variant_compares_by_value_exactly() {
        // Mixed I64/F64 comparisons are by numeric value, not variant rank
        // (a Range{min: I64(0)} must not admit F64(-5.0)).
        assert_eq!(
            Scalar::I64(5).total_order(&Scalar::F64(5.0)),
            Ordering::Equal
        );
        assert_eq!(
            Scalar::F64(-5.0).total_order(&Scalar::I64(0)),
            Ordering::Less
        );
        assert_eq!(
            Scalar::I64(0).total_order(&Scalar::F64(10.0)),
            Ordering::Less
        );
        // Exact above 2^53, where an i64→f64 cast would silently round: 2^53
        // and 2^53+1 both cast to 9007199254740992.0, but only 2^53 equals it.
        let two_pow_53 = 9_007_199_254_740_992i64;
        assert_eq!(
            Scalar::I64(two_pow_53).total_order(&Scalar::F64(9_007_199_254_740_992.0)),
            Ordering::Equal
        );
        assert_eq!(
            Scalar::I64(two_pow_53 + 1).total_order(&Scalar::F64(9_007_199_254_740_992.0)),
            Ordering::Greater
        );
        // Fractional parts decide equality ties in the right direction.
        assert_eq!(
            Scalar::I64(5).total_order(&Scalar::F64(5.5)),
            Ordering::Less
        );
        assert_eq!(
            Scalar::I64(6).total_order(&Scalar::F64(5.5)),
            Ordering::Greater
        );
        // ±inf bracket the whole integer range; NaN ranks above every number.
        assert_eq!(
            Scalar::I64(i64::MAX).total_order(&Scalar::F64(f64::INFINITY)),
            Ordering::Less
        );
        assert_eq!(
            Scalar::I64(i64::MIN).total_order(&Scalar::F64(f64::NEG_INFINITY)),
            Ordering::Greater
        );
        assert_eq!(
            Scalar::I64(i64::MAX).total_order(&Scalar::F64(f64::NAN)),
            Ordering::Less
        );
        // -2^63 is exactly representable and equals i64::MIN.
        assert_eq!(
            Scalar::I64(i64::MIN).total_order(&Scalar::F64(-9_223_372_036_854_775_808.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn all_nan_payloads_compare_equal_and_rank_above_numbers() {
        // A runtime 0.0/0.0 NaN carries a different bit pattern (negative-quiet
        // on x86) than the f64::NAN constant; the order must not care.
        let zero = f64::from(0u8); // opaque to constant folding
        let runtime_nan = zero / zero;
        assert_eq!(
            Scalar::F64(runtime_nan).total_order(&Scalar::F64(f64::NAN)),
            Ordering::Equal
        );
        assert_eq!(
            Scalar::F64(f64::NAN).total_order(&Scalar::F64(f64::INFINITY)),
            Ordering::Greater
        );
        // NaN is still an F64: it ranks below String like every number.
        assert_eq!(
            Scalar::F64(f64::NAN).total_order(&Scalar::String(String::new())),
            Ordering::Less
        );
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
