//! `koko-test` — run `.test` corpus files against the Rust engine.
//!
//! Usage: `koko-test <file-or-directory> [...]`. Exits non-zero if any case
//! fails. Skipped cases (datasets that aren't available, `-SKIP`) do not fail.
//! Set `KOKO_DATASET_DIR` to the directory holding `-DATASET CSV <name>`
//! datasets (e.g. `.../koko/dataset`) to run dataset-backed cases.

use koko_test_runner::{CorpusEnv, Outcome, parse_test_file, run_test_file_with};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: koko-test <file-or-directory> [...]");
        return ExitCode::FAILURE;
    }

    let dataset_dir = std::env::var_os("KOKO_DATASET_DIR").map(PathBuf::from);
    let dataset_root = dataset_dir.as_deref();

    let mut files = Vec::new();
    for arg in &args {
        collect_test_files(Path::new(arg), &mut files);
    }
    files.sort();

    let (mut passed, mut skipped, mut failed) = (0u32, 0u32, 0u32);
    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error reading {}: {e}", path.display());
                failed += 1;
                continue;
            }
        };
        let file = match parse_test_file(&content) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("parse error in {}: {e}", path.display());
                failed += 1;
                continue;
            }
        };
        let group = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        // `<FILE>:name` results resolve against the corpus `answers/` dir, and
        // `${KOKO_ROOT_DIRECTORY}` in statement paths against the repo root.
        let answers_dir = answers_dir_for(path);
        let root = root_dir_for(path, dataset_root);
        let env = CorpusEnv {
            answers_dir: answers_dir.as_deref(),
            root: root.as_deref(),
        };
        for result in run_test_file_with(&file, dataset_root, env) {
            match result.outcome {
                Outcome::Pass => {
                    passed += 1;
                    println!("PASS  {group}.{}", result.name);
                }
                Outcome::Skip(reason) => {
                    skipped += 1;
                    println!("SKIP  {group}.{} ({reason})", result.name);
                }
                Outcome::Fail(msg) => {
                    failed += 1;
                    println!("FAIL  {group}.{} — {msg}", result.name);
                }
            }
        }
    }

    println!("\n{passed} passed, {skipped} skipped, {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn collect_test_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                collect_test_files(&entry.path(), out);
            }
        }
    } else if path.extension().and_then(|e| e.to_str()) == Some("test") {
        out.push(path.to_path_buf());
    }
}

/// Find the corpus `answers/` directory for a `.test` file: `KOKO_ANSWERS_DIR` if
/// set, else the nearest ancestor with an `answers` subdir (so `test/answers/` is
/// found from any `test/test_files/...` path).
fn answers_dir_for(test_path: &Path) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("KOKO_ANSWERS_DIR") {
        return Some(PathBuf::from(d));
    }
    let mut dir = test_path.parent();
    while let Some(d) = dir {
        let candidate = d.join("answers");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Resolve `${KOKO_ROOT_DIRECTORY}` (the repo root that holds `dataset/`): the
/// `KOKO_ROOT_DIRECTORY` env var, else the parent of the dataset registry
/// (`KOKO_DATASET_DIR` points at `<root>/dataset`), else the nearest ancestor of
/// the `.test` file containing a `dataset/` subdir. Canonicalized to absolute so
/// the substituted `COPY`/`LOAD` paths resolve regardless of the process CWD.
fn root_dir_for(test_path: &Path, dataset_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("KOKO_ROOT_DIRECTORY") {
        return Some(PathBuf::from(d));
    }
    if let Some(root) = dataset_root.and_then(|d| d.parent()) {
        if let Ok(abs) = root.canonicalize() {
            return Some(abs);
        }
    }
    let mut dir = test_path.parent();
    while let Some(d) = dir {
        if d.join("dataset").is_dir() {
            return d.canonicalize().ok();
        }
        dir = d.parent();
    }
    None
}
