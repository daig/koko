//! Runs every first-party Cypher fixture as a fixed Koko product regression.
//! The manifest owns each fixture's contract, dataset, and case count. Every
//! case must execute and pass against data bundled in this repository.

use koko_test_runner::{CorpusEnv, Outcome, parse_test_file, run_test_file_with};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn product_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/product")
}

fn datasets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/datasets")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("koko-test-runner must live under <workspace>/crates")
        .to_path_buf()
}

fn fixture_manifest(dir: &Path) -> BTreeMap<String, (String, usize)> {
    let path = dir.join("manifest.tsv");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let mut lines = content.lines();
    assert_eq!(
        lines.next(),
        Some("fixture\tcategory\tdataset\tcases\tcontract"),
        "invalid product fixture manifest header"
    );

    let mut entries = BTreeMap::new();
    let mut previous_fixture = None;
    for (line_index, line) in lines.enumerate() {
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
        assert_eq!(
            fields.len(),
            5,
            "manifest line {} must have five tab-separated fields",
            line_index + 2
        );
        let [fixture, category, dataset, cases, contract] = fields.as_slice() else {
            unreachable!()
        };
        if let Some(previous) = previous_fixture {
            assert!(
                previous < *fixture,
                "manifest fixtures must be strictly sorted: {previous} before {fixture}"
            );
        }
        previous_fixture = Some(*fixture);
        assert!(
            fixture.ends_with(".test"),
            "invalid fixture name: {fixture}"
        );
        assert!(
            matches!(
                *category,
                "decisions"
                    | "execution"
                    | "language"
                    | "loading"
                    | "regressions"
                    | "schema-writes"
                    | "transactions"
                    | "types-functions"
            ),
            "invalid category for {fixture}: {category}"
        );
        assert!(
            matches!(*dataset, "empty" | "mini" | "tinysnb"),
            "invalid dataset for {fixture}: {dataset}"
        );
        assert!(
            !contract.trim().is_empty(),
            "missing contract for fixture {fixture}"
        );
        let cases = cases
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("invalid case count for {fixture}: {error}"));
        assert!(cases > 0, "fixture {fixture} must own at least one case");
        assert!(
            entries
                .insert((*fixture).to_string(), ((*dataset).to_string(), cases))
                .is_none(),
            "duplicate manifest entry for {fixture}"
        );
    }
    assert!(!entries.is_empty(), "product fixture manifest is empty");
    entries
}

#[test]
fn product_fixtures_match_their_manifest_and_pass() {
    let dir = product_dir();
    let datasets = datasets_dir();
    let manifest = fixture_manifest(&dir);
    let root = workspace_root();
    let corpus_env = CorpusEnv {
        answers_dir: None,
        root: Some(&root),
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.expect("cannot read product fixture entry").path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("test"))
        .collect();
    files.sort();

    let discovered: Vec<String> = files
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let listed: Vec<String> = manifest.keys().cloned().collect();
    assert_eq!(
        discovered, listed,
        "product fixture files and manifest entries differ"
    );

    let mut failures = Vec::new();
    let mut total = 0;
    for path in &files {
        let fixture = path.file_name().unwrap().to_str().unwrap();
        let (expected_dataset, expected_cases) = &manifest[fixture];
        let content = std::fs::read_to_string(path).unwrap();
        let file = parse_test_file(&content)
            .unwrap_or_else(|error| panic!("parse error in {}: {error}", path.display()));
        assert_eq!(
            &file.dataset, expected_dataset,
            "manifest dataset differs for {fixture}"
        );
        assert_eq!(
            file.cases.len(),
            *expected_cases,
            "manifest case count differs for {fixture}"
        );

        let group = path.file_stem().unwrap().to_str().unwrap();
        for result in run_test_file_with(&file, Some(&datasets), corpus_env) {
            total += 1;
            match &result.outcome {
                Outcome::Pass => {}
                Outcome::Skip(reason) => {
                    failures.push(format!("{group}.{} skipped: {reason}", result.name));
                }
                Outcome::Fail(message) => {
                    failures.push(format!("{group}.{}: {message}", result.name));
                }
            }
        }
    }

    let expected_total: usize = manifest.values().map(|(_, cases)| cases).sum();
    assert_eq!(total, expected_total, "not every manifested case ran");
    assert!(
        failures.is_empty(),
        "{} of {total} product cases did not pass:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
