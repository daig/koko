use assert_cmd::Command;
use predicates::prelude::*;
#[cfg(unix)]
use std::io::{BufRead, Read};
#[cfg(unix)]
use std::process::Stdio;

#[test]
fn help_and_version_are_successful_process_exits() {
    Command::cargo_bin("koko")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--command <CYPHER>"))
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("koko")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("Koko engine"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn process_usage_failures_exit_two_without_activation() {
    Command::cargo_bin("koko")
        .unwrap()
        .arg("--unknown")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("unexpected argument '--unknown'"));

    Command::cargo_bin("koko")
        .unwrap()
        .args(["--command", "RETURN 1", "--file", "missing.cypher"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));

    Command::cargo_bin("koko")
        .unwrap()
        .arg("native.db")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unexpected argument 'native.db'"));
}

#[test]
fn process_config_error_reports_provenance_before_activation() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg");
    let xdg_config = xdg.join("koko/config.toml");
    std::fs::create_dir_all(xdg_config.parent().unwrap()).unwrap();
    std::fs::write(&xdg_config, "format = \"unknown\"\n").unwrap();

    let mac_config = root
        .path()
        .join("Library/Application Support/Koko/config.toml");
    std::fs::create_dir_all(mac_config.parent().unwrap()).unwrap();
    std::fs::write(&mac_config, "format = \"unknown\"\n").unwrap();

    Command::cargo_bin("koko")
        .unwrap()
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("configuration key `format`")
                .and(predicate::str::contains("auto, box, table")),
        );
}

#[cfg(unix)]
#[test]
fn batch_sigint_exits_130_and_closes_machine_protocol() {
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("koko"))
        .args([
            "--no-config",
            "--command",
            "BEGIN TRANSACTION; UNWIND range(0, 100000000) AS value RETURN sum(value); RETURN 42 AS should_not_run",
            "--format",
            "json",
            "--progress",
            "on",
            "--keep-going",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut progress = String::new();
    stderr.read_line(&mut progress).unwrap();
    assert!(progress.contains("Running…"), "{progress:?}");
    let status = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = child.wait().unwrap();
    assert_eq!(exit.code(), Some(130));

    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(document["complete"], serde_json::json!(false));
    assert_eq!(
        document["error"]["error"]["kind"],
        serde_json::json!("interrupt")
    );
    assert!(!String::from_utf8_lossy(&stdout).contains("should_not_run"));
    let mut stderr_tail = String::new();
    stderr.read_to_string(&mut stderr_tail).unwrap();
    let stderr = progress + &stderr_tail;
    assert!(stderr.contains("Cancelling…"));
    assert!(stderr.contains("Query cancelled after"));
    assert!(!stderr.contains("rows returned"));
}

#[test]
fn batch_deadline_remains_distinct_from_user_cancellation() {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("koko"))
        .args([
            "--no-config",
            "--command",
            "CALL timeout=1; UNWIND range(0, 1000000) AS value RETURN sum(value)",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["complete"], serde_json::json!(false));
    assert_eq!(
        document["error"]["error"]["kind"],
        serde_json::json!("deadline")
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Query deadline expired after"));
    assert!(!stderr.contains("Query cancelled after"));
}
