//! M0 fixture generator and measurement harness for `drey`.
//!
//! Throwaway apparatus (spec §2, §5.1): this crate exists to produce
//! reproducible synthetic fixtures and to measure a [`driver`]-backed workload
//! against provisional budgets. It is never a dependency of the `drey` crate,
//! and its file formats are test apparatus, not `drey`'s persistence format.
//!
//! The two invariants a trial implementation is most likely to break, and that
//! everything here is built around, are **byte-for-byte determinism** (one seed
//! plus a generator version reproduces a fixture exactly) and **canonical
//! bytes** (the writer and reader agree on the exact serialization). See
//! [`rng`] and [`canonical`].

pub mod canonical;
pub mod params;
pub mod rng;

// Landing in subsequent M0 increments, in dependency order:
//   pub mod manifest;    // manifest.json + checksums
//   pub mod generator;   // graph, properties, embeddings, self-checks (§3)
//   pub mod workload;    // deterministic plan generation (§4)
//   pub mod driver;      // GraphDriver trait, NaiveDriver, DreyDriver stub (§5.1)
//   pub mod runner;      // measurement loop, nearest-rank percentiles (§5.2)
//   pub mod output;      // run JSON schema (§5.3)
