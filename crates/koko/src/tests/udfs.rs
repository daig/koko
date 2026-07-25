use super::*;

#[test]
fn im5_native_scalar_udfs_are_typed_local_and_prepared_generation_safe() {
    let database = Database::in_memory();
    let connection = database.connect();
    let peer = database.connect();
    connection
        .register_scalar_function(
            "plus_ten",
            vec![LogicalType::Int64],
            LogicalType::Int64,
            ScalarUdfNullPolicy::Propagate,
            |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() + 10)),
        )
        .unwrap();
    assert_eq!(
        connection
            .query("RETURN plus_ten(7)")
            .unwrap()
            .to_result_strings(),
        vec!["17"]
    );
    assert!(peer.query("RETURN plus_ten(7)").is_err());
    assert_eq!(
        connection
            .query("RETURN plus_ten(NULL)")
            .unwrap()
            .to_result_strings(),
        vec![""]
    );
    assert!(
        connection
            .query("RETURN plus_ten('bad')")
            .unwrap_err()
            .to_string()
            .contains("expected INT64")
    );
    assert!(
        connection
            .query("RETURN plus_ten(1, 2)")
            .unwrap_err()
            .to_string()
            .contains("expects 1 arguments")
    );
    assert!(
        connection
            .register_scalar_function(
                "PLUS_TEN",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                ScalarUdfNullPolicy::Propagate,
                |_| Ok(Value::Int64(0)),
            )
            .is_err()
    );
    connection
        .register_scalar_function(
            "add_two",
            vec![LogicalType::Int64, LogicalType::Int64],
            LogicalType::Int64,
            ScalarUdfNullPolicy::Propagate,
            |arguments| {
                Ok(Value::Int64(
                    arguments[0].as_i64().unwrap() + arguments[1].as_i64().unwrap(),
                ))
            },
        )
        .unwrap();
    assert_eq!(
        connection
            .query("RETURN add_two(2, 3)")
            .unwrap()
            .to_result_strings(),
        vec!["5"]
    );
    connection
        .register_scalar_function(
            "echo_int_list",
            vec![LogicalType::List(Box::new(LogicalType::Int64))],
            LogicalType::List(Box::new(LogicalType::Int64)),
            ScalarUdfNullPolicy::Propagate,
            |arguments| Ok(arguments[0].clone()),
        )
        .unwrap();
    assert_eq!(
        connection
            .query("RETURN echo_int_list([1, 2])")
            .unwrap()
            .to_result_strings(),
        vec!["[1,2]"]
    );

    assert!(
        connection
            .register_scalar_function(
                "abs",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                ScalarUdfNullPolicy::Propagate,
                |_| Ok(Value::Int64(0)),
            )
            .unwrap_err()
            .to_string()
            .contains("built-in")
    );
    connection.query("CREATE MACRO macro_only(x) AS x").unwrap();
    assert!(
        connection
            .register_scalar_function(
                "macro_only",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                ScalarUdfNullPolicy::Propagate,
                |arguments| Ok(arguments[0].clone()),
            )
            .unwrap_err()
            .to_string()
            .contains("macro")
    );
    connection
        .register_scalar_function(
            "native_only",
            vec![LogicalType::Int64],
            LogicalType::Int64,
            ScalarUdfNullPolicy::Propagate,
            |arguments| Ok(arguments[0].clone()),
        )
        .unwrap();
    assert!(
        connection
            .query("CREATE MACRO native_only(x) AS x")
            .is_err()
    );

    connection.query("CREATE GRAPH udf_any ANY").unwrap();
    connection.query("USE GRAPH udf_any").unwrap();
    let any_prepared = connection.prepare("RETURN plus_ten(3)").unwrap();
    assert_eq!(
        any_prepared.execute(&[]).unwrap().to_result_strings(),
        vec!["13"]
    );
    assert_eq!(
        any_prepared.result_schema()[0].logical_type(),
        &LogicalType::Int64
    );
    connection.query("USE GRAPH main").unwrap();

    let prepared = connection.prepare("RETURN plus_ten($value)").unwrap();
    assert_eq!(
        prepared
            .execute(&[("value", Value::Int64(2))])
            .unwrap()
            .to_result_strings(),
        vec!["12"]
    );
    assert!(connection.remove_scalar_function("PLUS_TEN").unwrap());
    assert!(prepared.execute(&[("value", Value::Int64(2))]).is_err());
    connection
        .register_scalar_function(
            "plus_ten",
            vec![LogicalType::Int64],
            LogicalType::Int64,
            ScalarUdfNullPolicy::Propagate,
            |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() + 20)),
        )
        .unwrap();
    assert_eq!(
        prepared
            .execute(&[("value", Value::Int64(2))])
            .unwrap()
            .to_result_strings(),
        vec!["22"]
    );
}

#[test]
fn im5_native_scalar_udf_null_error_panic_and_statement_rollback_contracts() {
    let connection = Database::in_memory().connect();
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    connection
        .register_scalar_function(
            "sees_null",
            vec![LogicalType::Any],
            LogicalType::Bool,
            ScalarUdfNullPolicy::Call,
            move |arguments| {
                callback_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Bool(arguments[0].is_null()))
            },
        )
        .unwrap();
    assert_eq!(
        connection
            .query("RETURN sees_null(NULL)")
            .unwrap()
            .to_result_strings(),
        vec!["True"]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    connection
        .query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .register_scalar_function(
            "fails",
            Vec::new(),
            LogicalType::Int64,
            ScalarUdfNullPolicy::Call,
            |_| Err(Error::runtime("callback failed")),
        )
        .unwrap();
    assert!(
        connection
            .query("CREATE (:P {id: 1}) RETURN fails()")
            .unwrap_err()
            .to_string()
            .contains("callback failed")
    );
    assert_eq!(
        connection
            .query("MATCH (n:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );
    connection.query("BEGIN TRANSACTION").unwrap();
    assert!(
        connection
            .query("CREATE (:P {id: 2}) RETURN fails()")
            .unwrap_err()
            .to_string()
            .contains("callback failed")
    );
    connection.query("CREATE (:P {id: 3})").unwrap();
    assert_eq!(
        connection
            .query("MATCH (n:P) RETURN n.id")
            .unwrap()
            .to_result_strings(),
        vec!["3"]
    );

    connection
        .register_scalar_function(
            "panics",
            Vec::new(),
            LogicalType::Int64,
            ScalarUdfNullPolicy::Call,
            |_| panic!("host panic"),
        )
        .unwrap();
    assert!(
        connection
            .query("RETURN panics()")
            .unwrap_err()
            .to_string()
            .contains("panicked: host panic")
    );
    assert_eq!(
        connection.query("RETURN 1").unwrap().to_result_strings(),
        vec!["1"]
    );

    connection
        .register_scalar_function(
            "wrong_type",
            Vec::new(),
            LogicalType::Int64,
            ScalarUdfNullPolicy::Call,
            |_| Ok(Value::String("wrong".to_string())),
        )
        .unwrap();
    assert!(
        connection
            .query("RETURN wrong_type()")
            .unwrap_err()
            .to_string()
            .contains("returned STRING, expected INT64")
    );
}

#[test]
fn im5_native_scalar_udf_running_queries_retain_callbacks_and_observe_deadlines() {
    let connection = Database::in_memory().connect();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    connection
        .register_scalar_function(
            "held",
            vec![LogicalType::Int64],
            LogicalType::Int64,
            ScalarUdfNullPolicy::Propagate,
            move |arguments| {
                if callback_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    callback_barrier.wait();
                    callback_barrier.wait();
                }
                Ok(Value::Int64(arguments[0].as_i64().unwrap() + 1))
            },
        )
        .unwrap();
    std::thread::scope(|scope| {
        let query = scope.spawn(|| connection.query("UNWIND [1, 2] AS x RETURN held(x)"));
        barrier.wait();
        assert!(connection.remove_scalar_function("held").unwrap());
        barrier.wait();
        assert_eq!(
            query.join().unwrap().unwrap().to_result_strings(),
            vec!["2", "3"]
        );
    });

    connection
        .register_scalar_function(
            "slow",
            Vec::new(),
            LogicalType::Int64,
            ScalarUdfNullPolicy::Call,
            |_| {
                std::thread::sleep(Duration::from_millis(20));
                Ok(Value::Int64(1))
            },
        )
        .unwrap();
    connection.set_query_timeout_ms(1).unwrap();
    let error = connection.query("RETURN slow()").unwrap_err().to_string();
    assert!(
        error.to_ascii_lowercase().contains("interrupted")
            || error.to_ascii_lowercase().contains("deadline"),
        "{error}"
    );
    connection.set_query_timeout_ms(0).unwrap();
    assert_eq!(
        connection.query("RETURN 1").unwrap().to_result_strings(),
        vec!["1"]
    );
}
#[test]
fn im5_native_scalar_udf_results_obey_the_database_memory_limit() {
    let result_bytes =
        koko_common::ColumnData::allocation_bytes(LogicalType::String.physical_type());
    let memory_limit = result_bytes + 8192;
    let database = Database::in_memory_with_config(
        DatabaseConfig::new()
            .with_memory_limit(memory_limit)
            .unwrap(),
    )
    .unwrap();
    let connection = database.connect();
    let oversized = "x".repeat((memory_limit + 4096) as usize);
    connection
        .register_scalar_function(
            "oversized",
            Vec::new(),
            LogicalType::String,
            ScalarUdfNullPolicy::Call,
            move |_| Ok(Value::String(oversized.clone())),
        )
        .unwrap();
    assert!(matches!(
        connection.query("RETURN oversized()").unwrap_err(),
        Error::BufferManager
    ));
    assert_eq!(database.memory_usage().current, 0);
    assert_eq!(
        connection.query("RETURN 1").unwrap().to_result_strings(),
        vec!["1"]
    );
}
