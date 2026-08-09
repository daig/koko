use koko::function::{NullPolicy, ScalarFunction};
use koko::prepared::StatementKind;
use koko::result::ResultKind;
use koko::{Database, Error, LogicalType, Result, Value, params};

#[test]
fn canonical_embedding_flow_and_borrowed_results() -> Result<()> {
    let database = Database::new();
    let mut connection = database.connect();
    connection.execute("CREATE NODE TABLE Person(id INT64, name STRING, PRIMARY KEY(id))")?;
    connection.execute_with(
        "CREATE (:Person {id: $id, name: $name})",
        params! { "id" => 1, "name" => "Alice" },
    )?;

    let result = connection.execute_with(
        "MATCH (p:Person) WHERE p.id >= $min RETURN p.id AS id, p.name AS name",
        params! { "min" => 1 },
    )?;
    assert_eq!(result.kind(), ResultKind::Rows);
    assert_eq!(result.len(), 1);
    assert_eq!(result.width(), 2);
    assert_eq!(result.columns()[0].name(), "id");
    assert_eq!(result.row(0).unwrap().get::<i64>(0)?, 1);
    assert_eq!(result.row(0).unwrap().get::<String>("name")?, "Alice");
    assert_eq!(
        (&result)
            .into_iter()
            .map(|row| row.get::<String>("name"))
            .collect::<Result<Vec<_>>>()?,
        ["Alice"]
    );
    assert_eq!(
        result
            .typed_column::<i64>("id")?
            .iter()
            .collect::<Result<Vec<_>>>()?,
        [1]
    );

    let batches = result.to_arrow_record_batches()?;
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 1);
    assert_eq!(batches[0].num_columns(), 2);

    {
        let mut prepared =
            connection.prepare("MATCH (p:Person) WHERE p.id = $id RETURN p.name AS name")?;
        assert_eq!(prepared.kind(), StatementKind::Query);
        assert!(prepared.is_read_only());
        assert_eq!(prepared.parameters()[0].name(), "id");
        assert_eq!(
            prepared
                .execute_with(params! { "id" => 1 })?
                .row(0)
                .unwrap()
                .get::<String>("name")?,
            "Alice"
        );
        assert!(prepared.execute_with(params! { "id" => 99 })?.is_empty());
    }

    {
        let transaction = connection.transaction()?;
        transaction.execute("CREATE (:Person {id: 2, name: 'Bob'})")?;
        transaction.commit()?;
    }
    assert_eq!(
        connection
            .execute("MATCH (p:Person) RETURN count(*)")?
            .row(0)
            .unwrap()
            .get::<i64>(0)?,
        2
    );

    {
        let transaction = connection.transaction()?;
        transaction.execute("CREATE (:Person {id: 3, name: 'Cara'})")?;
    }
    assert!(
        connection
            .execute("MATCH (p:Person {id: 3}) RETURN p.id")?
            .is_empty()
    );

    Ok(())
}

#[test]
fn union_canonical_schema_is_visible_through_public_and_arrow_apis() -> Result<()> {
    let database = Database::new();
    let connection = database.connect();

    for query in [
        "RETURN 1 AS x UNION ALL RETURN null AS x",
        "RETURN null AS x UNION ALL RETURN 1 AS x",
    ] {
        let result = connection.execute(query)?;
        assert_eq!(result.columns()[0].name(), "x");
        assert_eq!(result.columns()[0].logical_type(), &LogicalType::Int64);
        let batches = result.to_arrow_record_batches()?;
        assert_eq!(
            batches.iter().map(|batch| batch.num_rows()).sum::<usize>(),
            2
        );
        assert!(batches.iter().all(|batch| batch.num_columns() == 1));
    }

    {
        let mut prepared = connection.prepare("RETURN $value AS x UNION ALL RETURN 1 AS x")?;
        assert_eq!(prepared.parameters()[0].logical_type(), &LogicalType::Int64);
        assert_eq!(prepared.columns()[0].logical_type(), &LogicalType::Int64);
        assert_eq!(
            prepared.execute()?.row(0).unwrap().get::<Value>(0)?,
            Value::Null
        );
        assert_eq!(
            prepared
                .execute_with(params! { "value" => 2 })?
                .row(0)
                .unwrap()
                .get::<i64>(0)?,
            2
        );
    }

    let dynamic = connection.execute(
        "RETURN 's' AS x UNION ALL \
         RETURN union_extract(union_value(a := 1), 'a') AS x",
    )?;
    assert_eq!(dynamic.columns()[0].logical_type(), &LogicalType::Any);
    assert_eq!(
        dynamic.row(0).unwrap().get::<Value>(0)?,
        Value::String("s".to_string())
    );
    assert_eq!(dynamic.row(1).unwrap().get::<Value>(0)?, Value::Int64(1));
    assert_eq!(
        dynamic.to_arrow_record_batches().unwrap_err().to_string(),
        "Not implemented exception: Arrow interchange does not support Koko logical type ANY."
    );

    let null_only = connection.execute("RETURN null AS x UNION ALL RETURN null AS x")?;
    assert_eq!(null_only.columns()[0].logical_type(), &LogicalType::Any);
    assert_eq!(
        null_only.to_arrow_record_batches().unwrap_err().to_string(),
        "Not implemented exception: Arrow interchange does not support Koko logical type ANY."
    );

    Ok(())
}

#[test]
fn structural_presentations_and_scalar_function_descriptor() -> Result<()> {
    let database = Database::default();
    let connection = database.connect();

    let status = connection.execute("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")?;
    assert_eq!(status.kind(), ResultKind::Status);
    assert_eq!(status.len(), 0);
    assert_eq!(status.width(), 0);
    assert!(status.status_message().is_some());
    assert!(status.plan().is_none());
    assert_eq!(
        status.to_string(),
        format!("{}\n", status.status_message().unwrap())
    );

    connection.execute("CREATE (:N {id: 1})")?;
    let explain = connection.execute("EXPLAIN MATCH (n:N) RETURN n.id")?;
    assert_eq!(explain.kind(), ResultKind::Explain);
    assert!(explain.is_empty());
    assert!(explain.columns().is_empty());
    assert!(explain.plan().is_some());

    let profile = connection.execute("PROFILE MATCH (n:N) RETURN n.id")?;
    assert_eq!(profile.kind(), ResultKind::Profile);
    assert_eq!(profile.row(0).unwrap().get::<i64>(0)?, 1);
    assert!(profile.plan().is_some_and(|plan| plan.is_profile()));

    let function = ScalarFunction::new(
        "plus_one",
        [LogicalType::Int64],
        LogicalType::Int64,
        |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() + 1)),
    )
    .with_null_policy(NullPolicy::Propagate);
    assert_eq!(function.name(), "plus_one");
    assert_eq!(function.parameter_types(), [LogicalType::Int64]);
    assert_eq!(function.result_type(), &LogicalType::Int64);
    assert_eq!(function.null_policy(), NullPolicy::Propagate);
    connection.register_scalar_function(function)?;
    assert_eq!(
        connection
            .execute("RETURN plus_one(41)")?
            .row(0)
            .unwrap()
            .get::<i64>(0)?,
        42
    );
    assert!(connection.unregister_scalar_function("PLUS_ONE")?);

    Ok(())
}

fn integer_pairs(result: &koko::QueryResult) -> Result<Vec<(i64, i64)>> {
    result
        .into_iter()
        .map(|row| Ok((row.get::<i64>(0)?, row.get::<i64>(1)?)))
        .collect()
}

#[test]
fn topological_levels_rebinds_prepared_snapshots_and_uses_public_results() -> Result<()> {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")?;
    connection.execute("CREATE REL TABLE E(FROM N TO N)")?;
    connection.execute("CREATE (:N {id: 0}), (:N {id: 1})")?;

    let mut prepared = connection.prepare(
        "CALL topological_levels(['N'], ['E']) YIELD node, level \
         RETURN node.id AS id, level ORDER BY id",
    )?;
    assert_eq!(prepared.columns()[0].name(), "id");
    assert_eq!(prepared.columns()[0].logical_type(), &LogicalType::Int64);
    assert_eq!(prepared.columns()[1].logical_type(), &LogicalType::Int64);
    assert_eq!(integer_pairs(&prepared.execute()?)?, [(0, 0), (1, 0)]);

    connection.execute("MATCH (a:N), (b:N) WHERE a.id = 0 AND b.id = 1 CREATE (a)-[:E]->(b)")?;
    let ranked = prepared.execute()?;
    assert_eq!(integer_pairs(&ranked)?, [(0, 0), (1, 1)]);
    let batches = ranked.to_arrow_record_batches()?;
    assert_eq!(
        batches.iter().map(|batch| batch.num_rows()).sum::<usize>(),
        2
    );

    let parameterized = connection.execute_with(
        "CALL topological_levels($nodes, $relationships) YIELD node, level \
         RETURN node.id AS id, level ORDER BY id",
        params! {
            "nodes" => Value::List(vec![Value::String("N".to_string())]),
            "relationships" => Value::List(vec![Value::String("E".to_string())]),
        },
    )?;
    assert_eq!(integer_pairs(&parameterized)?, [(0, 0), (1, 1)]);

    connection.set_max_threads(1)?;
    let serial = integer_pairs(&prepared.execute()?)?;
    connection.set_max_threads(4)?;
    assert_eq!(integer_pairs(&prepared.execute()?)?, serial);

    connection.execute(
        "UNWIND range(0, 10000) AS ignored \
         MATCH (a:N), (b:N) WHERE a.id = 0 AND b.id = 1 CREATE (a)-[:E]->(b)",
    )?;

    connection.set_query_timeout(Some(std::time::Duration::from_nanos(1)))?;
    let error = prepared.execute().unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    connection.set_query_timeout(None)?;
    assert_eq!(integer_pairs(&prepared.execute()?)?, [(0, 0), (1, 1)]);

    Ok(())
}

fn page_rank_pairs(result: &koko::QueryResult) -> Result<Vec<(i64, f64)>> {
    result
        .into_iter()
        .map(|row| Ok((row.get::<i64>(0)?, row.get::<f64>(1)?)))
        .collect()
}

fn assert_page_rank_pairs(actual: &[(i64, f64)], expected: &[(i64, f64)], tolerance: f64) {
    assert_eq!(actual.len(), expected.len());
    for ((actual_id, actual_score), (expected_id, expected_score)) in actual.iter().zip(expected) {
        assert_eq!(actual_id, expected_id);
        assert!(
            (actual_score - expected_score).abs() <= tolerance,
            "expected score {expected_score:.16}, got {actual_score:.16}"
        );
    }
}

#[test]
fn page_rank_prepared_options_rebind_and_reject_nonfinite_values() -> Result<()> {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")?;
    connection.execute("CREATE REL TABLE E(FROM N TO N)")?;
    connection.execute("CREATE (:N {id: 0}), (:N {id: 1})")?;

    let mut prepared = connection.prepare(
        "CALL page_rank(['N'], ['E'], $damping, $tolerance, $iterations, $normalize) \
         YIELD node, score RETURN node.id AS id, score ORDER BY id",
    )?;
    assert_eq!(prepared.columns()[0].logical_type(), &LogicalType::Int64);
    assert_eq!(prepared.columns()[1].logical_type(), &LogicalType::Double);

    let options = || {
        params! {
            "damping" => 0.5,
            "tolerance" => 0.0,
            "iterations" => 3,
            "normalize" => true,
        }
    };
    let isolated = page_rank_pairs(&prepared.execute_with(options())?)?;
    assert_page_rank_pairs(&isolated, &[(0, 0.5), (1, 0.5)], 0.0);

    connection.execute("MATCH (a:N), (b:N) WHERE a.id = 0 AND b.id = 1 CREATE (a)-[:E]->(b)")?;
    let linked = page_rank_pairs(&prepared.execute_with(options())?)?;
    assert_page_rank_pairs(&linked, &[(0, 0.40625), (1, 0.59375)], 1e-15);

    let error = prepared
        .execute_with(params! {
            "damping" => f64::NAN,
            "tolerance" => 0.0,
            "iterations" => 3,
            "normalize" => true,
        })
        .unwrap_err();
    assert_eq!(
        error,
        Error::Binder("PageRank damping factor must be finite and in [0, 1).".to_string())
    );
    Ok(())
}
