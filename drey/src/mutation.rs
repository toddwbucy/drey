//! Mutation-support types (PRD §9.2).

use std::collections::BTreeMap;

use crate::types::Value;

/// A patch over a property map: `Some(v)` sets or replaces a key, `None`
/// removes it. Distinguishing set-null from remove is deliberate — `Some(Null)`
/// stores an explicit null, `None` deletes the key.
#[derive(Clone, Default, Debug)]
pub struct PropertyPatch(pub BTreeMap<String, Option<Value>>);

impl PropertyPatch {
    pub fn new() -> Self {
        PropertyPatch(BTreeMap::new())
    }
    pub fn set(mut self, key: impl Into<String>, value: Value) -> Self {
        self.0.insert(key.into(), Some(value));
        self
    }
    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.0.insert(key.into(), None);
        self
    }
}

/// A weight-update operation (PRD §9.2). Bounds are a constraint on the update,
/// not a peer operation: the op is applied, then the result is clamped into
/// `bounds` if present.
#[derive(Clone, Copy, Debug)]
pub struct WeightUpdate {
    pub op: WeightOp,
    pub bounds: Option<(f32, f32)>,
}

#[derive(Clone, Copy, Debug)]
pub enum WeightOp {
    Set(f32),
    Add(f32),
    Multiply(f32),
}

impl WeightUpdate {
    pub fn set(v: f32) -> Self {
        WeightUpdate {
            op: WeightOp::Set(v),
            bounds: None,
        }
    }
    pub fn add(v: f32) -> Self {
        WeightUpdate {
            op: WeightOp::Add(v),
            bounds: None,
        }
    }
    pub fn multiply(v: f32) -> Self {
        WeightUpdate {
            op: WeightOp::Multiply(v),
            bounds: None,
        }
    }
    pub fn with_bounds(mut self, min: f32, max: f32) -> Self {
        self.bounds = Some((min, max));
        self
    }

    /// Apply to a current weight: run the op, then clamp into bounds
    /// (PRD §9.2 reading; the M0 stopgap the harness also implements).
    pub fn apply(&self, current: f32) -> f32 {
        let raw = match self.op {
            WeightOp::Set(v) => v,
            WeightOp::Add(v) => current + v,
            WeightOp::Multiply(v) => current * v,
        };
        match self.bounds {
            // `f32::clamp` panics when min > max or either bound is NaN. Use a
            // manual clamp that can't panic; malformed bounds (validated and
            // rejected by `update_edge_weight`) degrade to leaving `raw`
            // unclamped rather than aborting the process (PRD §18 no-panic).
            Some((min, max)) if min <= max => raw.max(min).min(max),
            _ => raw,
        }
    }

    /// Whether the bounds are well-formed (`min <= max`, neither NaN). Malformed
    /// bounds are rejected at the API (`update_edge_weight`) so a caller cannot
    /// reach the no-op-clamp fallback in [`WeightUpdate::apply`] silently.
    pub fn bounds_valid(&self) -> bool {
        match self.bounds {
            Some((min, max)) => min <= max, // false if either is NaN
            None => true,
        }
    }
}

/// How node removal treats incident edges (PRD §9.2). Default is the safe one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RemoveNodeMode {
    /// Reject removal while incident edges exist (default — no accidental orphaning).
    #[default]
    RejectIfEdgesExist,
    /// Remove incident edges as part of the node removal.
    RemoveIncidentEdges,
}

/// A filter over edges, used by decay and export (PRD §9.2, §9.5).
#[derive(Clone, Default, Debug)]
pub struct EdgeFilter {
    /// Restrict to these edge types (empty = all types).
    pub edge_types: Vec<crate::types::EdgeType>,
    /// Restrict to edges with weight ≥ this.
    pub min_weight: Option<f32>,
}

impl EdgeFilter {
    pub fn new() -> Self {
        EdgeFilter::default()
    }
    pub fn with_edge_type(mut self, t: crate::types::EdgeType) -> Self {
        self.edge_types.push(t);
        self
    }
    pub fn with_min_weight(mut self, w: f32) -> Self {
        self.min_weight = Some(w);
        self
    }
}

/// Result of a batch decay operation (PRD §9.2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DecayReport {
    /// How many edges the decay factor was applied to.
    pub edges_decayed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_update_applies_then_clamps() {
        assert_eq!(WeightUpdate::set(2.0).apply(1.0), 2.0);
        assert_eq!(WeightUpdate::add(0.5).apply(1.0), 1.5);
        assert_eq!(WeightUpdate::multiply(0.9).apply(2.0), 1.8);
        // Clamp is applied to the op result, not the input.
        let clamped = WeightUpdate::add(10.0).with_bounds(0.0, 1.0).apply(0.5);
        assert_eq!(clamped, 1.0);
    }
}
