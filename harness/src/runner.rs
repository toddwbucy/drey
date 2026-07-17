//! The measurement loop (spec §5.2).
//!
//! Single-threaded (matching drey's single-writer model). Per (op class ×
//! parameter class) bucket: discard a warmup prefix, retain the rest, and
//! compute p50/p95/p99 by nearest-rank. Counters are checked for
//! reproducibility by the caller; here we only measure timing and carry
//! representative counters through.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::driver::{GraphDriver, OpStatus};
use crate::output::{budget_for, ResultRow};
use crate::workload::WorkloadOp;

/// Per-bucket accumulated timings (microseconds) and status.
#[derive(Default)]
struct Bucket {
    times_us: Vec<f64>,
    status: Option<OpStatus>,
    last_counters: BTreeMap<String, u64>,
    /// Bucket-defining parameters (constant within a bucket), for the row.
    params: BTreeMap<String, String>,
}

/// Run a plan against a driver, returning one [`ResultRow`] per bucket. `warmup`
/// samples are discarded per bucket before retention. `is_real` gates pass/fail:
/// a throwaway driver's rows never pass or fail (spec §5.3).
pub fn run(
    driver: &mut dyn GraphDriver,
    plan: &[WorkloadOp],
    warmup: usize,
    is_real: bool,
) -> Result<Vec<ResultRow>, String> {
    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();

    for op in plan {
        let key = op.bucket();
        // Pre-timing setup (e.g. Similar seed-embedding resolution) is plan
        // material, not measured work — it runs before the clock starts.
        driver.prepare_op(op)?;
        let start = Instant::now();
        let outcome = driver.run_op(op)?;
        let elapsed_us = start.elapsed().as_nanos() as f64 / 1000.0;

        let b = buckets.entry(key).or_default();
        b.status = Some(outcome.status);
        if b.params.is_empty() {
            b.params = op.params();
        }
        if outcome.status == OpStatus::Ok {
            b.times_us.push(elapsed_us);
            b.last_counters = outcome.counters;
        }
    }

    let mut rows = Vec::new();
    for (bucket, mut b) in buckets {
        let status_str = match b.status {
            Some(OpStatus::Ok) => "ok",
            Some(OpStatus::NotApplicable) => "n/a",
            Some(OpStatus::Error) => "error",
            None => "n/a",
        };

        // Discard the warmup prefix (spec §5.2 wants exactly 100 for a real
        // plan). Clamp to half a bucket only as a floor for tiny test plans;
        // real plans are sized by `measurement_samples` and guarded by
        // `plan_self_check`, so the clamp never bites them.
        let effective_warmup = warmup.min(b.times_us.len() / 2);
        let retained: Vec<f64> = b.times_us.split_off(effective_warmup);

        let budget = budget_for(&bucket);
        let (p50, p95, p99, throughput) = if retained.is_empty() {
            (None, None, None, None)
        } else {
            let mut sorted = retained.clone();
            sorted.sort_by(|a, z| a.partial_cmp(z).unwrap());
            let p50 = nearest_rank(&sorted, 0.50);
            let p95 = nearest_rank(&sorted, 0.95);
            let p99 = nearest_rank(&sorted, 0.99);
            // Sustained throughput: retained ops per total retained seconds.
            let total_s: f64 = sorted.iter().sum::<f64>() / 1_000_000.0;
            let tput = if total_s > 0.0 {
                sorted.len() as f64 / total_s
            } else {
                0.0
            };
            (Some(p50), Some(p95), Some(p99), Some(tput))
        };

        // pass: only for a real driver, only where a budget and measurement
        // exist; both halves of a dual budget must hold (spec §5.3).
        let pass = if is_real && status_str == "ok" {
            match (budget.p95_us, p95) {
                (Some(bp95), Some(m95)) => {
                    let latency_ok = m95 <= bp95;
                    let tput_ok = match (budget.throughput_per_s, throughput) {
                        (Some(bt), Some(mt)) => mt >= bt,
                        (Some(_), None) => false,
                        _ => true,
                    };
                    Some(latency_ok && tput_ok)
                }
                _ => None,
            }
        } else {
            None
        };

        rows.push(ResultRow {
            op: bucket,
            status: status_str.into(),
            params: std::mem::take(&mut b.params),
            samples: retained.len() as u64,
            p50_us: p50,
            p95_us: p95,
            p99_us: p99,
            throughput_per_s: throughput,
            budget_us: budget.p95_us,
            budget_throughput_per_s: budget.throughput_per_s,
            budget_source: "synthetic".into(),
            pass,
            counters: std::mem::take(&mut b.last_counters),
        });
    }
    Ok(rows)
}

/// Nearest-rank percentile: the `ceil(p·n)`-th smallest (1-indexed) retained
/// sample. `sorted` must be ascending and non-empty.
fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = (p * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_spec_formula() {
        let s: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(nearest_rank(&s, 0.50), 50.0); // ceil(50)=50 → index 49
        assert_eq!(nearest_rank(&s, 0.95), 95.0);
        assert_eq!(nearest_rank(&s, 0.99), 99.0);
        assert_eq!(nearest_rank(&s, 1.0), 100.0);
    }
}
