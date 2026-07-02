//! Run output schema and the provisional budget table (spec §5.3, §4.4).
//!
//! One JSON document per run. Measurement and budget are one artifact: each
//! result row carries its budget, source, and `pass`, so the M3 gate is this
//! same document with `pass` populated.

use serde::{Deserialize, Serialize};

/// A budget for one op bucket. All numbers are synthetic until a capture
/// supersedes them (spec §4.4).
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub p95_us: Option<f64>,
    pub throughput_per_s: Option<f64>,
}

/// Provisional budget for a measurement bucket (spec §4.4). Buckets not in the
/// table have no budget and never pass/fail.
pub fn budget_for(bucket: &str) -> Budget {
    let p95 = |v: f64| Budget { p95_us: Some(v), throughput_per_s: None };
    match bucket {
        "neighbors" => p95(100.0),
        "traverse:max_hops=2" => p95(1_000.0),
        "traverse:max_hops=5" => p95(10_000.0),
        "shortest_path:hops" | "shortest_path:weighted" => p95(10_000.0),
        "property_eq" | "property_range" => p95(1_000.0),
        "similar_nodes" => p95(10_000.0),
        // Dual budget: ≤10µs p95 AND ≥100k updates/s sustained.
        "update_edge_weight" => Budget { p95_us: Some(10.0), throughput_per_s: Some(100_000.0) },
        "decay_edges:batch=100000" => p95(100_000.0),
        "decay_edges:batch=10000" => p95(10_000.0),
        "decay_edges:batch=1000" => p95(1_000.0),
        _ => Budget { p95_us: None, throughput_per_s: None },
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HostFingerprint {
    pub cpu: String,
    pub cores: u32,
    pub ram_gb: u32,
    pub os: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunMeta {
    pub driver: String,
    pub fixture_size: String,
    pub fixture_source: String,
    pub checksum_verified: bool,
    pub ops_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResultRow {
    pub op: String,
    /// `"ok" | "n/a" | "error"`.
    pub status: String,
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
    pub host: HostFingerprint,
    pub results: Vec<ResultRow>,
    pub resources: Resources,
}

/// Best-effort host fingerprint (spec §5.2): results are only comparable within
/// one fingerprint.
pub fn host_fingerprint() -> HostFingerprint {
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0);
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
    HostFingerprint { cpu, cores, ram_gb, os: std::env::consts::OS.to_string() }
}
