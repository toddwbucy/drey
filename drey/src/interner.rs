//! Type-label interning (PRD §9.2).
//!
//! Caller-facing node/edge types are strings; internally they are interned to
//! compact `u32` ids for adjacency indexing, filtering, and export. These
//! internal ids are not public API and are never exposed
//! (stable-contracts-replaceable-internals, PRD §6.6).
//!
//! Interning is insertion-ordered and durable: an id, once assigned to a label,
//! keeps that label for the life of the graph and across reload, because
//! adjacency indexes and export type-id arrays are keyed on it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A bidirectional string↔`u32` interner.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Interner {
    labels: Vec<String>,
    #[serde(skip)]
    index: HashMap<String, u32>,
}

impl Interner {
    /// Intern a label, returning its id (assigning a fresh one if new).
    pub fn intern(&mut self, label: &str) -> u32 {
        if let Some(&id) = self.index.get(label) {
            return id;
        }
        let id = self.labels.len() as u32;
        self.labels.push(label.to_string());
        self.index.insert(label.to_string(), id);
        id
    }

    /// Look up an existing id without assigning.
    pub fn get(&self, label: &str) -> Option<u32> {
        self.index.get(label).copied()
    }

    /// Resolve an id back to its label.
    pub fn label(&self, id: u32) -> Option<&str> {
        self.labels.get(id as usize).map(|s| s.as_str())
    }

    /// The interned labels in id order — the persisted source of truth.
    pub(crate) fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Reconstruct an interner from its persisted label vector, rebuilding the
    /// lookup index. Ids are the vector positions, so they match what was saved.
    pub(crate) fn from_labels(labels: Vec<String>) -> Self {
        let mut it = Interner {
            labels,
            index: std::collections::HashMap::new(),
        };
        it.rebuild_index();
        it
    }

    /// Rebuild the `index` side after deserialization (the `labels` vector is
    /// the persisted source of truth; the map is derived).
    pub fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, label) in self.labels.iter().enumerate() {
            self.index.insert(label.clone(), i as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable_and_bidirectional() {
        let mut it = Interner::default();
        let a = it.intern("alpha");
        let b = it.intern("beta");
        assert_eq!(it.intern("alpha"), a); // stable
        assert_ne!(a, b);
        assert_eq!(it.label(a), Some("alpha"));
        assert_eq!(it.get("beta"), Some(b));
        assert_eq!(it.get("missing"), None);
    }

    #[test]
    fn rebuild_index_restores_lookups() {
        let mut it = Interner::default();
        it.intern("x");
        it.intern("y");
        it.index.clear(); // simulate a fresh deserialize (index is #[serde(skip)])
        it.rebuild_index();
        assert_eq!(it.get("y"), Some(1));
    }
}
