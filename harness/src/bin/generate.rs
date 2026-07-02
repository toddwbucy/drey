//! `generate` — write a synthetic fixture to a directory (spec §3, M0).
//!
//! Usage: `generate <small|representative|stress> <low|medium|high> <seed> <out_dir>`

use std::process::exit;

use harness::fixture::{self, Source};
use harness::generator::generate;
use harness::params::{Fanout, Parameters, SizeClass};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: {} <small|representative|stress> <low|medium|high> <seed> <out_dir>", args[0]);
        exit(2);
    }
    let size = match args[1].as_str() {
        "small" => SizeClass::Small,
        "representative" => SizeClass::Representative,
        "stress" => SizeClass::Stress,
        s => {
            eprintln!("bad size class: {s}");
            exit(2);
        }
    };
    let fanout = match args[2].as_str() {
        "low" => Fanout::Low,
        "medium" => Fanout::Medium,
        "high" => Fanout::High,
        s => {
            eprintln!("bad fanout: {s}");
            exit(2);
        }
    };
    let seed: u64 = args[3].parse().unwrap_or_else(|_| {
        eprintln!("seed must be a u64");
        exit(2);
    });
    let out_dir = std::path::PathBuf::from(&args[4]);

    let params = Parameters::new(size, fanout, seed);
    eprintln!(
        "generating {} nodes, {} edges (fanout {:?}), seed {seed}…",
        params.nodes, params.edges, fanout
    );
    let fixture = generate(params);
    match fixture::write_fixture(&out_dir, &fixture, Source::Synthetic) {
        Ok(manifest) => {
            println!(
                "wrote fixture to {}: {} nodes, {} edges, {} embeddings",
                out_dir.display(),
                manifest.counts.nodes,
                manifest.counts.edges,
                manifest.counts.embeddings
            );
        }
        Err(e) => {
            eprintln!("generation failed self-check: {e}");
            exit(1);
        }
    }
}
