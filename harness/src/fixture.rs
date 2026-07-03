//! Fixture manifest, on-disk format, and generation self-checks (spec §3.5–§3.7).
//!
//! A fixture directory holds `manifest.json`, `nodes.jsonl`, `edges.jsonl`, and
//! `embeddings.bin`. The manifest records full provenance (source, generator
//! version, seed, every parameter, counts, per-file checksums) so a synthetic
//! and a future captured fixture stay distinguishable. Generation runs a
//! self-check (spec §3.7) before writing: a fixture that fails is never
//! written.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical;
use crate::generator::Fixture;
use crate::params::Parameters;

/// Fixture provenance (spec §3.1). Per-artifact, so mixed runs stay honest.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Synthetic,
    Captured,
}

/// Record counts, for quick manifest inspection.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Counts {
    pub nodes: u64,
    pub edges: u64,
    pub embeddings: u64,
}

/// The fixture manifest (spec §3.5). `checksums` is a `BTreeMap` so it
/// serializes in sorted key order (canonical bytes).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub source: Source,
    pub parameters: Parameters,
    pub counts: Counts,
    pub checksums: BTreeMap<String, String>,
}

const NODES: &str = "nodes.jsonl";
const EDGES: &str = "edges.jsonl";
const EMBEDDINGS: &str = "embeddings.bin";
const MANIFEST: &str = "manifest.json";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{:x}", h.finalize())
}

/// Serialize embeddings to the binary sidecar layout (spec §3.5):
/// repeated `(node_id: u64 LE, dim: u32 LE, dim × f32 LE)`, hand-written so the
/// bytes are exactly the spec's layout with no framing.
fn encode_embeddings(embeddings: &[(u64, Vec<f32>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (id, v) in embeddings {
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }
    buf
}

/// Decode the embedding sidecar back to `(node_id, vector)` records.
pub fn decode_embeddings(bytes: &[u8]) -> Result<Vec<(u64, Vec<f32>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        if pos + 12 > bytes.len() {
            return Err("truncated embedding header".into());
        }
        let id = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        let dim = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap()) as usize;
        pos += 12;
        if pos + dim * 4 > bytes.len() {
            return Err("truncated embedding body".into());
        }
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            v.push(f32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()));
            pos += 4;
        }
        out.push((id, v));
    }
    Ok(out)
}

/// Write a fixture to `dir`, running the self-check first (spec §3.7). Returns
/// the manifest actually written.
pub fn write_fixture(dir: &Path, fixture: &Fixture, source: Source) -> Result<Manifest, String> {
    self_check(fixture)?;

    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let nodes_bytes = canonical::jsonl(fixture.nodes.iter()).into_bytes();
    let edges_bytes = canonical::jsonl(fixture.edges.iter()).into_bytes();
    let emb_bytes = encode_embeddings(&fixture.embeddings);

    write_all(dir.join(NODES), &nodes_bytes)?;
    write_all(dir.join(EDGES), &edges_bytes)?;
    write_all(dir.join(EMBEDDINGS), &emb_bytes)?;

    let mut checksums = BTreeMap::new();
    checksums.insert(NODES.to_string(), sha256_hex(&nodes_bytes));
    checksums.insert(EDGES.to_string(), sha256_hex(&edges_bytes));
    checksums.insert(EMBEDDINGS.to_string(), sha256_hex(&emb_bytes));

    let manifest = Manifest {
        source,
        parameters: fixture.params.clone(),
        counts: Counts {
            nodes: fixture.nodes.len() as u64,
            edges: fixture.edges.len() as u64,
            embeddings: fixture.embeddings.len() as u64,
        },
        checksums,
    };
    write_all(dir.join(MANIFEST), canonical::line(&manifest).as_bytes())?;
    Ok(manifest)
}

fn write_all(path: std::path::PathBuf, bytes: &[u8]) -> Result<(), String> {
    let mut f = fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

/// Load a manifest and verify every data-file checksum (spec §5.3
/// `checksum_verified`).
pub fn load_and_verify(dir: &Path) -> Result<(Manifest, bool), String> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(dir.join(MANIFEST)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let mut ok = true;
    for (file, expected) in &manifest.checksums {
        let bytes = fs::read(dir.join(file)).map_err(|e| e.to_string())?;
        if &sha256_hex(&bytes) != expected {
            ok = false;
        }
    }
    Ok((manifest, ok))
}

/// Read a fixture from disk into memory (spec §3.5 reader side), verifying
/// checksums. Returns the fixture, the manifest, and whether all checksums
/// matched.
pub fn read_fixture(dir: &Path) -> Result<(Fixture, Manifest, bool), String> {
    let (manifest, checksum_ok) = load_and_verify(dir)?;

    let nodes: Vec<crate::generator::FixtureNode> =
        parse_jsonl(&dir.join(NODES)).map_err(|e| format!("nodes.jsonl: {e}"))?;
    let edges: Vec<crate::generator::FixtureEdge> =
        parse_jsonl(&dir.join(EDGES)).map_err(|e| format!("edges.jsonl: {e}"))?;
    let embeddings =
        decode_embeddings(&fs::read(dir.join(EMBEDDINGS)).map_err(|e| e.to_string())?)?;

    let fixture = Fixture {
        params: manifest.parameters.clone(),
        nodes,
        edges,
        embeddings,
    };
    Ok((fixture, manifest, checksum_ok))
}

/// The on-disk name for a materialized workload plan (spec §3.6): the mix name
/// (`mixed`, …) or `measurement` for the budget-gate plan.
pub fn workload_filename(name: &str) -> String {
    format!("workload.{name}.jsonl")
}

/// Materialize a workload plan next to the fixture as `workload.<name>.jsonl`,
/// one op per line in execution order (spec §3.6 — the workload is data, not
/// code, so a run is reproducible from artifacts alone).
pub fn write_workload(
    dir: &Path,
    name: &str,
    plan: &[crate::workload::WorkloadOp],
) -> Result<(), String> {
    let bytes = canonical::jsonl(plan.iter()).into_bytes();
    write_all(dir.join(workload_filename(name)), &bytes)
}

/// Read a materialized workload plan back (spec §3.6). `bench` runs exactly this
/// sequence rather than synthesizing a plan in process.
pub fn read_workload(dir: &Path, name: &str) -> Result<Vec<crate::workload::WorkloadOp>, String> {
    let file = workload_filename(name);
    parse_jsonl(&dir.join(&file)).map_err(|e| format!("{file}: {e}"))
}

fn parse_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?);
    }
    Ok(out)
}

/// Generation self-check (spec §3.7). Fails generation before anything is
/// written, so distribution/count violations surface here, not as a wrong
/// number downstream.
pub fn self_check(fixture: &Fixture) -> Result<(), String> {
    let p = &fixture.params;
    p.verify()?;

    // Exact edge count.
    if fixture.edges.len() as u64 != p.edges {
        return Err(format!(
            "edge count {} != derived {}",
            fixture.edges.len(),
            p.edges
        ));
    }

    // Exact node count.
    if fixture.nodes.len() as u64 != p.nodes {
        return Err(format!("node count {} != {}", fixture.nodes.len(), p.nodes));
    }

    // Mean out-degree is exactly edges/nodes by construction; assert it.
    let mean = fixture.edges.len() as f64 / fixture.nodes.len() as f64;
    let expected_mean = p.fanout.mean() as f64;
    if (mean - expected_mean).abs() > 1e-9 {
        return Err(format!("mean out-degree {mean} != {expected_mean}"));
    }

    // No node exceeds the declared max_degree — the manifest must not assert a
    // truncation bound the data violates (spec §3.2).
    let mut degree = vec![0u32; fixture.nodes.len()];
    for e in &fixture.edges {
        if let Some(d) = degree.get_mut(e.from as usize) {
            *d += 1;
        }
        if let Some(d) = degree.get_mut(e.to as usize) {
            *d += 1;
        }
    }
    if let Some(&max) = degree.iter().max() {
        if max as u64 > p.max_degree as u64 {
            return Err(format!(
                "max node degree {max} exceeds declared max_degree {}",
                p.max_degree
            ));
        }
    }

    // Embedding coverage within tolerance of the parameter.
    let cov = fixture.embeddings.len() as f64 / p.nodes as f64;
    if (cov - p.embed_coverage).abs() > 0.05 {
        return Err(format!(
            "embedding coverage {cov:.3} not within 0.05 of {}",
            p.embed_coverage
        ));
    }

    // p_cat selectivity bands within tolerance of 0.1% / 1% / 10% for nt_00.
    check_selectivity(fixture)?;

    // Reader/writer agree: a node and an edge round-trip through JSON exactly.
    round_trip_check(fixture)?;

    Ok(())
}

fn check_selectivity(fixture: &Fixture) -> Result<(), String> {
    let mut rare = 0u64;
    let mut uncommon = 0u64;
    let mut common = 0u64;
    let mut pop = 0u64;
    for n in &fixture.nodes {
        if n.node_type != "nt_00" {
            continue;
        }
        pop += 1;
        match n.props.get("p_cat").and_then(|v| v.as_str()) {
            Some("cat_rare") => rare += 1,
            Some("cat_uncommon") => uncommon += 1,
            Some("cat_common") => common += 1,
            _ => {}
        }
    }
    if pop == 0 {
        return Err("no nt_00 nodes".into());
    }
    let check = |name: &str, count: u64, target: f64| -> Result<(), String> {
        let freq = count as f64 / pop as f64;
        // Generous multiplicative tolerance — bands are ceil-rounded on small
        // populations, so exact fractions are not achievable; the point is the
        // classes are separated by ~10× as intended.
        if freq > target * 3.0 + 0.002 || freq < target / 3.0 {
            return Err(format!(
                "{name} selectivity {freq:.4} not near target {target}"
            ));
        }
        Ok(())
    };
    check("cat_rare", rare, 0.001)?;
    check("cat_uncommon", uncommon, 0.01)?;
    check("cat_common", common, 0.10)?;
    Ok(())
}

fn round_trip_check(fixture: &Fixture) -> Result<(), String> {
    if let Some(n) = fixture.nodes.first() {
        let line = canonical::line(n);
        let back: crate::generator::FixtureNode =
            serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())?;
        if canonical::line(&back) != line {
            return Err("node did not round-trip to identical canonical bytes".into());
        }
    }
    if let Some(e) = fixture.edges.first() {
        let line = canonical::line(e);
        let back: crate::generator::FixtureEdge =
            serde_json::from_str(line.trim_end()).map_err(|err| err.to_string())?;
        if canonical::line(&back) != line {
            return Err("edge did not round-trip to identical canonical bytes".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate;
    use crate::params::{Fanout, SizeClass};

    #[test]
    fn small_fixture_generates_and_self_checks() {
        let fx = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 7));
        assert!(self_check(&fx).is_ok(), "{:?}", self_check(&fx));
        assert_eq!(fx.edges.len(), 5_000);
        assert_eq!(fx.nodes.len(), 1_000);
    }

    #[test]
    fn generation_is_byte_for_byte_deterministic() {
        let a = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 7));
        let b = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 7));
        assert_eq!(
            canonical::jsonl(a.nodes.iter()),
            canonical::jsonl(b.nodes.iter())
        );
        assert_eq!(
            canonical::jsonl(a.edges.iter()),
            canonical::jsonl(b.edges.iter())
        );
        assert_eq!(
            encode_embeddings(&a.embeddings),
            encode_embeddings(&b.embeddings)
        );
    }

    #[test]
    fn embeddings_sidecar_round_trips() {
        let fx = generate(Parameters::new(SizeClass::Small, Fanout::Medium, 3));
        let bytes = encode_embeddings(&fx.embeddings);
        let back = decode_embeddings(&bytes).unwrap();
        assert_eq!(back.len(), fx.embeddings.len());
        // Byte-exact including hostile components.
        for ((ia, va), (ib, vb)) in fx.embeddings.iter().zip(&back) {
            assert_eq!(ia, ib);
            for (x, y) in va.iter().zip(vb) {
                assert_eq!(x.to_bits(), y.to_bits());
            }
        }
    }
}
