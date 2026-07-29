use super::*;

#[test]
fn im3_copy_to_csv_preserves_schema_values_and_quoting() {
    let path = interchange_temp_path("copy-to.csv");
    let connection = Database::new().connect();
    assert_eq!(
        connection
            .execute("RETURN [['a'], []] AS nested")
            .unwrap()
            .rendered_rows(),
        vec!["[[a],[]]"]
    );
    connection
        .execute(&format!(
            "COPY (RETURN 1 AS id, 'Ada, \"A\"' AS name, [1,2] AS tags) TO '{}' \
         (HEADER=true)",
            path.display()
        ))
        .unwrap();
    let result = connection
        .execute(&format!(
            "LOAD WITH HEADERS (id INT64, name STRING, tags INT64[]) FROM '{}' \
         (AUTO_DETECT=false) RETURN id, name, tags",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        result
            .columns()
            .iter()
            .map(Column::name)
            .collect::<Vec<_>>(),
        ["id", "name", "tags"]
    );
    assert_eq!(result.rendered_rows(), vec!["1|Ada, \"A\"|[1,2]"]);
    let empty = interchange_temp_path("copy-to-empty.csv");
    connection
        .execute(&format!(
            "COPY (UNWIND [1] AS x WITH x WHERE false RETURN x AS id) TO '{}' (HEADER=true)",
            empty.display()
        ))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&empty).unwrap(), "id\n");
    std::fs::remove_file(empty).unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn im4_interchange_cancellation_removes_partial_output() {
    let directory = interchange_temp_path("cancel-output");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("result.csv");
    let result = QueryResult::from_typed_rows(
        vec!["value".to_string()],
        vec![LogicalType::Int64],
        vec![vec![Value::Int64(1)]],
    )
    .unwrap();
    let memory = MemoryTracker::new(None);
    let epoch = AtomicU64::new(0);
    let control = koko_common::QueryControl::new(&epoch, 0, None);
    epoch.store(1, Ordering::Release);

    let error = crate::interchange::write_query_result(
        &path,
        &koko_ir::bound::BoundOutputOptions::Csv(koko_common::csv_dialect::CsvOptions::default()),
        &result,
        &memory,
        control,
    )
    .unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");
    assert!(!path.exists());
    assert!(std::fs::read_dir(&directory).unwrap().next().is_none());
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn im3_export_import_csv_round_trips_schema_data_and_sequence_state() {
    let export = interchange_temp_path("database");
    let connection = Database::new().connect();
    connection.execute("CREATE TYPE Code AS STRING").unwrap();
    connection
        .execute("CREATE NODE TABLE Person(id SERIAL, code Code, name STRING, PRIMARY KEY(id))")
        .unwrap();
    connection
        .execute(
            "CREATE REL TABLE Knows(FROM Person TO Person, since INT64, MANY_MANY) \
     WITH (storage_direction='both')",
        )
        .unwrap();
    connection
        .execute("CREATE SEQUENCE ticket START 5 INCREMENT 2")
        .unwrap();
    connection
        .execute("CREATE MACRO plus1(x) AS x + 1")
        .unwrap();
    connection
        .execute("COMMENT ON TABLE Person IS 'people'")
        .unwrap();
    connection
        .execute("CREATE (:Person {code: 'a', name: 'Ada'}), (:Person {code: 'b', name: 'Bob'})")
        .unwrap();
    connection
        .execute(
            "MATCH (a:Person), (b:Person) WHERE a.code='a' AND b.code='b' \
     CREATE (a)-[:Knows {since: 2026}]->(b)",
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("RETURN nextval('ticket')")
            .unwrap()
            .rendered_rows(),
        vec!["5"]
    );
    connection
        .execute(&format!(
            "EXPORT DATABASE '{}' (format='csv', header=true, delim='|')",
            export.display()
        ))
        .unwrap();
    for file in [
        "schema.cypher",
        "copy.cypher",
        "index.cypher",
        "Person.csv",
        "Knows_Person_Person.csv",
    ] {
        assert!(export.join(file).is_file(), "missing {file}");
    }

    let restored = Database::new().connect();
    restored
        .execute(&format!("IMPORT DATABASE '{}'", export.display()))
        .unwrap();
    assert_eq!(
        sorted_rows(&restored, "MATCH (p:Person) RETURN p.id, p.code, p.name"),
        vec!["0|a|Ada", "1|b|Bob"]
    );
    assert_eq!(
        restored
            .execute("MATCH (:Person)-[r:Knows]->(:Person) RETURN r.since")
            .unwrap()
            .rendered_rows(),
        vec!["2026"]
    );
    assert_eq!(
        restored.execute("RETURN plus1(4)").unwrap().rendered_rows(),
        vec!["5"]
    );
    assert_eq!(
        restored
            .execute("RETURN nextval('ticket'), nextval('Person_id_serial')")
            .unwrap()
            .rendered_rows(),
        vec!["7|2"]
    );
    let comment = restored
        .execute("CALL show_tables() WHERE name='Person' RETURN comment")
        .unwrap()
        .rendered_rows();
    assert_eq!(comment, vec!["people"]);
    std::fs::remove_dir_all(export).unwrap();
}

#[test]
fn im3_import_rolls_back_all_schema_changes_on_nested_failure() {
    let root = interchange_temp_path("atomic-import");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("schema.cypher"),
        "CREATE NODE TABLE P(id INT64, PRIMARY KEY(id));\n\
         CREATE NODE TABLE P(id INT64, PRIMARY KEY(id));\n",
    )
    .unwrap();
    std::fs::write(root.join("copy.cypher"), "").unwrap();
    std::fs::write(root.join("index.cypher"), "").unwrap();
    std::fs::write(
        root.join("manifest.koko"),
        format!(
            "{}\nGRAPH\tT\t6d61696e\t.\n",
            crate::interchange::DATABASE_IMAGE_HEADER
        ),
    )
    .unwrap();
    let connection = Database::new().connect();
    let error = connection
        .execute(&format!("IMPORT DATABASE '{}'", root.display()))
        .unwrap_err();
    assert!(
        error.to_string().contains("Import database failed"),
        "{error}"
    );
    assert_eq!(
        connection
            .execute("CALL show_tables() RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn im3_parquet_copy_to_copy_from_and_load_round_trip() {
    let path = interchange_temp_path("typed.parquet");
    let source = Database::new().connect();
    source
        .execute("CREATE NODE TABLE P(id INT64, name STRING, span INTERVAL, PRIMARY KEY(id))")
        .unwrap();
    source
        .execute(
            "CREATE (:P {id: 1, name: 'Ada', span: '10 years 5 months 13:00:00.000024'}), \
     (:P {id: 2, name: 'Bob', span: '3 days 00:23:00'})",
        )
        .unwrap();
    source.execute(&format!(
        "COPY (MATCH (p:P) RETURN p.id AS id, p.name AS name, p.span AS span ORDER BY p.id) TO '{}'",
        path.display()
    ))
        .unwrap();

    let restored = Database::new().connect();
    restored
        .execute("CREATE NODE TABLE P(id INT64, name STRING, span INTERVAL, PRIMARY KEY(id))")
        .unwrap();
    let invalid = restored
        .execute(&format!("COPY P FROM '{}' (DELIM='|')", path.display()))
        .unwrap_err();
    assert_eq!(
        invalid.to_string(),
        "Binder exception: Copy from Parquet cannot have options other than IGNORE_ERRORS."
    );
    restored
        .execute(&format!("COPY P FROM '{}'", path.display()))
        .unwrap();
    assert_eq!(
        restored
            .execute("MATCH (p:P) RETURN p.id, p.name, p.span ORDER BY p.id")
            .unwrap()
            .rendered_rows(),
        vec!["1|Ada|10 years 5 months 13:00:00", "2|Bob|3 days 00:23:00"]
    );
    assert_eq!(
        restored
            .execute(&format!(
                "LOAD FROM '{}' RETURN id, name, span ORDER BY id",
                path.display()
            ))
            .unwrap()
            .rendered_rows(),
        vec!["1|Ada|10 years 5 months 13:00:00", "2|Bob|3 days 00:23:00"]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn im3_npy_copy_by_column_and_bare_load() {
    let root = external_dataset_dir()
        .join("npy-2d")
        .canonicalize()
        .unwrap();
    let connection = Database::new().connect();
    connection
        .execute(
            "CREATE NODE TABLE N(id INT64, i64 INT64[3], i32 INT32[3], i16 INT16[3], \
     f64 DOUBLE[3], f32 FLOAT[3], PRIMARY KEY(id))",
        )
        .unwrap();
    connection
        .execute(&format!(
            "COPY N FROM ('{}', '{}', '{}', '{}', '{}', '{}') BY COLUMN",
            root.join("id_int64.npy").display(),
            root.join("two_dim_int64.npy").display(),
            root.join("two_dim_int32.npy").display(),
            root.join("two_dim_int16.npy").display(),
            root.join("two_dim_double.npy").display(),
            root.join("two_dim_float.npy").display(),
        ))
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:N) RETURN n.id, n.i64, n.f64 ORDER BY n.id")
            .unwrap()
            .rendered_rows(),
        vec![
            "1|[1,2,3]|[1.000000,2.000000,3.000000]",
            "2|[4,5,6]|[4.000000,5.000000,6.000000]",
            "3|[7,8,9]|[7.000000,8.000000,9.000000]",
        ]
    );
    let one_dim = root.parent().unwrap().join("npy-1d/one_dim_int64.npy");
    connection
        .execute("CREATE NODE TABLE One(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let invalid = connection
        .execute(&format!(
            "COPY One FROM '{}' (invalid_option=true)",
            one_dim.display()
        ))
        .unwrap_err();
    assert_eq!(
        invalid.to_string(),
        "Binder exception: Copy from numpy cannot have options other than IGNORE_ERRORS."
    );
    assert_eq!(
        connection
            .execute(&format!(
                "LOAD FROM '{}' RETURN column0 ORDER BY column0",
                one_dim.display()
            ))
            .unwrap()
            .rendered_rows(),
        vec!["1", "2", "3"]
    );
}

#[test]
fn im3_relationship_group_copy_routes_selected_pair() {
    let path = interchange_temp_path("rel.csv");
    std::fs::write(&path, "1,3,9\n").unwrap();
    let connection = Database::new().connect();
    for table in ["A", "B", "C"] {
        connection
            .execute(&format!(
                "CREATE NODE TABLE {table}(id INT64, PRIMARY KEY(id))"
            ))
            .unwrap();
    }
    connection.execute("CREATE (:A {id: 1})").unwrap();
    connection.execute("CREATE (:B {id: 2})").unwrap();
    connection.execute("CREATE (:C {id: 3})").unwrap();
    connection
        .execute("CREATE REL TABLE R(FROM A TO B, FROM A TO C, v INT64, MANY_MANY)")
        .unwrap();
    let error = connection
        .execute(&format!("COPY R FROM '{}'", path.display()))
        .unwrap_err();
    assert!(
        error.to_string().contains("multiple FROM and TO pairs"),
        "{error}"
    );
    connection
        .execute(&format!(
            "COPY R FROM '{}' (FROM='A', TO='C')",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH (a:A)-[r:R]->(c:C) RETURN a.id, c.id, r.v")
            .unwrap()
            .rendered_rows(),
        vec!["1|3|9"]
    );
    assert_eq!(
        connection
            .execute("MATCH (:A)-[r:R]->(:B) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn im3_parquet_database_export_import_preserves_rel_groups_and_empty_tables() {
    let export = interchange_temp_path("parquet-database");
    let connection = Database::new().connect();
    for table in ["A", "B", "C", "Empty"] {
        connection
            .execute(&format!(
                "CREATE NODE TABLE {table}(id INT64, PRIMARY KEY(id))"
            ))
            .unwrap();
    }
    connection.execute("CREATE (:A {id: 1})").unwrap();
    connection.execute("CREATE (:B {id: 2})").unwrap();
    connection.execute("CREATE (:C {id: 3})").unwrap();
    connection
        .execute("CREATE REL TABLE R(FROM A TO B, FROM A TO C, v INT64, MANY_MANY)")
        .unwrap();
    connection
        .execute(
            "MATCH (a:A), (b:B), (c:C) \
     CREATE (a)-[:R {v: 12}]->(b), (a)-[:R {v: 13}]->(c)",
        )
        .unwrap();
    connection
        .execute(&format!(
            "EXPORT DATABASE '{}' (format='parquet')",
            export.display()
        ))
        .unwrap();
    assert!(export.join("Empty.parquet").is_file());
    assert!(export.join("R_A_B.parquet").is_file());
    assert!(export.join("R_A_C.parquet").is_file());

    let restored = Database::new().connect();
    restored
        .execute(&format!("IMPORT DATABASE '{}'", export.display()))
        .unwrap();
    assert_eq!(
        restored
            .execute("MATCH (:A)-[r:R]->(:B) RETURN r.v")
            .unwrap()
            .rendered_rows(),
        vec!["12"]
    );
    assert_eq!(
        restored
            .execute("MATCH (:A)-[r:R]->(:C) RETURN r.v")
            .unwrap()
            .rendered_rows(),
        vec!["13"]
    );
    assert_eq!(
        restored
            .execute("MATCH (n:Empty) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    std::fs::remove_dir_all(export).unwrap();
}

#[test]
fn im5_database_interchange_round_trips_every_graph_from_every_selection() {
    let roots = [
        interchange_temp_path("database-main"),
        interchange_temp_path("database-typed"),
        interchange_temp_path("database-any"),
    ];
    let source_database = Database::new();
    let source = source_database.connect();
    source
        .execute("CREATE NODE TABLE Main(id INT64, PRIMARY KEY(id))")
        .unwrap();
    source.execute("CREATE (:Main {id: 1})").unwrap();
    source.execute("CREATE GRAPH typed").unwrap();
    source.execute("USE GRAPH typed").unwrap();
    source
        .execute("CREATE NODE TABLE T(id INT64, name STRING, PRIMARY KEY(id))")
        .unwrap();
    source
        .execute("CREATE (:T {id: 2, name: 'typed'})")
        .unwrap();
    source
        .execute("CREATE HASH INDEX tidx FOR (n:T) ON (n.id)")
        .unwrap();
    source.execute("CREATE GRAPH dynamic ANY").unwrap();
    source.execute("USE GRAPH dynamic").unwrap();
    source
        .execute("CREATE (:A {name: 'left', z: 3}), (:B {name: 'right'})")
        .unwrap();
    source
        .execute(
            "MATCH (a:A {name: 'left'}), (b:B {name: 'right'}) \
     CREATE (a)-[:R {weight: 7}]->(b)",
        )
        .unwrap();

    for ((selection, root), index) in ["main", "typed", "dynamic"]
        .into_iter()
        .zip(&roots)
        .zip(0..)
    {
        source.execute(&format!("USE GRAPH {selection}")).unwrap();
        let statement = format!("EXPORT DATABASE '{}' (format='csv')", root.display());
        if index == 0 {
            let mut prepared = source.prepare(&statement).unwrap();
            prepared.execute().unwrap();
        } else {
            source.execute(&statement).unwrap();
        }
    }
    let manifest = std::fs::read(roots[0].join("manifest.koko")).unwrap();
    assert_eq!(
        manifest,
        std::fs::read(roots[1].join("manifest.koko")).unwrap()
    );
    assert_eq!(
        manifest,
        std::fs::read(roots[2].join("manifest.koko")).unwrap()
    );
    assert!(String::from_utf8_lossy(&manifest).contains("GRAPH\tA\t64796e616d6963"));
    assert_eq!(
        source
            .execute("MATCH (n:A) RETURN n.name")
            .unwrap()
            .rendered_rows(),
        vec!["left"]
    );

    let restored_database = Database::new();
    let importer = restored_database.connect();
    let observer = restored_database.connect();
    importer.execute("CREATE GRAPH typed").unwrap();
    observer.execute("USE GRAPH typed").unwrap();
    importer.execute("CREATE GRAPH stale").unwrap();
    importer.execute("USE GRAPH stale").unwrap();
    importer
        .prepare(&format!("IMPORT DATABASE '{}'", roots[2].display()))
        .unwrap()
        .execute()
        .unwrap();

    assert_eq!(
        importer
            .execute("MATCH (n:Main) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        observer
            .execute("MATCH (n:T) RETURN n.id, n.name")
            .unwrap()
            .rendered_rows(),
        vec!["2|typed"]
    );
    assert_eq!(
        observer
            .execute("CALL show_indexes() RETURN index_name, index_type")
            .unwrap()
            .rendered_rows(),
        vec!["tidx|HASH"]
    );
    importer.execute("USE GRAPH dynamic").unwrap();
    assert_eq!(
        importer
            .execute(
                "MATCH (a:A)-[r:R]->(b:B) \
                 RETURN a.name, r.weight, b.name",
            )
            .unwrap()
            .rendered_rows(),
        vec!["left|7|right"]
    );
    assert_eq!(
        importer
            .execute("MATCH (n) RETURN n.name ORDER BY n.name")
            .unwrap()
            .rendered_rows(),
        vec!["left", "right"]
    );
    assert_eq!(
        importer.execute("USE GRAPH stale").unwrap_err().to_string(),
        "Binder exception: No graph named stale."
    );

    let mut duplicate_manifest = String::from_utf8(manifest).unwrap();
    duplicate_manifest.push_str("GRAPH\tT\t6d61696e\tgraphs/000001\n");
    std::fs::write(roots[0].join("manifest.koko"), duplicate_manifest).unwrap();
    let collision = Database::new().connect();
    let error = collision
        .execute(&format!("IMPORT DATABASE '{}'", roots[0].display()))
        .unwrap_err();
    assert!(
        error.to_string().contains("duplicate graph name main"),
        "{error}"
    );
    for root in roots {
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn im5_database_import_detects_concurrent_registry_change_without_publication() {
    let root = interchange_temp_path("database-concurrent");
    let source = Database::new().connect();
    source
        .execute("CREATE NODE TABLE Imported(id INT64, PRIMARY KEY(id))")
        .unwrap();
    source.execute("CREATE (:Imported {id: 9})").unwrap();
    source
        .execute(&format!(
            "EXPORT DATABASE '{}' (format='csv')",
            root.display()
        ))
        .unwrap();
    let schema_path = root.join("schema.cypher");
    let mut schema = std::fs::read_to_string(&schema_path).unwrap();
    schema.push_str("RETURN hold_import(1);\n");
    std::fs::write(&schema_path, schema).unwrap();

    let database = Database::new();
    let importer = database.connect();
    let concurrent = database.connect();
    importer
        .execute("CREATE NODE TABLE Keep(id INT64, PRIMARY KEY(id))")
        .unwrap();
    importer.execute("CREATE (:Keep {id: 1})").unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback_barrier = Arc::clone(&barrier);
    importer
        .register_scalar_function(
            ScalarFunction::new(
                "hold_import",
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
        let importing =
            scope.spawn(|| importer.execute(&format!("IMPORT DATABASE '{}'", root.display())));
        barrier.wait();
        concurrent.execute("CREATE GRAPH raced").unwrap();
        barrier.wait();
        let error = importing.join().unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Graph registry changed while importing database"),
            "{error}"
        );
    });
    assert_eq!(
        importer
            .execute("MATCH (n:Keep) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert!(importer.execute("MATCH (n:Imported) RETURN n.id").is_err());
    concurrent.execute("USE GRAPH raced").unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn im5_database_import_deadline_and_low_memory_leave_registry_unchanged() {
    let root = interchange_temp_path("database-resources");
    let source = Database::new().connect();
    source
        .execute("CREATE NODE TABLE Payload(id INT64, value STRING, PRIMARY KEY(id))")
        .unwrap();
    source
        .execute(
            "UNWIND range(0, 9999) AS id \
     CREATE (:Payload {id: id, value: 'payload'})",
        )
        .unwrap();
    source
        .execute(&format!(
            "EXPORT DATABASE '{}' (format='csv')",
            root.display()
        ))
        .unwrap();

    let deadline_database = Database::new();
    let deadline = deadline_database.connect();
    deadline
        .execute("CREATE NODE TABLE Keep(id INT64, PRIMARY KEY(id))")
        .unwrap();
    deadline
        .set_query_timeout(Some(std::time::Duration::from_millis(1)))
        .unwrap();
    let error = deadline
        .execute(&format!("IMPORT DATABASE '{}'", root.display()))
        .unwrap_err()
        .to_string()
        .to_ascii_lowercase();
    assert!(
        error.contains("interrupt") || error.contains("deadline"),
        "{error}"
    );
    deadline.set_query_timeout(None).unwrap();
    assert_eq!(
        deadline
            .execute("CALL show_tables() RETURN name ORDER BY name")
            .unwrap()
            .rendered_rows(),
        vec!["Keep"]
    );

    let constrained_database =
        Database::with_config(DatabaseConfig::new().with_memory_limit(64 * 1024).unwrap());
    let constrained = constrained_database.connect();
    constrained
        .execute("CREATE NODE TABLE Keep(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let memory_before = constrained_database.memory_usage().current;
    assert!(matches!(
        constrained
            .execute(&format!("IMPORT DATABASE '{}'", root.display()))
            .unwrap_err(),
        Error::BufferManager
    ));
    assert_eq!(constrained_database.memory_usage().current, memory_before);
    assert_eq!(
        constrained
            .execute("CALL show_tables() RETURN name ORDER BY name")
            .unwrap()
            .rendered_rows(),
        vec!["Keep"]
    );
    std::fs::remove_dir_all(root).unwrap();
}
