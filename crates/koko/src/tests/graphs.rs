use super::*;

#[test]
fn im5_typed_graphs_route_connections_transactions_and_prepared_queries() {
    let database = Database::new();
    let first = database.connect();
    let second = database.connect();

    first.execute("CREATE GRAPH analytics").unwrap();
    first.execute("USE GRAPH analytics").unwrap();
    first
        .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    first.execute("CREATE (:Person {id: 1})").unwrap();

    second
        .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
        .unwrap();
    second.execute("CREATE (:Person {id: 2})").unwrap();
    assert_eq!(
        first
            .execute("MATCH (n:Person) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        second
            .execute("MATCH (n:Person) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );

    let mut prepared = first.prepare("MATCH (n:Person) RETURN n.id").unwrap();
    first.execute("USE GRAPH main").unwrap();
    assert_eq!(prepared.execute().unwrap().rendered_rows(), vec!["2"]);

    first.execute("USE GRAPH analytics").unwrap();
    first.execute("BEGIN TRANSACTION").unwrap();
    let error = first.execute("USE GRAPH main").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Cannot switch graphs while a transaction is active."
    );
    assert_eq!(
        first
            .execute("MATCH (n:Person) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    first.execute("ROLLBACK").unwrap();
    assert_eq!(
        first
            .execute("MATCH (n:Person) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    first.execute("USE GRAPH main").unwrap();
    assert_eq!(
        first
            .execute("MATCH (n:Person) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
}

#[test]
fn im5_dropped_graphs_reset_connections_and_graph_ddl_is_not_rolled_back() {
    let database = Database::new();
    let first = database.connect();
    let second = database.connect();

    first.execute("BEGIN TRANSACTION").unwrap();
    first.execute("CREATE GRAPH transient").unwrap();
    first.execute("ROLLBACK").unwrap();
    first.execute("USE GRAPH transient").unwrap();
    first
        .execute("CREATE NODE TABLE Item(id INT64, PRIMARY KEY(id))")
        .unwrap();
    first.execute("CREATE (:Item {id: 7})").unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    first
        .register_scalar_function(
            ScalarFunction::new(
                "hold_graph",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                move |arguments| {
                    callback_barrier.wait();
                    callback_barrier.wait();
                    Ok(arguments[0].clone())
                },
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| first.execute("MATCH (n:Item) RETURN hold_graph(n.id)"));
        barrier.wait();
        second.execute("DROP GRAPH transient").unwrap();
        barrier.wait();
        assert_eq!(running.join().unwrap().unwrap().rendered_rows(), vec!["7"]);
    });

    let missing = first.execute("RETURN 1").unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Binder exception: No graph named transient."
    );
    assert_eq!(
        first.execute("RETURN 1").unwrap().rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        second
            .execute("CALL show_tables() RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn im5_any_graph_transactions_memory_and_typed_isolation() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE NODE TABLE User(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.execute("CREATE (:User {id: 1})").unwrap();
    let typed_usage = database.memory_usage().current;

    connection.execute("CREATE GRAPH dynamic ANY").unwrap();
    connection.execute("USE GRAPH dynamic").unwrap();
    connection.execute("BEGIN TRANSACTION").unwrap();
    connection
        .execute("CREATE (:User:Admin {name: 'rolled back', score: 1})")
        .unwrap();
    connection.execute("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );

    let payload = "x".repeat(16 * 1024);
    connection
        .execute(&format!(
            "CREATE (:User:Admin {{name: 'kept', payload: '{payload}'}})"
        ))
        .unwrap();
    assert!(database.memory_usage().current > typed_usage);
    assert_eq!(
        connection
            .execute("MATCH (n:User:Admin) WHERE n.name = 'kept' RETURN n.name")
            .unwrap()
            .rendered_rows(),
        vec!["kept"]
    );

    connection.execute("USE GRAPH main").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:User) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    connection.execute("DROP GRAPH dynamic").unwrap();
    assert_eq!(database.memory_usage().current, typed_usage);
}

#[test]
fn im5_any_graph_normal_pipeline_is_ordered_and_complete() {
    let connection = Database::new().connect();
    connection.execute("CREATE GRAPH dynamic ANY").unwrap();
    connection.execute("USE GRAPH dynamic").unwrap();
    assert_eq!(
        connection
            .execute("CALL show_tables() RETURN *")
            .unwrap()
            .rendered_rows(),
        vec![
            "0|_nodes|NODE|dynamic(graph)|",
            "2|_edges|REL|dynamic(graph)|",
        ]
    );
    connection
        .execute(
            "CREATE (a:N:Extra {b: 2, name: 'B', a: 'x'}), \
            (b:N {name: 'A'}), (c:N {name: 'A'}), \
            (a)-[:R {weight: 3}]->(b)",
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.name ORDER BY n.name")
            .unwrap()
            .rendered_rows(),
        vec!["A", "A", "B"]
    );
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN DISTINCT n.name ORDER BY n.name SKIP 1 LIMIT 1",)
            .unwrap()
            .rendered_rows(),
        vec!["B"]
    );
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.name ORDER BY n.name DESC SKIP 1 LIMIT 2")
            .unwrap()
            .rendered_rows(),
        vec!["A", "A"]
    );
    assert_eq!(
        connection
            .execute("OPTIONAL MATCH (n:Missing) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (n:N) WITH n WHERE n.name = 'B' RETURN n.name \
                 UNION ALL MATCH (n:N) WHERE n.name = 'B' RETURN n.name",
            )
            .unwrap()
            .rendered_rows(),
        vec!["B", "B"]
    );
    assert_eq!(
        connection
            .execute("MATCH (n:N:Extra) WHERE n.name = 'B' RETURN n")
            .unwrap()
            .rendered_rows(),
        vec![
            "{_ID: 0:0, _LABEL: _nodes, id: 0, label: [N,Extra], \
             data: {\"a\":\"x\",\"name\":\"B\",\"b\":2}}",
        ]
    );
    assert_eq!(
        connection
            .execute("MATCH (a:N {name: 'B'})-[r:R]->(b:N) RETURN r.weight, b.name")
            .unwrap()
            .rendered_rows(),
        vec!["3|A"]
    );
    assert_eq!(
        connection
            .execute("MATCH (b:N {name: 'A'})<-[r:R]-(a:N) RETURN r.weight, a.name")
            .unwrap()
            .rendered_rows(),
        vec!["3|B"]
    );
    assert_eq!(
        connection
            .execute("MATCH (a:N {name: 'B'})-[r:R]-(b:N) RETURN r.weight, b.name")
            .unwrap()
            .rendered_rows(),
        vec!["3|A"]
    );
    assert_eq!(
        connection
            .execute("MATCH (a:N {name: 'B'})-[:R*1..2]->(b:N) RETURN b.name")
            .unwrap()
            .rendered_rows(),
        vec!["A"]
    );
    connection
        .execute("MATCH (n:N {name: 'B'}) SET n.extra = 7, n = {more: 8}")
        .unwrap();
    assert_eq!(
        connection
            .execute(
                "MATCH (n:N {name: 'B'}) SET n.extra = NULL \
                 RETURN n.extra IS NULL, n.more",
            )
            .unwrap()
            .rendered_rows(),
        vec!["True|8"]
    );
    connection
        .execute("MERGE (n:N {name: 'C'}) ON CREATE SET n.created = 1")
        .unwrap();
    connection
        .execute("MERGE (n:N {name: 'C'}) ON MATCH SET n.matched = 2")
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:N {name: 'C'}) RETURN n.created, n.matched")
            .unwrap()
            .rendered_rows(),
        vec!["1|2"]
    );
    connection
        .execute("MATCH (n:N {name: 'B'}) DETACH DELETE n")
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH ()-[r:R]->() RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn im5_any_graph_prepared_errors_and_rollback_use_retained_contracts() {
    let connection = Database::new().connect();
    connection.execute("CREATE GRAPH dynamic ANY").unwrap();
    connection.execute("USE GRAPH dynamic").unwrap();
    connection
        .execute("CREATE (:N {name: 'A', value: 1})")
        .unwrap();
    let mut prepared = connection
        .prepare("MATCH (n:N) WHERE n.name = $name RETURN n.value")
        .unwrap();
    assert_eq!(
        prepared
            .execute_with(params! { "name" => "A" })
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );

    connection
        .register_scalar_function(
            ScalarFunction::new(
                "fail_any",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                |_| Err(Error::runtime("native ANY failure")),
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    let udf_error = connection
        .execute("MATCH (n:N) SET n.value = 2 RETURN fail_any(n.value)")
        .unwrap_err()
        .to_string();
    assert!(udf_error.contains("native ANY failure"), "{udf_error}");
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.value")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        connection
            .execute("MATCH (n:N) SET n.runtime = 1 RETURN 1 % 0")
            .unwrap_err()
            .to_string(),
        "Runtime exception: Modulo by zero."
    );
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.runtime IS NULL")
            .unwrap()
            .rendered_rows(),
        vec!["True"]
    );
    assert!(
        connection
            .execute("MATCH (n:N) CREATE (:Copy {bad: n})")
            .unwrap_err()
            .to_string()
            .contains("Cannot convert")
    );
    assert_eq!(
        connection
            .execute("MATCH (n:Copy) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );

    connection.execute("BEGIN TRANSACTION").unwrap();
    connection
        .execute("MATCH (n:N) SET n.committed = 3")
        .unwrap();
    connection.execute("COMMIT").unwrap();
    connection.execute("BEGIN TRANSACTION").unwrap();
    connection
        .execute("MATCH (n:N) SET n.rolled_back = 4")
        .unwrap();
    connection.execute("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.committed, n.rolled_back IS NULL")
            .unwrap()
            .rendered_rows(),
        vec!["3|True"]
    );
}

#[test]
fn im5_any_graph_snapshots_deadlines_and_low_memory_are_safe() {
    let database = Database::new();
    let running = database.connect();
    let peer = database.connect();
    running.execute("CREATE GRAPH transient ANY").unwrap();
    running.execute("USE GRAPH transient").unwrap();
    running
        .execute("UNWIND range(0, 32) AS value CREATE (:N {value: value})")
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    running
        .register_scalar_function(
            ScalarFunction::new(
                "hold_any",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                move |arguments| {
                    callback_barrier.wait();
                    callback_barrier.wait();
                    Ok(arguments[0].clone())
                },
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    std::thread::scope(|scope| {
        let query = scope.spawn(|| running.execute("MATCH (n:N) RETURN hold_any(n.value) LIMIT 1"));
        barrier.wait();
        peer.execute("DROP GRAPH transient").unwrap();
        barrier.wait();
        assert_eq!(query.join().unwrap().unwrap().rendered_rows(), vec!["0"]);
    });

    peer.execute("CREATE GRAPH deadline ANY").unwrap();
    peer.execute("USE GRAPH deadline").unwrap();
    peer.execute("CREATE (:N {value: 1})").unwrap();
    peer.set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let interrupted = peer
        .execute(
            "MATCH (n:N) UNWIND range(0, 1000000) AS value \
     RETURN count(*)",
        )
        .unwrap_err()
        .to_string()
        .to_ascii_lowercase();
    assert!(
        interrupted.contains("interrupt") || interrupted.contains("deadline"),
        "{interrupted}"
    );
    peer.set_query_timeout(None).unwrap();
    assert_eq!(
        peer.execute("MATCH (n:N) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );

    let constrained =
        Database::with_config(DatabaseConfig::new().with_memory_limit(64 * 1024).unwrap());
    let constrained_connection = constrained.connect();
    constrained_connection
        .execute("CREATE GRAPH constrained ANY")
        .unwrap();
    constrained_connection
        .execute("USE GRAPH constrained")
        .unwrap();
    assert!(matches!(
        constrained_connection
            .execute("CREATE (:N {payload: 'value'})")
            .unwrap_err(),
        Error::BufferManager
    ));
    assert_eq!(
        constrained_connection
            .execute("MATCH (n) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn im5_indexes_are_graph_scoped_and_transactional() {
    let connection = Database::new().connect();
    connection
        .execute("CALL enable_default_hash_index=false")
        .unwrap();

    for graph in ["first", "second"] {
        connection
            .execute(&format!("CREATE GRAPH {graph}"))
            .unwrap();
        connection.execute(&format!("USE GRAPH {graph}")).unwrap();
        connection
            .execute("CREATE NODE TABLE Person(id INT64, PRIMARY KEY(id))")
            .unwrap();
    }

    connection.execute("USE GRAPH first").unwrap();
    connection.execute("BEGIN TRANSACTION").unwrap();
    connection
        .execute("CREATE HASH INDEX person_pk FOR (p:Person) ON (p.id)")
        .unwrap();
    connection.execute("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .execute("CALL show_indexes() RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    connection
        .execute("CREATE HASH INDEX person_pk FOR (p:Person) ON (p.id)")
        .unwrap();
    connection.execute("CREATE (:Person {id: 1})").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (p:Person) WHERE p.id = 1 RETURN p.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );

    connection.execute("USE GRAPH second").unwrap();
    connection
        .execute("CREATE ART INDEX person_pk FOR (p:Person) ON (p.id)")
        .unwrap();
    assert_eq!(
        connection
            .execute("CALL show_indexes() RETURN index_type")
            .unwrap()
            .rendered_rows(),
        vec!["ART"]
    );
    connection.execute("USE GRAPH first").unwrap();
    connection.execute("DROP INDEX person_pk").unwrap();
    assert_eq!(
        connection
            .execute("CALL show_indexes() RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    connection.execute("USE GRAPH second").unwrap();
    assert_eq!(
        connection
            .execute("CALL show_indexes() RETURN index_type")
            .unwrap()
            .rendered_rows(),
        vec!["ART"]
    );
}

#[test]
fn im5_icebug_repeated_scans_cancellation_and_low_memory_are_leak_free() {
    let root = interchange_temp_path("icebug-focused");
    std::fs::create_dir_all(&root).unwrap();
    let fields = vec![
        koko_loader::ParquetField::new("id", LogicalType::Int64, false),
        koko_loader::ParquetField::new("name", LogicalType::String, true),
    ];
    let schema = koko_loader::ParquetSchema::new(fields).unwrap();
    let mut writer = koko_loader::ParquetFileWriter::create(
        root.join("nodes_person.parquet"),
        schema,
        koko_loader::ParquetWriterOptions::default(),
    )
    .unwrap();
    let types = [LogicalType::Int64, LogicalType::String];
    for start in (0..20_000).step_by(VECTOR_CAPACITY) {
        let len = VECTOR_CAPACITY.min(20_000 - start);
        let mut chunk = DataChunk::new(&types);
        for position in 0..len {
            let id = (start + position) as i64;
            chunk.columns[0].set_value_owned(position, Value::Int64(id));
            chunk.columns[1].set_value_owned(position, Value::String(format!("person-{id}")));
        }
        chunk.set_flat(len);
        writer.write_chunk(&chunk).unwrap();
    }
    assert_eq!(writer.finish().unwrap(), 20_000);
    let storage = root.to_string_lossy().replace('\\', "/");
    let ddl = format!(
        "CREATE NODE TABLE person(id INT64, name STRING, PRIMARY KEY(id)) \
         WITH (storage = '{storage}', format = 'icebug-disk')"
    );

    let database = Database::new();
    let connection = database.connect();
    connection.execute(&ddl).unwrap();
    for _ in 0..2 {
        assert_eq!(
            connection
                .execute("MATCH (n:person) RETURN count(*)")
                .unwrap()
                .rendered_rows(),
            vec!["20000"]
        );
    }
    assert_eq!(
        connection
            .execute("MATCH (n:person) WHERE n.id = 12345 RETURN n.name")
            .unwrap()
            .rendered_rows(),
        vec!["person-12345"]
    );
    connection
        .set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let interrupted = connection
        .execute(
            "MATCH (n:person) UNWIND range(0, 100000) AS value \
     RETURN count(*)",
        )
        .unwrap_err()
        .to_string()
        .to_ascii_lowercase();
    assert!(
        interrupted.contains("interrupt") || interrupted.contains("deadline"),
        "{interrupted}"
    );
    connection.set_query_timeout(None).unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:person) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["20000"]
    );

    let constrained =
        Database::with_config(DatabaseConfig::new().with_memory_limit(64 * 1024).unwrap());
    let constrained_connection = constrained.connect();
    constrained_connection.execute(&ddl).unwrap();
    let baseline = constrained.memory_usage().current;
    assert!(matches!(
        constrained_connection
            .execute("MATCH (n:person) RETURN n.name")
            .unwrap_err(),
        Error::BufferManager
    ));
    assert_eq!(constrained.memory_usage().current, baseline);
    assert_eq!(
        constrained_connection
            .execute("RETURN 1")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn im5_icebug_relationship_scans_cover_directions_recursion_and_parallel_batches() {
    const ROWS: i64 = 5_000;
    let root = interchange_temp_path("icebug-relationships");
    std::fs::create_dir_all(&root).unwrap();
    let nodes: Vec<_> = (0..ROWS)
        .map(|id| vec![Value::Int64(id), Value::Int64(id % 10)])
        .collect();
    write_parquet_rows(
        &root.join("nodes_person.parquet"),
        vec![
            koko_loader::ParquetField::new("id", LogicalType::Int64, false),
            koko_loader::ParquetField::new("grp", LogicalType::Int64, true),
        ],
        &nodes,
    );
    let edges: Vec<_> = (0..ROWS)
        .map(|id| vec![Value::Int64((id + 1) % ROWS), Value::Int64(id)])
        .collect();
    write_parquet_rows(
        &root.join("indices_edge.parquet"),
        vec![
            koko_loader::ParquetField::new("nbr", LogicalType::Int64, false),
            koko_loader::ParquetField::new("weight", LogicalType::Int64, true),
        ],
        &edges,
    );
    let indptr: Vec<_> = (0..=ROWS)
        .map(|offset| vec![Value::Int64(offset)])
        .collect();
    write_parquet_rows(
        &root.join("indptr_edge.parquet"),
        vec![koko_loader::ParquetField::new(
            "offset",
            LogicalType::Int64,
            false,
        )],
        &indptr,
    );
    let flat_path = root.join("flat.parquet");
    let flat_edges: Vec<_> = (0..ROWS)
        .map(|id| {
            vec![
                Value::Int64(id),
                Value::Int64((id + 2) % ROWS),
                Value::Int64(id + 10),
            ]
        })
        .collect();
    write_parquet_rows(
        &flat_path,
        vec![
            koko_loader::ParquetField::new("source", LogicalType::Int64, false),
            koko_loader::ParquetField::new("target", LogicalType::Int64, false),
            koko_loader::ParquetField::new("weight", LogicalType::Int64, true),
        ],
        &flat_edges,
    );
    let storage = root.to_string_lossy().replace('\\', "/");
    let connection = Database::new().connect();
    connection
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, grp INT64, PRIMARY KEY(id)) \
         WITH (storage = '{storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    connection
        .execute(&format!(
            "CREATE REL TABLE edge(FROM person TO person, weight INT64) \
         WITH (storage = '{storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    let flat_storage = flat_path.to_string_lossy().replace('\\', "/");
    connection
        .execute(&format!(
            "CREATE REL TABLE flat(FROM person TO person, weight INT64) \
         WITH (storage = '{flat_storage}', format = 'icebug-disk')"
        ))
        .unwrap();

    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 2047})-[r:edge]->(b:person) \
                 RETURN b.id, r.weight",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2048|2047"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 2048})<-[r:edge]-(b:person) \
                 RETURN b.id, r.weight",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2047|2047"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 2047})-[r:flat]->(b:person) \
                 RETURN b.id, r.weight",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2049|2057"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 2049})<-[r:flat]-(b:person) \
                 RETURN b.id, r.weight",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2047|2057"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 1})-[r:edge]-(b:person) \
                 RETURN count(*)",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 0})-[e:edge*1..3 \
                 (r, _ | WHERE r.weight < 2)]->(b:person) \
                 RETURN b.id, length(e) ORDER BY b.id",
            )
            .unwrap()
            .rendered_rows(),
        vec!["1|1", "2|2"]
    );
    assert_eq!(
        connection
            .execute(
                "MATCH (a:person {id: 0})-[:edge]->(b:person)-[:edge]->(c:person) \
                 RETURN c.id",
            )
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    let rel = connection
        .execute(
            "MATCH (a:person {id: 0})-[r:edge]->(b:person) \
     RETURN r, a, b",
        )
        .unwrap()
        .rendered_rows();
    assert_eq!(rel.len(), 1);
    assert!(rel[0].contains("_LABEL: edge") && rel[0].contains("weight: 0"));

    let mut prepared = connection
        .prepare("MATCH (n:person) WHERE n.id = $id RETURN n.grp")
        .unwrap();
    assert_eq!(
        prepared
            .execute_with(params! { "id" => 4_097 })
            .unwrap()
            .rendered_rows(),
        vec!["7"]
    );
    for mutation in [
        "CREATE (:person {id: 6000})",
        "MATCH (n:person {id: 1}) SET n.grp = 99",
        "MATCH (n:person {id: 1}) DELETE n",
        "MATCH (n:person {id: 1}) CREATE (n)-[:edge {weight: 1}]->(n)",
    ] {
        let error = connection.execute(mutation).unwrap_err().to_string();
        assert!(error.contains("icebug-disk"), "{mutation}: {error}");
    }
    connection.execute("BEGIN TRANSACTION READ ONLY").unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:person) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["5000"]
    );
    connection.execute("COMMIT").unwrap();

    let aggregate = "MATCH (a:person)-[r:edge]->(b:person) RETURN sum(r.weight), count(*)";
    connection.set_max_threads(1).unwrap();
    let serial = connection.execute(aggregate).unwrap().rendered_rows();
    assert_eq!(serial, vec!["12497500|5000"]);
    connection.set_max_threads(4).unwrap();
    assert_eq!(
        connection.execute(aggregate).unwrap().rendered_rows(),
        serial
    );
    connection
        .set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let deadline = connection.execute(aggregate).unwrap_err().to_string();
    assert!(
        deadline.contains("Interrupted") || deadline.contains("deadline"),
        "{deadline}"
    );
    connection.set_query_timeout(None).unwrap();
    assert_eq!(
        connection.execute(aggregate).unwrap().rendered_rows(),
        serial
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn im5_icebug_rejects_schema_row_count_endpoint_and_csr_corruption() {
    let schema_root = interchange_temp_path("icebug-bad-schema");
    std::fs::create_dir_all(&schema_root).unwrap();
    write_parquet_rows(
        &schema_root.join("nodes_person.parquet"),
        vec![koko_loader::ParquetField::new(
            "id",
            LogicalType::String,
            false,
        )],
        &[vec![Value::String("bad".to_string())]],
    );
    let schema_storage = schema_root.to_string_lossy().replace('\\', "/");
    let schema_error = Database::new()
        .connect()
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, PRIMARY KEY(id)) \
         WITH (storage = '{schema_storage}', format = 'icebug-disk')"
        ))
        .unwrap_err()
        .to_string();
    assert!(
        schema_error.contains("column id has type STRING"),
        "{schema_error}"
    );

    let count_root = interchange_temp_path("icebug-bad-count");
    std::fs::create_dir_all(&count_root).unwrap();
    let count_path = count_root.join("nodes_person.parquet");
    let id_field = || {
        vec![koko_loader::ParquetField::new(
            "id",
            LogicalType::Int64,
            false,
        )]
    };
    write_parquet_rows(
        &count_path,
        id_field(),
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ],
    );
    let count_storage = count_root.to_string_lossy().replace('\\', "/");
    let count_connection = Database::new().connect();
    count_connection
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, PRIMARY KEY(id)) \
         WITH (storage = '{count_storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    let replacement = count_root.join("replacement.parquet");
    write_parquet_rows(
        &replacement,
        id_field(),
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ],
    );
    std::fs::remove_file(&count_path).unwrap();
    std::fs::rename(replacement, &count_path).unwrap();
    let count_error = count_connection
        .execute("MATCH (n:person) RETURN count(*)")
        .unwrap_err()
        .to_string();
    assert!(count_error.contains("row count changed"), "{count_error}");

    let endpoint_root = interchange_temp_path("icebug-bad-endpoint");
    std::fs::create_dir_all(&endpoint_root).unwrap();
    write_parquet_rows(
        &endpoint_root.join("nodes_person.parquet"),
        id_field(),
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ],
    );
    write_parquet_rows(
        &endpoint_root.join("indices_edge.parquet"),
        vec![koko_loader::ParquetField::new(
            "target",
            LogicalType::Int64,
            false,
        )],
        &[vec![Value::Int64(3)]],
    );
    write_parquet_rows(
        &endpoint_root.join("indptr_edge.parquet"),
        vec![koko_loader::ParquetField::new(
            "offset",
            LogicalType::Int64,
            false,
        )],
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(1)],
            vec![Value::Int64(1)],
        ],
    );
    let endpoint_storage = endpoint_root.to_string_lossy().replace('\\', "/");
    let endpoint_connection = Database::new().connect();
    endpoint_connection
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, PRIMARY KEY(id)) \
         WITH (storage = '{endpoint_storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    endpoint_connection
        .execute(&format!(
            "CREATE REL TABLE edge(FROM person TO person) \
         WITH (storage = '{endpoint_storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    let endpoint_error = endpoint_connection
        .execute("MATCH (:person)-[:edge]->(:person) RETURN count(*)")
        .unwrap_err()
        .to_string();
    assert!(
        endpoint_error.contains("endpoint") && endpoint_error.contains("out of range"),
        "{endpoint_error}"
    );

    let csr_root = interchange_temp_path("icebug-bad-csr");
    std::fs::create_dir_all(&csr_root).unwrap();
    write_parquet_rows(
        &csr_root.join("nodes_person.parquet"),
        id_field(),
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ],
    );
    write_parquet_rows(
        &csr_root.join("indices_edge.parquet"),
        vec![koko_loader::ParquetField::new(
            "target",
            LogicalType::Int64,
            false,
        )],
        &[vec![Value::Int64(1)]],
    );
    write_parquet_rows(
        &csr_root.join("indptr_edge.parquet"),
        vec![koko_loader::ParquetField::new(
            "offset",
            LogicalType::Int64,
            false,
        )],
        &[
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
        ],
    );
    let csr_storage = csr_root.to_string_lossy().replace('\\', "/");
    let csr_connection = Database::new().connect();
    csr_connection
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, PRIMARY KEY(id)) \
         WITH (storage = '{csr_storage}', format = 'icebug-disk')"
        ))
        .unwrap();
    let csr_error = csr_connection
        .execute(&format!(
            "CREATE REL TABLE edge(FROM person TO person) \
         WITH (storage = '{csr_storage}', format = 'icebug-disk')"
        ))
        .unwrap_err()
        .to_string();
    assert!(csr_error.contains("not a monotone"), "{csr_error}");

    for root in [schema_root, count_root, endpoint_root, csr_root] {
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn im5_icebug_query_sources_are_pinned_without_eager_table_hydration() {
    const ROWS: i64 = 4_096;
    let root = interchange_temp_path("icebug-source-lifetime");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("nodes_person.parquet");
    let fields = || {
        vec![koko_loader::ParquetField::new(
            "id",
            LogicalType::Int64,
            false,
        )]
    };
    let old_rows: Vec<_> = (0..ROWS).map(|id| vec![Value::Int64(id)]).collect();
    write_parquet_rows(&path, fields(), &old_rows);
    let storage = root.to_string_lossy().replace('\\', "/");
    let database = Database::new();
    let running = database.connect();
    running
        .execute(&format!(
            "CREATE NODE TABLE person(id INT64, PRIMARY KEY(id)) \
         WITH (storage = '{storage}', format = 'icebug-disk')"
        ))
        .unwrap();

    let replacement = root.join("replacement.parquet");
    let new_rows: Vec<_> = (0..ROWS)
        .map(|id| vec![Value::Int64(id + 10_000)])
        .collect();
    write_parquet_rows(&replacement, fields(), &new_rows);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    let blocked = Arc::new(AtomicBool::new(false));
    let callback_blocked = Arc::clone(&blocked);
    running
        .register_scalar_function(
            ScalarFunction::new(
                "hold_source",
                vec![LogicalType::Int64],
                LogicalType::Int64,
                move |arguments| {
                    if !callback_blocked.swap(true, Ordering::AcqRel) {
                        callback_barrier.wait();
                        callback_barrier.wait();
                    }
                    Ok(arguments[0].clone())
                },
            )
            .with_null_policy(NullPolicy::Propagate),
        )
        .unwrap();
    std::thread::scope(|scope| {
        let query =
            scope.spawn(|| running.execute("MATCH (n:person) RETURN sum(hold_source(n.id))"));
        barrier.wait();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        barrier.wait();
        assert_eq!(
            query.join().unwrap().unwrap().rendered_rows(),
            vec!["8386560"]
        );
    });
    assert_eq!(
        running
            .execute("MATCH (n:person) RETURN sum(n.id)")
            .unwrap()
            .rendered_rows(),
        vec!["49346560"]
    );
    std::fs::remove_dir_all(root).unwrap();
}
