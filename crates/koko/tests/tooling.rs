use koko::diagnostics::{FailureKind, InterruptReason};
use koko::execution::Parameter;
use koko::function::{NullPolicy, ScalarFunction};
use koko::result::{CellValue, Column, ResultKind};
use koko::tooling::{
    CursorContextKind, FunctionKind, GraphKind, OutputClass, StatementClass, SyntaxStatus,
    TokenKind, TransactionMode, analyze_cypher, tabular_result,
};
use koko::{Database, LogicalType, Value};

#[test]
fn canonical_segmentation_preserves_nested_semicolons_and_ignores_empty_segments() {
    let source = " ; RETURN ';' AS text; /* ; */ RETURN [1, 2][0] AS value;;; // tail\n";
    let analysis = analyze_cypher(source, None);
    assert_eq!(analysis.status(), SyntaxStatus::Complete);
    assert_eq!(analysis.statements().len(), 2);
    assert!(
        analysis
            .tokens()
            .iter()
            .any(|token| token.kind() == TokenKind::Comment)
    );
    assert!(analysis.statements().iter().all(|statement| {
        statement.class() == Some(StatementClass::Query)
            && statement.output_class() == Some(OutputClass::Rows)
    }));
}

#[test]
fn completeness_distinguishes_empty_incomplete_invalid_and_forced_submit_source() {
    assert_eq!(
        analyze_cypher(" ; // comment only\n", None).status(),
        SyntaxStatus::Empty
    );
    let incomplete = analyze_cypher("MATCH (n RETURN n", None);
    assert_eq!(incomplete.status(), SyntaxStatus::Incomplete);
    assert!(
        incomplete
            .diagnostic()
            .and_then(|value| value.span())
            .is_some()
    );

    let invalid = analyze_cypher("RETURN )", None);
    assert_eq!(invalid.status(), SyntaxStatus::Invalid);
    assert!(invalid.diagnostic().is_some());

    // Tooling classification never blocks forced submission through Connection::query.
    let forced = analyze_cypher("RETURN", None);
    assert_eq!(forced.status(), SyntaxStatus::Incomplete);
}

#[test]
fn statement_and_token_spans_are_utf8_byte_offsets() {
    let source = "RETURN '東京' AS city; RETURN $名前 AS parameter";
    let analysis = analyze_cypher(source, Some(source.len()));
    assert_eq!(analysis.status(), SyntaxStatus::Complete);
    assert_eq!(analysis.statements().len(), 2);
    let literal = analysis
        .tokens()
        .iter()
        .find(|token| token.kind() == TokenKind::String)
        .expect("string token");
    assert_eq!(
        &source[literal.span().start()..literal.span().end()],
        "'東京'"
    );
    let parameter = analysis
        .tokens()
        .iter()
        .find(|token| token.kind() == TokenKind::Parameter)
        .expect("parameter token");
    assert_eq!(
        &source[parameter.span().start()..parameter.span().end()],
        "$名前"
    );
}

#[test]
fn every_completion_context_family_is_structured() {
    let cases = [
        ("RET", CursorContextKind::Keyword),
        ("USE GRAPH ana", CursorContextKind::Graph),
        ("MATCH (n:Per", CursorContextKind::NodeLabel),
        ("MATCH ()-[r:Kno", CursorContextKind::RelationshipLabel),
        ("MATCH (n) RETURN n", CursorContextKind::Variable),
        ("MATCH (n) RETURN n.na", CursorContextKind::Property),
        ("RETURN count(", CursorContextKind::Function),
        ("RETURN $min", CursorContextKind::Parameter),
        ("CALL thr", CursorContextKind::Setting),
        ("IMPORT DATABASE '", CursorContextKind::Path),
    ];
    for (source, expected) in cases {
        let cursor = source.len() - usize::from(expected == CursorContextKind::Function);
        let analysis = analyze_cypher(source, Some(cursor));
        assert_eq!(
            analysis.cursor_context().map(|context| context.kind()),
            Some(expected),
            "source: {source}"
        );
    }
}

#[test]
fn multiple_statement_classes_are_exposed_without_binding() {
    let cases = [
        ("CREATE GRAPH g", StatementClass::Graph, OutputClass::Status),
        (
            "BEGIN TRANSACTION",
            StatementClass::Transaction,
            OutputClass::Status,
        ),
        ("CALL threads=1", StatementClass::Setting, OutputClass::Rows),
        (
            "CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))",
            StatementClass::DataDefinition,
            OutputClass::Status,
        ),
        (
            "EXPLAIN RETURN 1",
            StatementClass::Explain,
            OutputClass::Plan,
        ),
        (
            "PROFILE RETURN 1",
            StatementClass::Profile,
            OutputClass::Plan,
        ),
    ];
    for (source, class, output) in cases {
        let analysis = analyze_cypher(source, None);
        let statement = &analysis.statements()[0];
        assert_eq!(statement.class(), Some(class), "source: {source}");
        assert_eq!(statement.output_class(), Some(output), "source: {source}");
    }
}

#[test]
fn session_snapshots_follow_graph_transaction_and_setting_authority() {
    let database = Database::new();
    let connection = database.connect();
    let initial = connection.session_snapshot().unwrap();
    assert_eq!(initial.graph().name(), "main");
    assert_eq!(initial.graph().kind(), GraphKind::Typed);
    assert_eq!(initial.transaction(), TransactionMode::None);
    assert!(initial.timeout().is_none());

    connection.execute("CREATE GRAPH dynamic ANY").unwrap();
    connection.execute("USE GRAPH dynamic").unwrap();
    connection
        .set_query_timeout(Some(std::time::Duration::from_millis(25)))
        .unwrap();
    connection.set_max_threads(1).unwrap();
    let selected = connection.session_snapshot().unwrap();
    assert_eq!(selected.graph().name(), "dynamic");
    assert_eq!(selected.graph().kind(), GraphKind::Any);
    assert!(selected.revision() > initial.revision());
    assert_eq!(selected.timeout().unwrap().as_millis(), 25);
    assert_eq!(selected.workers(), 1);

    connection.execute("BEGIN TRANSACTION READ ONLY").unwrap();
    assert_eq!(
        connection.session_snapshot().unwrap().transaction(),
        TransactionMode::ReadOnly
    );
    connection.execute("ROLLBACK").unwrap();
    connection.execute("BEGIN TRANSACTION").unwrap();
    assert_eq!(
        connection.session_snapshot().unwrap().transaction(),
        TransactionMode::ReadWrite
    );
    connection.execute("ROLLBACK").unwrap();
    assert_eq!(
        connection.session_snapshot().unwrap().transaction(),
        TransactionMode::None
    );
}

#[test]
fn catalog_snapshot_is_transaction_coherent_owned_and_hides_any_tables() {
    let database = Database::new();
    let writer = database.connect();
    let observer = database.connect();
    writer.execute("CREATE GRAPH dynamic ANY").unwrap();
    writer.execute("USE GRAPH dynamic").unwrap();
    let any = writer.catalog_snapshot().unwrap();
    assert!(any.node_tables().is_empty());
    assert!(any.relationship_tables().is_empty());
    assert!(
        any.graphs()
            .iter()
            .any(|graph| graph.name() == "dynamic" && graph.kind() == GraphKind::Any)
    );

    writer.execute("USE GRAPH main").unwrap();
    writer.execute("BEGIN TRANSACTION").unwrap();
    writer
        .execute("CREATE NODE TABLE Person(id INT64, name STRING, PRIMARY KEY(id))")
        .unwrap();
    writer
        .execute("CREATE NODE TABLE Company(id INT64, PRIMARY KEY(id))")
        .unwrap();
    writer
        .execute("CREATE REL TABLE WorksAt(FROM Person TO Company, since INT64)")
        .unwrap();
    writer.execute("CREATE MACRO add_one(x) AS x + 1").unwrap();
    writer
        .execute("CREATE HASH INDEX person_pk FOR (p:Person) ON (p.id)")
        .unwrap();
    let inside = writer.catalog_snapshot().unwrap();
    assert_eq!(inside.node_tables()[0].name(), "Person");
    assert!(
        inside.node_tables()[0]
            .properties()
            .iter()
            .any(|property| property.name() == "id" && property.is_primary_key())
    );
    assert_eq!(inside.indexes()[0].name(), "person_pk");
    assert_eq!(inside.relationship_tables()[0].name(), "WorksAt");
    assert_eq!(
        inside.relationship_tables()[0].endpoints()[0].from(),
        "Person"
    );
    assert_eq!(
        inside.relationship_tables()[0].endpoints()[0].to(),
        "Company"
    );
    assert!(inside.macros().iter().any(|item| item.name() == "ADD_ONE"));
    assert!(
        inside
            .functions()
            .iter()
            .any(|item| { item.name() == "ADD_ONE" && item.kind() == FunctionKind::Macro })
    );
    assert!(
        inside
            .schema_script()
            .contains("CREATE NODE TABLE `Person`")
    );
    assert!(
        observer
            .catalog_snapshot()
            .unwrap()
            .node_tables()
            .is_empty()
    );

    writer.execute("ROLLBACK").unwrap();
    assert!(writer.catalog_snapshot().unwrap().node_tables().is_empty());
    // The prior owned view remains valid after locks and transaction state are gone.
    assert_eq!(inside.node_tables()[0].name(), "Person");
}

#[test]
fn graph_drop_fallback_and_registry_revisions_are_authoritative() {
    let database = Database::new();
    let selected = database.connect();
    let mutator = database.connect();
    let before = selected.session_snapshot().unwrap();
    selected.execute("CREATE GRAPH transient").unwrap();
    selected.execute("USE GRAPH transient").unwrap();
    let during = selected.session_snapshot().unwrap();
    assert!(during.graph_registry_revision() > before.graph_registry_revision());
    mutator.execute("DROP GRAPH transient").unwrap();
    let after = selected.session_snapshot().unwrap();
    assert_eq!(after.graph().name(), "main");
    assert!(after.revision() > during.revision());
}

#[test]
fn catalog_and_function_revisions_track_committed_and_connection_local_changes() {
    let database = Database::new();
    let connection = database.connect();
    let initial = connection.catalog_snapshot().unwrap();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let catalog_changed = connection.catalog_snapshot().unwrap();
    assert!(catalog_changed.catalog_revision() > initial.catalog_revision());
    assert!(
        catalog_changed
            .settings()
            .iter()
            .any(|setting| setting.name() == "threads")
    );

    connection
        .register_scalar_function(
            ScalarFunction::new("answer", vec![], LogicalType::Int64, |_| {
                Ok(Value::Int64(42))
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    let udf_changed = connection.catalog_snapshot().unwrap();
    assert!(udf_changed.function_revision() > catalog_changed.function_revision());
    assert!(udf_changed.functions().iter().any(|function| {
        function.name() == "answer"
            && function.kind() == FunctionKind::ConnectionLocal
            && function.return_type() == "INT64"
    }));
    assert!(
        udf_changed
            .functions()
            .iter()
            .any(|function| function.kind() == FunctionKind::Aggregate)
    );
}

#[test]
fn concurrent_catalog_snapshots_never_publish_torn_table_metadata() {
    use std::sync::Arc;
    use std::thread;

    let database = Database::new();
    let connection = Arc::new(database.connect());
    let reader = Arc::clone(&connection);
    let handle = thread::spawn(move || {
        for _ in 0..32 {
            let snapshot = reader.catalog_snapshot().unwrap();
            for table in snapshot.node_tables() {
                assert!(!table.properties().is_empty());
                assert!(
                    table
                        .properties()
                        .iter()
                        .any(|property| property.is_primary_key())
                );
            }
        }
    });
    for index in 0..8 {
        connection
            .execute(&format!(
                "CREATE NODE TABLE T{index}(id INT64, PRIMARY KEY(id))"
            ))
            .unwrap();
    }
    handle.join().unwrap();
}

#[test]
fn structured_results_retain_kinds_borrowed_cells_and_type_context() {
    let database = Database::new();
    let connection = database.connect();
    let status = connection
        .execute("CREATE NODE TABLE Person(id INT64, name STRING, PRIMARY KEY(id))")
        .unwrap();
    assert_eq!(status.kind(), ResultKind::Status);
    assert_eq!(
        status.status_message(),
        Some("Table Person has been created.")
    );

    connection
        .execute("CREATE (:Person {id: 7, name: 'Ada'})")
        .unwrap();
    let rows = connection
        .execute("MATCH (person:Person) RETURN person.id, person.name")
        .unwrap();
    assert_eq!(rows.kind(), ResultKind::Rows);
    assert!(matches!(
        rows.cell(0, 0).unwrap().value(),
        CellValue::Int { value: 7, .. }
    ));
    assert!(matches!(
        rows.cell(0, 1).unwrap().value(),
        CellValue::String("Ada")
    ));
    let person_type = rows
        .type_context()
        .graph_values()
        .iter()
        .find(|value| value.name() == "Person")
        .unwrap();
    assert!(!person_type.is_relationship());
    assert_eq!(
        person_type.properties()[0].logical_type(),
        &LogicalType::Int64
    );

    connection.execute("DROP TABLE Person").unwrap();
    assert_eq!(
        rows.type_context().graph_values()[0].name(),
        "Person",
        "materialized result context must not observe later catalog changes"
    );
}

#[test]
fn explain_and_profile_publish_structural_rust_plans() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.execute("CREATE (:Person {id: 1})").unwrap();

    let explain = connection
        .execute("EXPLAIN MATCH (person:Person) RETURN person.id")
        .unwrap();
    assert_eq!(explain.kind(), ResultKind::Explain);
    assert_eq!(explain.len(), 0);
    let plan = explain.plan().unwrap();
    assert!(!plan.is_profile());
    assert_eq!(plan.roots()[0].operator(), "UnionOperand");
    assert!(!plan.roots()[0].children().is_empty());

    let profile = connection
        .execute("PROFILE MATCH (person:Person) RETURN person.id")
        .unwrap();
    assert_eq!(profile.kind(), ResultKind::Profile);
    assert_eq!(profile.rows().next().unwrap().get::<i64>(0).unwrap(), 1);
    assert!(profile.plan().unwrap().is_profile());
    assert!(profile.plan().unwrap().execution_time().is_some());
}

#[test]
fn explain_validates_and_profile_executes_non_query_statements() {
    let connection = Database::new().connect();
    connection
        .execute("EXPLAIN CREATE NODE TABLE Explained(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .execute("CREATE NODE TABLE Explained(id INT64, PRIMARY KEY(id))")
        .unwrap();

    connection
        .execute("PROFILE CREATE NODE TABLE Profiled(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.execute("CREATE (:Profiled {id: 1})").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (node:Profiled) RETURN node.id")
            .unwrap()
            .rows()
            .map(|row| row.get::<i64>(0).unwrap())
            .collect::<Vec<_>>(),
        vec![1]
    );

    connection.execute("CALL threads=3").unwrap();
    connection.execute("EXPLAIN CALL threads=2").unwrap();
    assert_eq!(connection.session_snapshot().unwrap().workers(), 3);
    connection.execute("PROFILE CALL threads=2").unwrap();
    assert_eq!(connection.session_snapshot().unwrap().workers(), 2);
}

#[test]
fn result_diagnostics_are_scoped_to_the_result() {
    let path = std::env::temp_dir().join(format!(
        "koko_cli_warning_{}_{}.csv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, "1\n1\n").unwrap();
    let source = path.to_string_lossy().replace('\\', "/");
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let copy = connection
        .execute(&format!(
            "COPY Person FROM \"{source}\" (ignore_errors=true)"
        ))
        .unwrap();
    assert_eq!(copy.diagnostics().total_warning_count(), 1);
    assert_eq!(copy.diagnostics().warnings().len(), 1);
    assert_eq!(
        std::fs::canonicalize(copy.diagnostics().warnings()[0].file_path()).unwrap(),
        std::fs::canonicalize(&path).unwrap()
    );
    assert_eq!(
        connection
            .execute("RETURN 1")
            .unwrap()
            .diagnostics()
            .total_warning_count(),
        0
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn typed_parameters_and_structured_failures_preserve_engine_metadata() {
    let database = Database::new();
    let connection = database.connect();
    let source_value = Value::String("42".to_string());
    let declared_type = LogicalType::Int64;
    let result = connection
        .execute_with(
            "RETURN $answer",
            [Parameter::typed("answer", source_value, declared_type)],
        )
        .unwrap();
    assert_eq!(result.rows().next().unwrap().get::<i64>(0).unwrap(), 42);

    let parser = connection.execute_detailed("RETURN (");
    assert_eq!(parser.failure().unwrap().kind(), FailureKind::Parser);
    assert!(parser.failure().unwrap().diagnostic().is_some());
    assert!(parser.into_result().is_err());

    let binder = connection.execute_detailed("MATCH (n:Missing) RETURN n");
    assert_eq!(binder.failure().unwrap().kind(), FailureKind::Binder);

    connection
        .set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let deadline =
        connection.execute_detailed("UNWIND range(0, 1000000) AS value RETURN sum(value)");
    assert_eq!(deadline.failure().unwrap().kind(), FailureKind::Interrupt);
    assert_eq!(
        deadline.failure().unwrap().interrupt_reason(),
        Some(InterruptReason::Deadline)
    );
    connection.set_query_timeout(None).unwrap();
}

#[test]
fn tooling_rows_use_normal_columnar_traversal_and_validate_shape() {
    let result = tabular_result(
        vec![
            Column::new("name", LogicalType::String),
            Column::new("value", LogicalType::Any),
        ],
        vec![vec![Value::String("answer".to_string()), Value::Int64(42)]],
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(
        result.rows().next().unwrap().get::<String>(0).unwrap(),
        "answer"
    );
    assert_eq!(result.rows().next().unwrap().get::<i64>(1).unwrap(), 42);
    assert!(
        tabular_result(
            vec![Column::new("only", LogicalType::String)],
            vec![vec![Value::Int64(42)]],
        )
        .is_err()
    );
    assert!(
        tabular_result(
            vec![Column::new("only", LogicalType::String)],
            vec![vec![Value::String("extra".to_string()), Value::Null]],
        )
        .is_err()
    );
}

#[test]
fn graph_scoped_catalog_snapshot_is_observational_and_structured() {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE GRAPH analytics").unwrap();
    connection.execute("USE GRAPH analytics").unwrap();
    connection
        .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.execute("USE GRAPH main").unwrap();

    let snapshot = connection.catalog_snapshot_for_graph("analytics").unwrap();
    assert_eq!(snapshot.node_tables()[0].name(), "Person");
    assert_eq!(
        snapshot.schema_statements_for_object("Person"),
        vec!["CREATE NODE TABLE `Person` (`id` INT64, PRIMARY KEY(`id`));"]
    );
    assert_eq!(
        connection.session_snapshot().unwrap().graph().name(),
        "main"
    );
    assert!(connection.catalog_snapshot_for_graph("missing").is_err());
}
