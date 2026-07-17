//! Crate error model (PRD §19).
//!
//! All public APIs return typed errors, never panic on malformed input or
//! missing IDs (PRD §18). Errors carry enough context to debug without leaking
//! policy-layer assumptions.

use std::fmt;

use crate::types::{EdgeId, NodeId};

/// The crate-level result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Error categories per PRD §19.
#[derive(Debug)]
pub enum Error {
    /// A persistence/storage-layer failure (I/O, etc.).
    Storage(String),
    /// Encoding or decoding of the on-disk format failed.
    Codec(String),
    /// A referenced node was not found.
    NodeNotFound(NodeId),
    /// A referenced edge was not found.
    EdgeNotFound(EdgeId),
    /// A node type was used without being registered, or registered twice
    /// incompatibly.
    InvalidNodeType(String),
    /// An edge type was malformed.
    InvalidEdgeType(String),
    /// A property value violated a constraint (e.g. a nested value).
    InvalidPropertyValue(String),
    /// An embedding's dimensionality did not match the node type's declaration.
    DimensionMismatch { expected: usize, actual: usize },
    /// A node removal would leave dangling edges under the chosen mode.
    DanglingEdge(NodeId),
    /// An internal index was found inconsistent (a bug guard).
    IndexCorruption(String),
    /// A query used an unsupported shape.
    UnsupportedQuery(String),
    /// The persisted format version does not match this build.
    VersionMismatch { found: u32, supported: u32 },
    /// The WAL belongs to a newer snapshot generation than the snapshot on
    /// disk — the snapshot was replaced by an older copy (e.g. a backup
    /// restore) or the files were read mid-rotation. Replaying would blend
    /// mutations onto the wrong base image.
    GenerationMismatch { wal_epoch: u64, snapshot_epoch: u64 },
    /// A file lock could not be acquired (concurrent writer).
    LockConflict(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Storage(m) => write!(f, "storage error: {m}"),
            Error::Codec(m) => write!(f, "codec error: {m}"),
            Error::NodeNotFound(id) => write!(f, "node not found: {id:?}"),
            Error::EdgeNotFound(id) => write!(f, "edge not found: {id:?}"),
            Error::InvalidNodeType(m) => write!(f, "invalid node type: {m}"),
            Error::InvalidEdgeType(m) => write!(f, "invalid edge type: {m}"),
            Error::InvalidPropertyValue(m) => write!(f, "invalid property value: {m}"),
            Error::DimensionMismatch { expected, actual } => {
                write!(
                    f,
                    "embedding dimension mismatch: expected {expected}, got {actual}"
                )
            }
            Error::DanglingEdge(id) => {
                write!(f, "removing node {id:?} would orphan incident edges")
            }
            Error::IndexCorruption(m) => write!(f, "index corruption: {m}"),
            Error::UnsupportedQuery(m) => write!(f, "unsupported query: {m}"),
            Error::VersionMismatch { found, supported } => {
                write!(
                    f,
                    "format version mismatch: found {found}, this build supports {supported}"
                )
            }
            Error::GenerationMismatch {
                wal_epoch,
                snapshot_epoch,
            } => {
                write!(
                    f,
                    "generation mismatch: WAL epoch {wal_epoch} is newer than snapshot epoch \
                     {snapshot_epoch}; the snapshot is not the base this WAL was written against"
                )
            }
            Error::LockConflict(m) => write!(f, "lock conflict: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}
