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

/// A bidirectional string↔`u32` interner.
///
/// Not `serde`-serialized: the interner is persisted by its `labels()` vector
/// through the hand-rolled binary codec (`persist`), and reconstructed with
/// [`Interner::from_labels`]. It carries no `Serialize`/`Deserialize` derive so
/// no dead codec machinery is generated for it (SQLite-class weight).
#[derive(Default, Clone, Debug)]
pub struct Interner {
    labels: Vec<String>,
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
    ///
    /// Duplicate labels are rejected: `intern` can never produce them, so a
    /// duplicate proves a malformed (checksum-valid but hand-altered or buggy)
    /// image. Accepting one would leave two live ids for one label with the
    /// lookup map resolving only the later — collapsing distinct type ids on
    /// every subsequent mutation.
    pub(crate) fn from_labels(labels: Vec<String>) -> crate::error::Result<Self> {
        let mut it = Interner {
            labels,
            index: std::collections::HashMap::new(),
        };
        for (i, label) in it.labels.iter().enumerate() {
            if it.index.insert(label.clone(), i as u32).is_some() {
                return Err(crate::error::Error::Codec(format!(
                    "duplicate interned type label {label:?} in persisted image"
                )));
            }
        }
        Ok(it)
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
    fn from_labels_restores_lookups_and_rejects_duplicates() {
        let it = Interner::from_labels(vec!["x".into(), "y".into()]).unwrap();
        assert_eq!(it.get("y"), Some(1));
        assert_eq!(it.label(0), Some("x"));
        // A duplicate label proves a malformed image: two ids would share one
        // label with lookups resolving only the later — reject at decode.
        assert!(Interner::from_labels(vec!["x".into(), "x".into()]).is_err());
    }
}
