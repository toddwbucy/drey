//! Canonical serialization (spec §3.5).
//!
//! Byte-for-byte determinism requires that the same value always serialize to
//! the same bytes. The rules:
//!
//! - UTF-8, compact (`serde_json`'s default: no incidental whitespace).
//! - One complete JSON object per line, each terminated by a single `\n`.
//! - Files end with a trailing `\n`.
//! - Field order follows struct declaration order (`serde_json` preserves it);
//!   map-valued fields must use [`std::collections::BTreeMap`], which
//!   serializes in sorted key order — never `HashMap`, whose order is
//!   nondeterministic (checklist trap 5).
//! - Floats render as shortest round-trip decimal (`serde_json` uses Ryu).
//!
//! Every fixture data file and the run output go through these two functions,
//! and the manifest checksum is taken over exactly these bytes.

use std::io::{self, Write};

use serde::Serialize;

/// One canonical line: compact JSON plus a trailing `\n`.
pub fn line<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string(value).expect("canonical JSON serialization is infallible for fixture types");
    s.push('\n');
    s
}

/// JSONL for a sequence: one canonical [`line`] per item, in the order given.
/// Callers pass records already sorted by ascending id (spec §3.5).
pub fn jsonl<'a, T, I>(items: I) -> String
where
    T: Serialize + 'a,
    I: IntoIterator<Item = &'a T>,
{
    let mut out = String::new();
    for item in items {
        out.push_str(&line(item));
    }
    out
}

/// Stream JSONL to a writer without building the whole string in memory — for
/// the large fixtures (`stress` is millions of records).
pub fn write_jsonl<'a, T, I, W>(mut w: W, items: I) -> io::Result<()>
where
    T: Serialize + 'a,
    I: IntoIterator<Item = &'a T>,
    W: Write,
{
    for item in items {
        serde_json::to_writer(&mut w, item)?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::collections::BTreeMap;

    #[derive(Serialize)]
    struct Rec {
        id: u64,
        // Declaration order is the serialization order: `id` then `weight`.
        weight: f32,
    }

    #[test]
    fn line_is_compact_with_trailing_newline() {
        let r = Rec { id: 7, weight: 0.5 };
        assert_eq!(line(&r), "{\"id\":7,\"weight\":0.5}\n");
    }

    #[test]
    fn field_order_follows_declaration() {
        // `id` precedes `weight` because that is the struct's field order,
        // regardless of alphabetization.
        let r = Rec { id: 1, weight: 0.25 };
        let s = line(&r);
        assert!(s.find("\"id\"").unwrap() < s.find("\"weight\"").unwrap());
    }

    #[test]
    fn btreemap_serializes_in_sorted_key_order() {
        let mut m: BTreeMap<String, u32> = BTreeMap::new();
        m.insert("zeta".into(), 1);
        m.insert("alpha".into(), 2);
        m.insert("mu".into(), 3);
        assert_eq!(line(&m), "{\"alpha\":2,\"mu\":3,\"zeta\":1}\n");
    }

    #[test]
    fn float_shortest_round_trip() {
        // Ryu shortest round-trip: 0.1_f32 renders as "0.1", not a padded form.
        #[derive(Serialize)]
        struct F {
            x: f32,
        }
        assert_eq!(line(&F { x: 0.1 }), "{\"x\":0.1}\n");
    }
}
