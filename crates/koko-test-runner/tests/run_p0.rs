//! Runs the bespoke P0 `.test` fixtures through the engine — the Phase 0
//! definition of done. Every case must pass (none may fail; skips are allowed
//! only for fixtures that need later-phase features).

use koko_test_runner::{Outcome, parse_test_file, run_test_file};
use std::path::PathBuf;

fn p0_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/p0")
}

/// Hermetic datasets bundled in the repo (so the suite needs no submodule).
fn datasets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/datasets")
}

#[test]
fn p0_corpus_passes() {
    let dir = p0_dir();
    let datasets = datasets_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("test"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no .test fixtures found in {}",
        dir.display()
    );

    let mut failures = Vec::new();
    let mut total = 0;
    for path in &files {
        let content = std::fs::read_to_string(path).unwrap();
        let file = parse_test_file(&content)
            .unwrap_or_else(|e| panic!("parse error in {}: {e}", path.display()));
        let group = path.file_stem().unwrap().to_str().unwrap();
        for result in run_test_file(&file, Some(&datasets)) {
            total += 1;
            if let Outcome::Fail(msg) = &result.outcome {
                failures.push(format!("{group}.{}: {msg}", result.name));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} P0 cases failed:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
}
