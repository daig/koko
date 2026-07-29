use assert_cmd::Command;
use serde_json::Value as JsonValue;
use std::process::Stdio;

fn koko() -> Command {
    let mut command = Command::cargo_bin("koko").unwrap();
    command.arg("--no-config");
    command
}

fn json_output(arguments: &[&str]) -> (std::process::ExitStatus, JsonValue, String) {
    let output = koko().args(arguments).output().unwrap();
    let document = serde_json::from_slice(&output.stdout).unwrap();
    (
        output.status,
        document,
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn bat_01_command_file_init_pipe_and_empty_stdin_are_real_process_modes() {
    koko()
        .args(["--command", "RETURN 1 AS command", "--format", "csv"])
        .assert()
        .success()
        .stdout("command\n1\n");

    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("source.cypher");
    std::fs::write(&file, "RETURN 2 AS file;\n").unwrap();
    koko()
        .args(["--file", file.to_str().unwrap(), "--format", "csv"])
        .assert()
        .success()
        .stdout("file\n2\n");

    let init = root.path().join("init.cypher");
    std::fs::write(
        &init,
        "CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id)); CREATE (:Person {id: 7});",
    )
    .unwrap();
    koko()
        .args([
            "--init",
            init.to_str().unwrap(),
            "--command",
            "MATCH (p:Person) RETURN p.id AS initialized",
            "--format",
            "csv",
        ])
        .assert()
        .success()
        .stdout("initialized\n7\n");

    koko()
        .arg("--format")
        .arg("csv")
        .write_stdin("RETURN 3 AS piped;\n")
        .assert()
        .success()
        .stdout("piped\n3\n");
    koko()
        .arg("--format")
        .arg("json")
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"complete\":true"));
}

#[test]
fn bat_02_parameter_precedence_tags_duplicates_and_malformed_options() {
    let root = tempfile::tempdir().unwrap();
    let parameters = root.path().join("parameters.json");
    std::fs::write(
        &parameters,
        r#"{"name":"file","wide":{"$type":"INTEGER","logical_type":"INT128","value":"9007199254740992"}}"#,
    )
    .unwrap();
    let output = koko()
        .args([
            "--params-file",
            parameters.to_str().unwrap(),
            "--param",
            "name=\"inline\"",
            "--command",
            "RETURN $name AS name, $wide AS wide",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("inline"));
    assert!(stdout.contains("9007199254740992"));

    koko()
        .args(["--param", "x=1", "--param", "x=2", "--command", "RETURN $x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("duplicate command-line parameter"));
    koko()
        .args(["--param", "x=not-json", "--command", "RETURN $x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid JSON value"));
    koko()
        .args(["--command", "RETURN 1", "--file", "also.cypher"])
        .assert()
        .code(2);
}

#[test]
fn bat_03_machine_stdout_has_no_interactive_or_diagnostic_contamination() {
    let output = koko()
        .args(["--command", "RETURN 42 AS answer", "--format", "csv"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"answer\n42\n");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("koko["));
    assert!(!stdout.contains("Koko"));
    assert!(!stdout.contains("rows returned"));
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn bat_04_keep_going_stops_at_explicit_transaction_boundaries() {
    let output = koko()
        .args([
            "--command",
            "RETURN missing; RETURN 2 AS survived",
            "--keep-going",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"type\":\"error\""));
    assert!(stdout.contains("survived"));

    let output = koko()
        .args([
            "--command",
            "BEGIN TRANSACTION; RETURN missing; RETURN 2 AS forbidden",
            "--keep-going",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("forbidden")
    );
}

#[test]
fn bat_05_delimited_null_empty_escape_and_disclosure_contracts() {
    let output = koko()
        .args([
            "--command",
            "RETURN NULL AS null_value, '' AS empty_value, 'a,b' AS comma_value",
            "--format",
            "csv",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "null_value,empty_value,comma_value\n\\N,\"\",\"a,b\"\n"
    );

    let output = koko()
        .args([
            "--command",
            "RETURN 'a\tb\nc' AS escaped",
            "--format",
            "tsv",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("a\\tb\\nc")
    );

    koko()
        .args([
            "--command",
            "RETURN 1 AS first; RETURN 2 AS second",
            "--format",
            "csv",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("only one row-producing result"));
}

#[test]
fn bat_06_json_and_jsonl_success_and_failure_protocols_are_valid() {
    let (status, document, stderr) =
        json_output(&["--command", "RETURN 1 AS one", "--format", "json"]);
    assert!(status.success(), "{stderr}");
    assert_eq!(document["complete"], serde_json::json!(true));
    assert_eq!(document["results"][0]["rows"][0][0], serde_json::json!(1));

    let (status, document, _) = json_output(&["--command", "RETURN missing", "--format", "json"]);
    assert_eq!(status.code(), Some(1));
    assert_eq!(document["complete"], serde_json::json!(false));
    assert_eq!(
        document["error"]["error"]["kind"],
        serde_json::json!("binder")
    );

    let output = koko()
        .args([
            "--command",
            "RETURN 1 AS one; RETURN missing",
            "--keep-going",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let records = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(records.iter().any(|record| record["type"] == "schema"));
    assert!(records.iter().any(|record| record["type"] == "row"));
    assert!(records.iter().any(|record| record["type"] == "summary"));
    assert!(records.iter().any(|record| record["type"] == "error"));
}

#[test]
fn bat_07_every_required_format_and_trash_runs_the_real_binary() {
    for format in [
        "box", "table", "csv", "tsv", "json", "jsonl", "markdown", "line", "trash",
    ] {
        let output = koko()
            .args(["--command", "RETURN 7 AS value", "--format", format])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if format == "trash" {
            assert!(output.stdout.is_empty());
        } else {
            assert!(!output.stdout.is_empty(), "{format}");
        }
    }
}

#[test]
fn bat_08_output_collision_atomic_rollback_unicode_and_broken_pipe() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("résultat.tsv");
    std::fs::write(&destination, b"old\n").unwrap();
    koko()
        .args([
            "--command",
            "RETURN 1 AS value",
            "--format",
            "tsv",
            "--output",
            destination.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    assert_eq!(std::fs::read(&destination).unwrap(), b"old\n");

    koko()
        .args([
            "--command",
            "RETURN missing",
            "--format",
            "tsv",
            "--output",
            destination.to_str().unwrap(),
            "--force",
        ])
        .assert()
        .code(1);
    assert_eq!(std::fs::read(&destination).unwrap(), b"old\n");

    koko()
        .args([
            "--command",
            "RETURN 9 AS value",
            "--format",
            "tsv",
            "--output",
            destination.to_str().unwrap(),
            "--force",
        ])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "value\n9\n");

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("koko"))
        .args([
            "--no-config",
            "--command",
            "UNWIND range(1, 100000) AS value RETURN value",
            "--format",
            "jsonl",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn bat_09_active_transaction_eof_rolls_back_with_nonzero_status() {
    let output = koko()
        .args(["--command", "BEGIN TRANSACTION", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("rolling it back")
    );
    let document: JsonValue = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["complete"], serde_json::json!(false));
}

#[test]
fn bat_10_config_history_and_implicit_init_trust_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let roaming = home.join("AppData/Roaming");
    let local = home.join("AppData/Local");
    let config = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Koko/config.toml")
    } else if cfg!(target_os = "windows") {
        roaming.join("Koko/config.toml")
    } else {
        home.join("koko/config.toml")
    };
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "format = \"csv\"\nhistory = true\n").unwrap();
    std::fs::write(root.path().join("init.cypher"), "RETURN missing;").unwrap();

    Command::cargo_bin("koko")
        .unwrap()
        .current_dir(root.path())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_STATE_HOME", &home)
        .env("APPDATA", &roaming)
        .env("LOCALAPPDATA", &local)
        .args(["--command", "RETURN 5 AS configured"])
        .assert()
        .success()
        .stdout("configured\n5\n");

    Command::cargo_bin("koko")
        .unwrap()
        .current_dir(root.path())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_STATE_HOME", &home)
        .env("APPDATA", &roaming)
        .env("LOCALAPPDATA", &local)
        .args([
            "--no-config",
            "--no-history",
            "--command",
            "RETURN 6 AS isolated",
            "--format",
            "csv",
        ])
        .assert()
        .success()
        .stdout("isolated\n6\n");
    let history = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Koko/history")
    } else if cfg!(target_os = "windows") {
        local.join("Koko/history")
    } else {
        home.join("koko/history")
    };
    assert!(!history.exists());
}

#[test]
fn editor_continuation_markers_are_not_batch_syntax() {
    let source = "RETURN 1 \\\n+ 2";
    koko()
        .args(["--command", source])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Parser exception:"));

    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("continuation.cypher");
    std::fs::write(&file, source).unwrap();
    koko()
        .args(["--file", file.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Parser exception:"));

    koko()
        .write_stdin(source)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Parser exception:"));
}

use predicates::prelude::*;
