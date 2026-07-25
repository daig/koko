use koko::{
    Database, IntKind, InternalId, Interval, JsonValue, LogicalType, NodeValue, RecursiveRelValue,
    RelValue, TableId, Value,
};
use koko_cli::bootstrap::Format;
use koko_cli::output::{CollisionPolicy, OutputError, OutputTransaction};
use koko_cli::presentation::{
    PresentationError, PresentationSettings, Presenter, StatementContext,
};
use std::io::Write as _;

fn settings(format: Format) -> PresentationSettings {
    PresentationSettings {
        format,
        timing: false,
        header: true,
        null_token: "\\N".to_string(),
        row_limit: Some(20),
        max_width: Some(80),
        null_display: "NULL".to_string(),
    }
}

#[test]
fn json_document_is_valid_typed_and_preserves_duplicate_columns() {
    let database = Database::in_memory();
    let connection = database.connect();
    let outcome = connection.execute_with_metadata(
        "RETURN 9007199254740992 AS duplicate, 2 AS duplicate, NULL AS missing",
        &[],
    );
    let result = outcome.result().unwrap();
    let mut presenter = Presenter::begin(Vec::new(), Vec::new(), settings(Format::Json)).unwrap();
    presenter
        .present_result(&StatementContext::new(1, 1), result)
        .unwrap();
    let (data, diagnostics) = presenter.finish().unwrap();
    assert!(diagnostics.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(json["version"], 1);
    assert_eq!(json["complete"], true);
    assert_eq!(json["results"][0]["columns"][0]["name"], "duplicate");
    assert_eq!(json["results"][0]["columns"][1]["name"], "duplicate");
    assert_eq!(json["results"][0]["rows"][0][0]["$type"], "INTEGER");
    assert_eq!(
        json["results"][0]["rows"][0][0]["value"],
        "9007199254740992"
    );
    assert_eq!(json["results"][0]["rows"][0][2], serde_json::Value::Null);
    assert!(json["results"][0]["summary"].get("compiling_ms").is_none());
}

#[test]
fn json_failure_closes_one_valid_incomplete_document() {
    let database = Database::in_memory();
    let connection = database.connect();
    let outcome = connection.execute_with_metadata("RETURN )", &[]);
    let failure = outcome.failure().unwrap();
    let mut presenter = Presenter::begin(Vec::new(), Vec::new(), settings(Format::Json)).unwrap();
    presenter
        .present_failure(&StatementContext::new(1, 1), failure)
        .unwrap();
    let (data, diagnostics) = presenter.finish().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(json["complete"], false);
    assert_eq!(json["error"]["statement"], 1);
    assert_eq!(json["error"]["error"]["kind"], "parser");
    assert!(!diagnostics.is_empty());
}

#[test]
fn json_lines_are_independently_valid_and_versioned() {
    let database = Database::in_memory();
    let connection = database.connect();
    let outcome = connection.execute_with_metadata("RETURN 1 AS answer UNION ALL RETURN 2", &[]);
    let mut presenter =
        Presenter::begin(Vec::new(), Vec::new(), settings(Format::JsonLines)).unwrap();
    presenter
        .present_result(&StatementContext::new(3, 2), outcome.result().unwrap())
        .unwrap();
    let (data, _) = presenter.finish().unwrap();
    let records = std::str::from_utf8(&data)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.first().unwrap()["type"], "schema");
    assert_eq!(records.last().unwrap()["type"], "summary");
    assert_eq!(records.len(), 4);
    assert!(
        records
            .iter()
            .all(|record| record["version"] == 1 && record["result"] == 2)
    );
}

#[test]
fn csv_quotes_strings_and_withholds_ambiguous_multiple_results() {
    let database = Database::in_memory();
    let connection = database.connect();
    let status = connection
        .query("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let first = connection
        .query("RETURN 'a,b' AS text, NULL AS missing")
        .unwrap();
    let second = connection.query("RETURN 2 AS answer").unwrap();
    let mut presenter = Presenter::begin(Vec::new(), Vec::new(), settings(Format::Csv)).unwrap();
    presenter
        .present_result(&StatementContext::new(1, 1), &first)
        .unwrap();
    let error = presenter
        .present_result(&StatementContext::new(2, 2), &second)
        .unwrap_err();
    assert!(matches!(error, PresentationError::AmbiguousDelimited));
    let (data, _) = presenter.finish().unwrap();
    assert!(data.is_empty(), "ambiguous row bytes must remain staged");

    let mut presenter = Presenter::begin(Vec::new(), Vec::new(), settings(Format::Csv)).unwrap();
    presenter
        .present_result(&StatementContext::new(1, 1), &status)
        .unwrap();
    presenter
        .present_result(&StatementContext::new(2, 2), &first)
        .unwrap();
    let (data, diagnostics) = presenter.finish().unwrap();
    assert_eq!(
        std::str::from_utf8(&data).unwrap(),
        "text,missing\n\"a,b\",\\N\n"
    );
    assert!(!std::str::from_utf8(&data).unwrap().contains("created"));
    assert!(
        std::str::from_utf8(&diagnostics)
            .unwrap()
            .contains("created")
    );
}

#[test]
fn typed_value_codec_preserves_order_and_lossless_scalars() {
    let value = Value::Struct(vec![
        (
            "wide".to_string(),
            Value::IntX {
                value: i128::MAX,
                kind: IntKind::I128,
            },
        ),
        (
            "decimal".to_string(),
            Value::Decimal {
                value: 12345,
                precision: 8,
                scale: 2,
            },
        ),
        ("blob".to_string(), Value::Blob(vec![0, 1, 2, 255])),
    ]);
    let logical_type = LogicalType::Struct(vec![
        ("wide".to_string(), LogicalType::Int(IntKind::I128)),
        ("decimal".to_string(), LogicalType::Decimal(8, 2)),
        ("blob".to_string(), LogicalType::Blob),
    ]);
    let mut encoded = Vec::new();
    koko_cli::value_codec::write_typed_value(
        &mut encoded,
        &value,
        &logical_type,
        &koko::ResultTypeContext::default(),
    )
    .unwrap();
    let text = std::str::from_utf8(&encoded).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert!(text.find("wide").unwrap() < text.find("decimal").unwrap());
    assert_eq!(json["fields"][0][1]["logical_type"], "INT128");
    assert_eq!(json["fields"][1][1]["value"], "123.45");
    assert_eq!(json["fields"][2][1]["value"], "AAEC/w==");
}

#[test]
fn typed_value_codec_covers_machine_scalar_and_graph_classes() {
    fn encode(value: &Value, logical_type: &LogicalType) -> serde_json::Value {
        let mut output = Vec::new();
        koko_cli::value_codec::write_typed_value(
            &mut output,
            value,
            logical_type,
            &koko::ResultTypeContext::default(),
        )
        .unwrap();
        serde_json::from_slice(&output).unwrap()
    }

    assert_eq!(encode(&Value::Double(1.5), &LogicalType::Double), 1.5);
    assert_eq!(
        encode(&Value::Double(f64::NAN), &LogicalType::Double)["value"],
        "NaN"
    );
    assert_eq!(
        encode(
            &Value::Json(JsonValue::Object(vec![
                ("b".to_string(), JsonValue::Int(2)),
                ("a".to_string(), JsonValue::Int(1)),
            ])),
            &LogicalType::Json,
        )
        .to_string(),
        r#"{"b":2,"a":1}"#
    );
    assert_eq!(
        encode(&Value::Date(0), &LogicalType::Date)["value"],
        "1970-01-01"
    );
    assert_eq!(
        encode(
            &Value::Interval(Interval {
                months: 1,
                days: 2,
                micros: 3,
            }),
            &LogicalType::Interval
        )["micros"],
        3
    );
    assert_eq!(
        encode(
            &Value::List(vec![Value::Bool(true), Value::Bool(false)]),
            &LogicalType::List(Box::new(LogicalType::Bool)),
        ),
        serde_json::json!([true, false])
    );
    assert_eq!(
        encode(
            &Value::Map(vec![(Value::Int64(1), Value::String("one".to_string()))]),
            &LogicalType::Map(Box::new(LogicalType::Int64), Box::new(LogicalType::String),),
        )["entries"][0],
        serde_json::json!([1, "one"])
    );
    let variants = vec![
        ("number".to_string(), LogicalType::Int64),
        ("text".to_string(), LogicalType::String),
    ];
    assert_eq!(
        encode(
            &Value::Union {
                variants: variants.clone(),
                tag: 1,
                value: Box::new(Value::String("selected".to_string())),
            },
            &LogicalType::Union(variants),
        )["tag"],
        "text"
    );

    let node_id = InternalId::new(TableId(1), 2);
    let rel_id = InternalId::new(TableId(3), 4);
    let node = NodeValue {
        id: node_id,
        label: "Person".to_string(),
        props: vec![("name".to_string(), Value::String("Ada".to_string()))],
    };
    let rel = RelValue {
        src: node_id,
        dst: InternalId::new(TableId(1), 5),
        id: rel_id,
        label: "KNOWS".to_string(),
        props: vec![("since".to_string(), Value::Int64(1843))],
        src_node: None,
        dst_node: None,
    };
    let encoded_node = encode(
        &Value::Node(Box::new(node.clone())),
        &LogicalType::Node(TableId(1)),
    );
    assert_eq!(encoded_node["id"]["table"], "1");
    assert_eq!(
        encoded_node["properties"][0],
        serde_json::json!(["name", "Ada"])
    );
    let encoded_rel = encode(
        &Value::Rel(Box::new(rel.clone())),
        &LogicalType::Rel(TableId(3)),
    );
    assert_eq!(encoded_rel["src"]["offset"], "2");
    assert!(encoded_rel.get("src_node").is_none());
    let path = Value::RecursiveRel(Box::new(RecursiveRelValue {
        nodes: vec![node],
        rels: vec![rel],
        degenerate: false,
        cost: Some(2.5),
        null_nodes: 0,
    }));
    let encoded_path = encode(&path, &LogicalType::RecursiveRel);
    assert_eq!(encoded_path["relationships"].as_array().unwrap().len(), 1);
    assert_eq!(encoded_path["cost"], 2.5);
    let decoded =
        koko_cli::value_codec::decode_value(&serde_json::to_string(&encoded_path).unwrap())
            .unwrap();
    assert_eq!(decoded, path);
}

#[test]
fn human_box_uses_head_tail_rows_and_grapheme_safe_truncation() {
    let database = Database::in_memory();
    let connection = database.connect();
    let result = connection
        .query("UNWIND range(1, 5) AS n RETURN n, '漢字évery-long' AS text")
        .unwrap();
    let mut options = settings(Format::Box);
    options.row_limit = Some(2);
    options.max_width = Some(30);
    let mut presenter = Presenter::begin(Vec::new(), Vec::new(), options).unwrap();
    presenter
        .present_result(&StatementContext::new(1, 1), &result)
        .unwrap();
    let (data, diagnostics) = presenter.finish().unwrap();
    let text = std::str::from_utf8(&data).unwrap();
    assert!(text.contains('┌') && text.contains('…'));
    assert!(text.contains("1") && text.contains("5"));
    assert!(
        std::str::from_utf8(&diagnostics)
            .unwrap()
            .contains("2 displayed")
    );
    assert!(!text.contains('\u{fffd}'));
    use unicode_width::UnicodeWidthStr as _;
    assert!(text.lines().all(|line| line.width() <= 30));
}

#[test]
fn output_transaction_publishes_atomically_and_detects_races() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("result.jsonl");
    let mut transaction =
        OutputTransaction::begin(&output, CollisionPolicy::Refuse, Format::JsonLines).unwrap();
    transaction.write_all(b"new\n").unwrap();
    assert!(!output.exists());
    transaction.commit().unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"new\n");

    let mut raced =
        OutputTransaction::begin(&output, CollisionPolicy::Replace, Format::JsonLines).unwrap();
    raced.write_all(b"replacement\n").unwrap();
    std::fs::write(&output, b"concurrent\n").unwrap();
    assert!(matches!(raced.commit(), Err(OutputError::Changed(_))));
    assert_eq!(std::fs::read(&output).unwrap(), b"concurrent\n");

    assert!(matches!(
        OutputTransaction::begin(&output, CollisionPolicy::Append, Format::Json),
        Err(OutputError::JsonAppend)
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn output_transaction_rejects_symlink_destinations() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let link = root.path().join("link");
    std::fs::write(&target, b"old").unwrap();
    symlink(&target, &link).unwrap();
    assert!(matches!(
        OutputTransaction::begin(&link, CollisionPolicy::Replace, Format::JsonLines),
        Err(OutputError::Symlink(_))
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[test]
fn write_failures_propagate_without_partial_success() {
    struct FailingWriter(usize);
    impl std::io::Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("injected"));
            }
            let written = self.0.min(bytes.len());
            self.0 -= written;
            Ok(written)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let database = Database::in_memory();
    let connection = database.connect();
    let result = connection.query("RETURN 1 AS answer").unwrap();
    let mut presenter =
        Presenter::begin(FailingWriter(64), Vec::new(), settings(Format::Json)).unwrap();
    let error = presenter
        .present_result(&StatementContext::new(1, 1), &result)
        .unwrap_err();
    assert!(matches!(error, PresentationError::Io(_)));
}
