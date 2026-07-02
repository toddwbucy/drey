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
pub mod driver;
pub mod fixture;
pub mod generator;
pub mod output;
pub mod params;
pub mod rng;
pub mod runner;
pub mod workload;
