//! `bench` — run a measurement plan against a fixture and emit run JSON (M0/M3).
//!
//! Usage: `bench <fixture_dir> <drey|naive> [per_bucket]`

use std::process::exit;

use harness::driver::{DreyDriver, GraphDriver, NaiveDriver};
use harness::fixture::read_fixture;
use harness::output::{host_fingerprint, RunMeta, RunOutput, Resources};
use harness::runner;
use harness::workload::measurement_plan;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <fixture_dir> <drey|naive> [per_bucket]", args[0]);
        exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let driver_kind = args[2].as_str();
    // Default 1000, but a present-yet-unparseable value is a user error, not a
    // silent fallback.
    let per_bucket: usize = match args.get(3) {
        None => 1000,
        Some(s) => match s.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("per_bucket must be a non-negative integer, got {s:?}");
                exit(2);
            }
        },
    };

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

    let mut driver: Box<dyn GraphDriver> = match driver_kind {
        "drey" => Box::new(DreyDriver::new()),
        "naive" => Box::new(NaiveDriver::new()),
        s => {
            eprintln!("unknown driver: {s}");
            exit(2);
        }
    };
    let is_real = driver_kind == "drey";

    eprintln!("loading fixture into {} driver…", driver.name());
    let load = match driver.load_fixture(&fixture) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load failed: {e}");
            exit(1);
        }
    };
    eprintln!("loaded {} nodes, {} edges; running plan…", load.nodes, load.edges);

    let plan = measurement_plan(&fixture, per_bucket);
    // Warmup: discard up to 100 per bucket (spec §5.2), but never more than
    // half a small plan so short runs still retain samples.
    let warmup = 100.min(per_bucket / 2);
    let results = match runner::run(driver.as_mut(), &plan, warmup, is_real) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("run failed: {e}");
            exit(1);
        }
    };

    let raw_payload = manifest
        .checksums
        .keys()
        .filter_map(|f| std::fs::metadata(dir.join(f)).ok().map(|m| m.len()))
        .sum();

    let output = RunOutput {
        harness_version: env!("CARGO_PKG_VERSION").into(),
        run: RunMeta {
            driver: driver.name(),
            fixture_size: format!("{:?}", manifest.parameters.size_class).to_lowercase(),
            fixture_source: format!("{:?}", manifest.source).to_lowercase(),
            checksum_verified: checksum_ok,
            ops_total: plan.len() as u64,
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
