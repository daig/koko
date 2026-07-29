use super::*;

#[test]
fn im5_native_scalar_udfs_are_typed_local_and_prepared_generation_safe() {
    let database = Database::new();
    let connection = database.connect();
    let peer = database.connect();
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "plus_ten",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() + 10)),
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("RETURN plus_ten(7)")
            .unwrap()
            .rendered_rows(),
        vec!["17"]
    );
    assert!(peer.execute("RETURN plus_ten(7)").is_err());
    assert_eq!(
        connection
            .execute("RETURN plus_ten(NULL)")
            .unwrap()
            .rendered_rows(),
        vec![""]
    );
    assert!(
        connection
            .execute("RETURN plus_ten('bad')")
            .unwrap_err()
            .to_string()
            .contains("expected INT64")
    );
    assert!(
        connection
            .execute("RETURN plus_ten(1, 2)")
            .unwrap_err()
            .to_string()
            .contains("expects 1 arguments")
    );
    assert!(
        connection
            .register_scalar_function(
                ScalarFunction::new(
                    "PLUS_TEN",
                    vec![LogicalType::Int64],
                    LogicalType::Int64,
                    |_| Ok(Value::Int64(0)),
                )
                .with_null_policy(NullPolicy::Propagate),
            )
            .is_err()
    );
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "add_two",
                vec![LogicalType::Int64, LogicalType::Int64],
                LogicalType::Int64,
                |arguments| {
                    Ok(Value::Int64(
                        arguments[0].as_i64().unwrap() + arguments[1].as_i64().unwrap(),
                    ))
                },
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("RETURN add_two(2, 3)")
            .unwrap()
            .rendered_rows(),
        vec!["5"]
    );
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "echo_int_list",
                vec![LogicalType::List(Box::new(LogicalType::Int64))],
                LogicalType::List(Box::new(LogicalType::Int64)),
                |arguments| Ok(arguments[0].clone()),
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("RETURN echo_int_list([1, 2])")
            .unwrap()
            .rendered_rows(),
        vec!["[1,2]"]
    );

    assert!(
        connection
            .register_scalar_function(
                ScalarFunction::new("abs", vec![LogicalType::Int64], LogicalType::Int64, |_| Ok(
                    Value::Int64(0)
                ),)
                .with_null_policy(NullPolicy::Propagate),
            )
            .unwrap_err()
            .to_string()
            .contains("built-in")
    );
    connection
        .execute("CREATE MACRO macro_only(x) AS x")
        .unwrap();
    assert!(
        connection
            .register_scalar_function(
                ScalarFunction::new(
                    "macro_only",
                    vec![LogicalType::Int64],
                    LogicalType::Int64,
                    |arguments| Ok(arguments[0].clone()),
                )
                .with_null_policy(NullPolicy::Propagate),
            )
            .unwrap_err()
            .to_string()
            .contains("macro")
    );
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "native_only",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                |arguments| Ok(arguments[0].clone()),
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    assert!(
        connection
            .execute("CREATE MACRO native_only(x) AS x")
            .is_err()
    );

    connection.execute("CREATE GRAPH udf_any ANY").unwrap();
    connection.execute("USE GRAPH udf_any").unwrap();
    let mut any_prepared = connection.prepare("RETURN plus_ten(3)").unwrap();
    assert_eq!(any_prepared.execute().unwrap().rendered_rows(), vec!["13"]);
    assert_eq!(
        any_prepared.columns()[0].logical_type(),
        &LogicalType::Int64
    );
    connection.execute("USE GRAPH main").unwrap();

    let mut prepared = connection.prepare("RETURN plus_ten($value)").unwrap();
    assert_eq!(
        prepared
            .execute_with(params! { "value" => 2 })
            .unwrap()
            .rendered_rows(),
        vec!["12"]
    );
    assert!(connection.unregister_scalar_function("PLUS_TEN").unwrap());
    assert!(prepared.execute_with(params! { "value" => 2 }).is_err());
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "plus_ten",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() + 20)),
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    assert_eq!(
        prepared
            .execute_with(params! { "value" => 2 })
            .unwrap()
            .rendered_rows(),
        vec!["22"]
    );
}

#[test]
fn im5_native_scalar_udf_null_error_panic_and_statement_rollback_contracts() {
    let connection = Database::new().connect();
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "sees_null",
                vec![LogicalType::Any],
                LogicalType::Bool,
                move |arguments| {
                    callback_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(Value::Bool(arguments[0].is_null()))
                },
            )
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("RETURN sees_null(NULL)")
            .unwrap()
            .rendered_rows(),
        vec!["True"]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .register_scalar_function(
            ScalarFunction::new("fails", Vec::new(), LogicalType::Int64, |_| {
                Err(Error::runtime("callback failed"))
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    assert!(
        connection
            .execute("CREATE (:P {id: 1}) RETURN fails()")
            .unwrap_err()
            .to_string()
            .contains("callback failed")
    );
    assert_eq!(
        connection
            .execute("MATCH (n:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    connection.execute("BEGIN TRANSACTION").unwrap();
    assert!(
        connection
            .execute("CREATE (:P {id: 2}) RETURN fails()")
            .unwrap_err()
            .to_string()
            .contains("callback failed")
    );
    connection.execute("CREATE (:P {id: 3})").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:P) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["3"]
    );

    connection
        .register_scalar_function(
            ScalarFunction::new("panics", Vec::new(), LogicalType::Int64, |_| {
                panic!("host panic")
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    assert!(
        connection
            .execute("RETURN panics()")
            .unwrap_err()
            .to_string()
            .contains("panicked: host panic")
    );
    assert_eq!(
        connection.execute("RETURN 1").unwrap().rendered_rows(),
        vec!["1"]
    );

    connection
        .register_scalar_function(
            ScalarFunction::new("wrong_type", Vec::new(), LogicalType::Int64, |_| {
                Ok(Value::String("wrong".to_string()))
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    assert!(
        connection
            .execute("RETURN wrong_type()")
            .unwrap_err()
            .to_string()
            .contains("returned STRING, expected INT64")
    );
}

#[test]
fn im5_native_scalar_udf_running_queries_retain_callbacks_and_observe_deadlines() {
    let connection = Database::new().connect();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    connection
        .register_scalar_function(
            ScalarFunction::new(
                "held",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                move |arguments| {
                    if callback_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                        callback_barrier.wait();
                        callback_barrier.wait();
                    }
                    Ok(Value::Int64(arguments[0].as_i64().unwrap() + 1))
                },
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    std::thread::scope(|scope| {
        let query = scope.spawn(|| connection.execute("UNWIND [1, 2] AS x RETURN held(x)"));
        barrier.wait();
        assert!(connection.unregister_scalar_function("held").unwrap());
        barrier.wait();
        assert_eq!(
            query.join().unwrap().unwrap().rendered_rows(),
            vec!["2", "3"]
        );
    });

    connection
        .register_scalar_function(
            ScalarFunction::new("slow", Vec::new(), LogicalType::Int64, |_| {
                std::thread::sleep(Duration::from_millis(20));
                Ok(Value::Int64(1))
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    connection
        .set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let error = connection.execute("RETURN slow()").unwrap_err().to_string();
    assert!(
        error.to_ascii_lowercase().contains("interrupted")
            || error.to_ascii_lowercase().contains("deadline"),
        "{error}"
    );
    connection.set_query_timeout(None).unwrap();
    assert_eq!(
        connection.execute("RETURN 1").unwrap().rendered_rows(),
        vec!["1"]
    );
}
#[test]
fn im5_native_scalar_udf_results_obey_the_database_memory_limit() {
    let result_bytes =
        koko_common::ColumnData::allocation_bytes(LogicalType::String.physical_type());
    let memory_limit = result_bytes + 8192;
    let database = Database::with_config(
        DatabaseConfig::new()
            .with_memory_limit(memory_limit)
            .unwrap(),
    );
    let connection = database.connect();
    let oversized = "x".repeat((memory_limit + 4096) as usize);
    connection
        .register_scalar_function(
            ScalarFunction::new("oversized", Vec::new(), LogicalType::String, move |_| {
                Ok(Value::String(oversized.clone()))
            })
            .with_null_policy(NullPolicy::Call),
        )
        .unwrap();
    assert!(matches!(
        connection.execute("RETURN oversized()").unwrap_err(),
        Error::BufferManager
    ));
    assert_eq!(database.memory_usage().current, 0);
    assert_eq!(
        connection.execute("RETURN 1").unwrap().rendered_rows(),
        vec!["1"]
    );
}
