//! `bench` — replay a materialized workload against a fixture and emit run JSON
//! (M0/M3).
//!
//! Usage: `bench <fixture_dir> <drey|naive> [workload_name]`
//!
//! `workload_name` selects which `workload.<name>.jsonl` to replay (default
//! `measurement`, the budget-gate plan; the four mix names are also available).
//! The plan is read from disk, never synthesized in process — a run is
//! reproducible from the fixture artifacts alone (spec §3.6).

use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};

use harness::driver::{DreyDriver, GraphDriver, NaiveDriver};
use harness::fixture::{read_fixture, read_workload};
use harness::output::{host_fingerprint, FixtureInfo, Resources, RunMeta, RunOutput};
use harness::runner;
use harness::workload::WARMUP;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <fixture_dir> <drey|naive> [workload_name]",
            args[0]
        );
        exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let driver_kind = args[2].as_str();
    let workload_name = args.get(3).map(|s| s.as_str()).unwrap_or("measurement");

    let (fixture, manifest, checksum_ok) = match read_fixture(&dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to read fixture: {e}");
            exit(1);
        }
    };
    if !checksum_ok {
        eprintln!("warning: fixture checksums did not verify");
    }

    // Replay the materialized plan; do not synthesize one (spec §3.6).
    let plan = match read_workload(&dir, workload_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to read workload {workload_name:?}: {e}");
            eprintln!("(run `generate` to materialize workload.*.jsonl next to the fixture)");
            exit(1);
        }
    };

    let mut driver: Box<dyn GraphDriver> = match driver_kind {
        "drey" => Box::new(DreyDriver::new()),
        "naive" => Box::new(NaiveDriver::new()),
        s => {
            eprintln!("unknown driver: {s}");
            exit(2);
        }
    };
    let is_real = driver_kind == "drey";

    // Capture the run start before any load/measurement work, so `started_at`
    // reflects the start rather than completion time.
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    eprintln!("loading fixture into {} driver…", driver.name());
    let load = match driver.load_fixture(&fixture) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load failed: {e}");
            exit(1);
        }
    };
    eprintln!(
        "loaded {} nodes, {} edges; running plan…",
        load.nodes, load.edges
    );

    // Warmup: discard the spec §5.2 prefix per bucket. The materialized plan is
    // sized (by `generate`) so each budgeted bucket still retains its floor.
    let results = match runner::run(driver.as_mut(), &plan, WARMUP, is_real) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("run failed: {e}");
            exit(1);
        }
    };

    // Sum the payload file sizes, but never silently drop a file: a metadata
    // failure would undercount raw_payload_bytes invisibly, so warn per file.
    let mut raw_payload: u64 = 0;
    for f in manifest.checksums.keys() {
        match std::fs::metadata(dir.join(f)) {
            Ok(m) => raw_payload += m.len(),
            Err(e) => eprintln!("warning: raw_payload skips {f}: {e}"),
        }
    }

    let output = RunOutput {
        harness_version: env!("CARGO_PKG_VERSION").into(),
        run: RunMeta {
            driver: driver.name(),
            mix: workload_name.to_string(),
            ops_total: plan.len() as u64,
            started_at,
        },
        fixture: FixtureInfo {
            manifest,
            checksum_verified: checksum_ok,
        },
        host: host_fingerprint(),
        results,
        resources: Resources {
            resident_bytes: None,
            raw_payload_bytes: Some(raw_payload),
            link_size_bytes: None,
            daemon_or_listener: false, // verified by inspection: no daemon exists
        },
    };

    println!("{}", harness::canonical::line(&output).trim_end());

    // Exit non-zero if any real-driver row failed its budget, so the gate is
    // scriptable in CI.
    let failed = output.results.iter().any(|r| r.pass == Some(false));
    if failed {
        eprintln!("budget gate: at least one bucket failed");
        exit(3);
    }
}
