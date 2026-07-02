//! Fixture parameters (spec §3.2, §3.3).
//!
//! Two shapes an implementation is likely to get wrong, fixed here by
//! construction:
//!
//! - `edges` is **derived**, never independent: `nodes × fanout mean`
//!   (checklist trap 8). [`Parameters::new`] is the only constructor, and
//!   [`Parameters::verify`] re-checks the identity.
//! - Type cardinality comes from the **size class**, not the fanout class
//!   (Qwen tied node/edge types to fanout). Fanout determines only mean
//!   out-degree.
//!
//! Every parameter is serialized — none are `#[serde(skip)]` — so the manifest
//! preserves the full provenance the reproducibility contract needs
//! (checklist trap 4).

use serde::{Deserialize, Serialize};

/// The generator version. Bump whenever output for the same `(seed, params)`
/// would change (spec §3.4).
pub const GENERATOR_VERSION: u32 = 1;

/// Default truncated-Zipf exponent for out-degree (spec §3.2 `[spec decision]`).
pub const DEFAULT_DEGREE_S: f64 = 1.2;
/// Default out-degree truncation.
pub const DEFAULT_MAX_DEGREE: u32 = 1000;

/// The three size classes of spec §3.3. Node/edge type counts, embedding
/// dimensionality, and coverage are properties of the size class.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SizeClass {
    Small,
    Representative,
    Stress,
}

impl SizeClass {
    pub fn nodes(self) -> u64 {
        match self {
            SizeClass::Small => 1_000,
            SizeClass::Representative => 50_000,
            SizeClass::Stress => 500_000,
        }
    }

    pub fn node_types(self) -> u32 {
        match self {
            SizeClass::Small => 4,
            SizeClass::Representative => 12,
            SizeClass::Stress => 24,
        }
    }

    pub fn edge_types(self) -> u32 {
        match self {
            SizeClass::Small => 8,
            SizeClass::Representative => 24,
            SizeClass::Stress => 48,
        }
    }

    pub fn embed_dim(self) -> u32 {
        match self {
            SizeClass::Small => 256,
            SizeClass::Representative | SizeClass::Stress => 1_024,
        }
    }

    pub fn embed_coverage(self) -> f64 {
        0.5
    }
}

/// Fanout class — determines mean out-degree only (spec §3.3).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Fanout {
    Low,
    Medium,
    High,
}

impl Fanout {
    /// Integer mean out-degree, so `edges = nodes × mean` is exact with no
    /// rounding (matches the size-table counts: 1000×5=5000, 50000×5=250000…).
    pub fn mean(self) -> u64 {
        match self {
            Fanout::Low => 2,
            Fanout::Medium => 5,
            Fanout::High => 25,
        }
    }
}

/// A fully-resolved parameter set. `edges` is derived and stored so the
/// manifest is self-describing, but it is never set independently.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Parameters {
    pub generator_version: u32,
    pub size_class: SizeClass,
    pub fanout: Fanout,
    pub nodes: u64,
    /// Derived: `nodes × fanout.mean()`. Serialized for provenance.
    pub edges: u64,
    pub node_types: u32,
    pub edge_types: u32,
    pub embed_dim: u32,
    pub embed_coverage: f64,
    pub degree_s: f64,
    pub max_degree: u32,
    pub seed: u64,
}

impl Parameters {
    /// The only constructor. Derives `edges` and every size-class quantity, so
    /// an inconsistent parameter set cannot be built.
    pub fn new(size_class: SizeClass, fanout: Fanout, seed: u64) -> Self {
        let nodes = size_class.nodes();
        Parameters {
            generator_version: GENERATOR_VERSION,
            size_class,
            fanout,
            nodes,
            edges: nodes * fanout.mean(),
            node_types: size_class.node_types(),
            edge_types: size_class.edge_types(),
            embed_dim: size_class.embed_dim(),
            embed_coverage: size_class.embed_coverage(),
            degree_s: DEFAULT_DEGREE_S,
            max_degree: DEFAULT_MAX_DEGREE,
            seed,
        }
    }

    /// Re-assert the derived-edges identity. Called by the generator's
    /// self-check (spec §3.7) before anything is written to disk.
    pub fn verify(&self) -> Result<(), String> {
        let expected = self.nodes * self.fanout.mean();
        if self.edges != expected {
            return Err(format!(
                "edges must be derived: nodes({}) × fanout_mean({}) = {}, but parameters carry {}",
                self.nodes,
                self.fanout.mean(),
                expected,
                self.edges
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_derived_from_fanout() {
        assert_eq!(Parameters::new(SizeClass::Small, Fanout::Medium, 0).edges, 5_000);
        assert_eq!(
            Parameters::new(SizeClass::Representative, Fanout::Medium, 0).edges,
            250_000
        );
        assert_eq!(
            Parameters::new(SizeClass::Stress, Fanout::Medium, 0).edges,
            2_500_000
        );
        // Fanout sweep at representative (spec §3.3): 100k low, 1.25M high.
        assert_eq!(
            Parameters::new(SizeClass::Representative, Fanout::Low, 0).edges,
            100_000
        );
        assert_eq!(
            Parameters::new(SizeClass::Representative, Fanout::High, 0).edges,
            1_250_000
        );
    }

    #[test]
    fn type_cardinality_tracks_size_not_fanout() {
        let low = Parameters::new(SizeClass::Representative, Fanout::Low, 0);
        let high = Parameters::new(SizeClass::Representative, Fanout::High, 0);
        assert_eq!(low.node_types, high.node_types);
        assert_eq!(low.edge_types, high.edge_types);
    }

    #[test]
    fn verify_catches_tampered_edge_count() {
        let mut p = Parameters::new(SizeClass::Small, Fanout::Medium, 0);
        assert!(p.verify().is_ok());
        p.edges += 1;
        assert!(p.verify().is_err());
    }

    #[test]
    fn all_parameters_survive_serialization_round_trip() {
        let p = Parameters::new(SizeClass::Representative, Fanout::High, 0xABCD);
        let json = crate::canonical::line(&p);
        let back: Parameters = serde_json::from_str(json.trim_end()).unwrap();
        // Nothing dropped: degree_s and max_degree are present (trap 4).
        assert_eq!(back.degree_s, DEFAULT_DEGREE_S);
        assert_eq!(back.max_degree, DEFAULT_MAX_DEGREE);
        assert_eq!(back.edges, p.edges);
    }
}
