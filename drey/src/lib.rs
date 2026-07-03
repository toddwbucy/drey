//! `drey` — an embedded, memory-primary property-graph substrate crate.
//!
//! A SQLite-class library (linked, local file, no daemon, no listener) that is
//! memory-primary where SQLite is disk-primary: the working graph lives in RAM,
//! disk provides durability, and the two reconcile through an explicit
//! `commit`. It offers typed nodes and directed weighted edges, property lookup
//! with equality/range scalar indexes, bounded traversal, shortest path,
//! exhaustive-scan vector similarity composable with structural filters, and a
//! feature-export trait for external structural-embedding pipelines.
//!
//! The crate is a primitive, not a reasoning layer (PRD §1, §6.1): it stores,
//! indexes, persists, mutates, traverses, and searches. What nodes and edges
//! mean, when weights change, and what similarity signifies are consumer
//! policy.
//!
//! # Layout
//! - [`types`] — the data model (PRD §7, §9.1).
//! - [`error`] — the typed error model (PRD §19).
//! - [`config`] — [`GraphConfig`], construction and operating policy (PRD §9.1).
//! - [`mutation`] — patch / weight-update / removal-mode types (PRD §9.2).
//! - [`query`] — property queries and lookups (PRD §9.3).
//! - [`traverse`] — neighbors, bounded traversal, shortest path (PRD §9.3).
//! - [`similarity`] — vector similarity composed with filters (PRD §9.4).
//! - [`export`] — graph-feature export for GNN pipelines (PRD §9.5, §14).
//! - [`read`] — the read-adapter seam for a query layer (PRD §12).

mod graph;
mod interner;
mod persist;
mod store;

pub mod config;
pub mod error;
pub mod export;
pub mod mutation;
pub mod query;
pub mod read;
pub mod similarity;
pub mod traverse;
pub mod types;

pub use config::GraphConfig;
pub use error::{Error, Result};
pub use graph::Graph;
pub use types::{
    Direction, Edge, EdgeId, EdgeType, Embedding, Node, NodeId, NodeType, Properties, Scalar, Value,
};
