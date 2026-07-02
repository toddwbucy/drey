//! Persistence (PRD §10). Stub at M1 — the type exists so the mutation API can
//! route through a `log` seam, but no durability is implemented until M2.
//!
//! M2 lands the real design: an in-memory graph plus a write-ahead log with
//! periodic snapshots (PRD §10.3 candidate 1, the leading candidate), a format
//! version, the commit durability level, and the §10.2.1 recovery matrix.

use crate::error::Result;
use crate::graph::Mutation;

/// The write-ahead persistence handle. Present only on file-backed graphs.
pub(crate) struct Persister {
    // Filled in at M2: log file handle, snapshot path, format version.
    _private: (),
}

impl Persister {
    /// Append a mutation to the log. No-op stub until M2.
    pub(crate) fn append(&mut self, _mutation: &Mutation) -> Result<()> {
        Ok(())
    }

    /// Flush the log to durable storage. No-op stub until M2.
    pub(crate) fn commit(&mut self) -> Result<()> {
        Ok(())
    }
}
