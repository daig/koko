use assert_cmd::Command;
use koko_cli::command::{CommandError, MetaCommand, OutputCommand, OutputMode, parse_meta_command};
use predicates::prelude::*;
use serde_json::Value;

#[test]
fn typed_meta_parser_preserves_json_and_quoted_paths() {
    assert_eq!(
        parse_meta_command(":param answer {\"wide\": 9007199254740993}", true).unwrap(),
        MetaCommand::ParameterSet {
            name: "answer".to_string(),
            json: "{\"wide\": 9007199254740993}".to_string(),
        }
    );
    assert_eq!(
        parse_meta_command(":output 'result file.jsonl' append", true).unwrap(),
        MetaCommand::Output(OutputCommand::File {
            path: "result file.jsonl".into(),
            mode: OutputMode::Append,
        })
    );
    assert!(matches!(
        parse_meta_command(":history show 10", false),
        Err(CommandError::InteractiveOnly { .. })
    ));
}

#[test]
fn command_source_uses_one_ordered_connection_and_parameter_store() {
    let output = Command::cargo_bin("koko")
        .unwrap()
        .args([
            "--no-config",
            "--command",
            ":param answer 42\nRETURN $answer AS answer;\n:params --values\n:status",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record["values"] == serde_json::json!([42]))
    );
    assert!(records.iter().any(|record| {
        record["values"] == serde_json::json!(["answer", "INT64", "interactive", 42])
    }));
    assert!(
        records
            .iter()
            .any(|record| { record["values"] == serde_json::json!(["transaction", "none"]) })
    );
}

#[test]
fn relative_reads_preserve_order_and_cycles_show_the_chain() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child.cypher");
    let parent = root.path().join("parent.cypher");
    std::fs::write(
        &child,
        "CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id));\n",
    )
    .unwrap();
    std::fs::write(
        &parent,
        ":read child.cypher\nCREATE (:Person {id: 7});\nMATCH (p:Person) RETURN p.id AS id;\n",
    )
    .unwrap();
    Command::cargo_bin("koko")
        .unwrap()
        .args(["--no-config", "--file"])
        .arg(&parent)
        .args(["--format", "jsonl"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"values\":[7]"));

    let first = root.path().join("first.cypher");
    let second = root.path().join("second.cypher");
    std::fs::write(&first, ":read second.cypher\n").unwrap();
    std::fs::write(&second, ":read first.cypher\n").unwrap();
    Command::cargo_bin("koko")
        .unwrap()
        .args(["--no-config", "--file"])
        .arg(&first)
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("include cycle")
                .and(predicate::str::contains("first.cypher"))
                .and(predicate::str::contains("second.cypher")),
        );
}

#[test]
fn keep_going_continues_only_independent_autocommit_work() {
    Command::cargo_bin("koko")
        .unwrap()
        .args([
            "--no-config",
            "--command",
            "RETURN missing; RETURN 2 AS answer;",
            "--keep-going",
            "--format",
            "jsonl",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"values\":[2]"));

    Command::cargo_bin("koko")
        .unwrap()
        .args([
            "--no-config",
            "--command",
            "BEGIN TRANSACTION; RETURN missing; RETURN 2 AS answer;",
            "--keep-going",
            "--format",
            "jsonl",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"values\":[2]").not());
}

#[test]
fn failed_file_invocation_keeps_the_old_destination() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("result.json");
    std::fs::write(&output, b"old\n").unwrap();
    Command::cargo_bin("koko")
        .unwrap()
        .args([
            "--no-config",
            "--command",
            "RETURN missing",
            "--format",
            "json",
            "--output",
        ])
        .arg(&output)
        .arg("--force")
        .assert()
        .code(1);
    assert_eq!(std::fs::read(&output).unwrap(), b"old\n");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn initialization_runs_before_main_output_and_cli_settings_reapply() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.cypher");
    std::fs::write(
        &init,
        ":format csv\nCREATE NODE TABLE Person(id INT64, PRIMARY KEY(id));\n",
    )
    .unwrap();
    let output = Command::cargo_bin("koko")
        .unwrap()
        .args(["--no-config", "--init"])
        .arg(&init)
        .args([
            "--command",
            "CREATE (:Person {id: 9}); MATCH (p:Person) RETURN p.id AS id;",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["results"][1]["rows"], serde_json::json!([[9]]));
}

#[test]
fn observational_meta_commands_use_structured_graph_scoped_results() {
    let output = Command::cargo_bin("koko")
        .unwrap()
        .args([
            "--no-config",
            "--command",
            "CREATE GRAPH analytics;\nUSE GRAPH analytics;\nCREATE NODE TABLE Person(id INT64, PRIMARY KEY(id));\nUSE GRAPH main;\n:graphs\n:schema analytics.Person\n:describe analytics.Person\n:functions count",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| {
            record["values"] == serde_json::json!([false, "analytics", "typed", 1])
        })
    );
    assert!(records.iter().any(|record| {
        record["values"].as_array().is_some_and(|values| {
            values.len() == 1
                && values[0]
                    .as_str()
                    .is_some_and(|value| value.contains("CREATE NODE TABLE `Person`"))
        })
    }));
    assert!(records.iter().any(|record| {
        record["values"]
            .as_array()
            .is_some_and(|values| values.first() == Some(&Value::String("Person".to_string())))
    }));
    assert!(records.iter().any(|record| {
        record["values"].as_array().is_some_and(|values| {
            values
                .first()
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case("count"))
        })
    }));
}

#[test]
fn output_command_switches_to_an_explicit_atomic_destination() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("selected.jsonl");
    let source = format!(
        ":output '{}' replace\nRETURN 7 AS selected",
        destination.display()
    );
    Command::cargo_bin("koko")
        .unwrap()
        .args(["--no-config", "--command", &source, "--format", "jsonl"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    let records = std::fs::read_to_string(&destination).unwrap();
    assert!(records.contains("\"values\":[7]"));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
