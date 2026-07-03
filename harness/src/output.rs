//! Run output schema and the provisional budget table (spec §5.3, §4.4).
//!
//! One JSON document per run. Measurement and budget are one artifact: each
//! result row carries its budget, source, and `pass`, so the M3 gate is this
//! same document with `pass` populated. The full fixture manifest is embedded so
//! two sweep runs at the same size class (different fanout/dim) are never
//! indistinguishable (checklist trap 34).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fixture::Manifest;

/// A budget for one op bucket. All numbers are synthetic until a capture
/// supersedes them (spec §4.4).
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub p95_us: Option<f64>,
    pub throughput_per_s: Option<f64>,
}

/// Provisional budget for a measurement bucket (spec §4.4). Keyed off the
/// op-class prefix (before `:`), with the parameter class after it where the
/// budget varies by parameter (traverse hops, decay batch). Buckets not in the
/// table have no budget and never pass/fail.
pub fn budget_for(bucket: &str) -> Budget {
    let mut it = bucket.splitn(2, ':');
    let class = it.next().unwrap_or("");
    let param = it.next().unwrap_or("");
    let p95 = |v: f64| Budget {
        p95_us: Some(v),
        throughput_per_s: None,
    };
    let none = Budget {
        p95_us: None,
        throughput_per_s: None,
    };
    match class {
        "neighbors" => p95(100.0),
        "traverse" => match param {
            "max_hops=2" => p95(1_000.0),
            "max_hops=5" => p95(10_000.0),
            _ => none,
        },
        "shortest_path" => p95(10_000.0),
        "property_eq" | "property_range" => p95(1_000.0),
        "similar_nodes" => p95(10_000.0),
        // Dual budget: ≤10µs p95 AND ≥100k updates/s sustained.
        "update_edge_weight" => Budget {
            p95_us: Some(10.0),
            throughput_per_s: Some(100_000.0),
        },
        "decay_edges" => match param {
            "batch=1000" => p95(1_000.0),
            "batch=10000" => p95(10_000.0),
            "batch=100000" => p95(100_000.0),
            _ => none,
        },
        _ => none,
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostFingerprint {
    pub cpu: String,
    pub cores: u32,
    pub ram_gb: u32,
    pub os: String,
}

/// The fixture context for a run: the full manifest plus whether its checksums
/// verified (spec §5.3 `fixture: { manifest, checksum_verified }`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FixtureInfo {
    pub manifest: Manifest,
    pub checksum_verified: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunMeta {
    pub driver: String,
    /// The workload plan run: a mix name (`mixed`, …) or `measurement`.
    pub mix: String,
    pub ops_total: u64,
    /// Wall-clock start (seconds since the Unix epoch). Run context only; never
    /// on any deterministic path.
    pub started_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResultRow {
    pub op: String,
    /// `"ok" | "n/a" | "error"`.
    pub status: String,
    /// Bucket-defining parameters (spec §5.3 `params`), e.g. `{"max_hops":"2"}`.
    pub params: BTreeMap<String, String>,
    pub samples: u64,
    pub p50_us: Option<f64>,
    pub p95_us: Option<f64>,
    pub p99_us: Option<f64>,
    pub throughput_per_s: Option<f64>,
    pub budget_us: Option<f64>,
    pub budget_throughput_per_s: Option<f64>,
    pub budget_source: String,
    /// `null` until a real driver runs and a budget exists; the M3 gate.
    pub pass: Option<bool>,
    /// Representative op counters (e.g. `paths_returned`, `hits`, `candidates`),
    /// carried so the spec §5.4 exact-counter repeatability check can run off the
    /// emitted document (audit #5: they were computed but silently dropped).
    pub counters: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Resources {
    pub resident_bytes: Option<u64>,
    pub raw_payload_bytes: Option<u64>,
    pub link_size_bytes: Option<u64>,
    pub daemon_or_listener: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunOutput {
    pub harness_version: String,
    pub run: RunMeta,
    pub fixture: FixtureInfo,
    pub host: HostFingerprint,
    pub results: Vec<ResultRow>,
    pub resources: Resources,
}

/// Best-effort host fingerprint (spec §5.2): results are only comparable within
/// one fingerprint.
pub fn host_fingerprint() -> HostFingerprint {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(0);
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let ram_gb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
                .map(|kb| (kb / 1_048_576) as u32)
        })
        .unwrap_or(0);
    HostFingerprint {
        cpu,
        cores,
        ram_gb,
        os: std::env::consts::OS.to_string(),
    }
}
