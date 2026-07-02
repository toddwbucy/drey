//! Deterministic randomness. The only RNG the harness is allowed to use.
//!
//! Spec §3.4 mandates a single seeded PRNG whose seed reproduces the fixture
//! byte-for-byte at a given generator version. That rules out `thread_rng` /
//! OS entropy anywhere in generation (checklist trap 5) — the type here is the
//! only sanctioned source, and it is constructible only from an explicit seed.
//!
//! `Xoshiro256++` seeded via `seed_from_u64` is exactly the spec's
//! "splitmix64-seeded xoshiro": `SeedableRng::seed_from_u64` fills the state
//! with SplitMix64 output.
//!
//! Reproducibility contract: byte-for-byte identical for a given
//! `(generator version, seed, parameters)` **on a fixed host/toolchain** — this
//! is what the manifest checksums assert and what the M0 tests verify. Most of
//! the generation path uses only IEEE-754 correctly-rounded operations (`+`,
//! `*`, `sqrt`) and so is also bit-identical across machines; the one exception
//! is the Zipf degree/edge-type CDF, which calls `f64::powf` (see
//! [`crate::generator`]). `powf` is not required by IEEE-754 to be
//! correctly-rounded, so a fixture *may* differ by a ULP across libm versions.
//! Cross-host bit-identity is therefore best-effort, not contractual; the
//! manifest records the host so any gap is visible rather than hidden.

use rand_xoshiro::rand_core::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

/// The harness's deterministic RNG. Alias so call sites never name a concrete
/// third-party type and the choice stays swappable.
pub type DetRng = Xoshiro256PlusPlus;

/// Generation is split into independent phases so that, e.g., changing how
/// embeddings are drawn does not shift the node or edge byte stream. Each phase
/// gets its own stream derived deterministically from the single fixture seed,
/// so one seed still reproduces the whole fixture (spec §3.4).
#[derive(Clone, Copy, Debug)]
pub enum Phase {
    Nodes = 1,
    Edges = 2,
    Embeddings = 3,
    Workload = 4,
}

/// SplitMix64 finalizer — mixes the seed with a phase tag so phase streams are
/// decorrelated. Deterministic and platform-independent (integer ops only).
fn mix(seed: u64, tag: u64) -> u64 {
    let mut z = seed
        .wrapping_add(tag.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// An RNG for one generation phase, derived from the fixture's single seed.
pub fn phase(seed: u64, phase: Phase) -> DetRng {
    DetRng::seed_from_u64(mix(seed, phase as u64))
}

/// A bare seeded RNG, for tests and one-off deterministic streams.
pub fn seeded(seed: u64) -> DetRng {
    DetRng::seed_from_u64(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    #[test]
    fn same_seed_same_stream() {
        let mut a = seeded(0xD00D);
        let mut b = seeded(0xD00D);
        for _ in 0..1000 {
            assert_eq!(a.gen::<u64>(), b.gen::<u64>());
        }
    }

    #[test]
    fn phases_are_decorrelated_but_deterministic() {
        // Distinct phases diverge...
        let mut nodes = phase(42, Phase::Nodes);
        let mut edges = phase(42, Phase::Edges);
        assert_ne!(nodes.gen::<u64>(), edges.gen::<u64>());
        // ...yet each phase is reproducible from the same fixture seed.
        let mut nodes_again = phase(42, Phase::Nodes);
        let mut n1 = phase(42, Phase::Nodes);
        assert_eq!(n1.gen::<u64>(), nodes_again.gen::<u64>());
    }
}
