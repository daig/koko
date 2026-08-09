use super::*;

#[test]
fn end_to_end_create_match_return() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))")
        .unwrap();
    c.execute("CREATE (:Person {name: 'Alice', age: 35})")
        .unwrap();
    c.execute("CREATE (:Person {name: 'Bob', age: 20})")
        .unwrap();
    let r = c
        .execute("MATCH (p:Person) WHERE p.age > 30 RETURN p.name, p.age")
        .unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r.value(0, 0).unwrap().as_str(), Some("Alice"));
    assert_eq!(r.get_row_i64(0, 1), 35);
}

#[test]
fn query_result_preserves_typed_batches_and_checked_views() {
    let db = Database::new();
    let connection = db.connect();
    let result = connection
        .execute(&format!(
            "UNWIND range(0, {}) AS i RETURN i",
            VECTOR_CAPACITY + 2
        ))
        .unwrap();

    assert_eq!(
        result.columns(),
        &[Column::new("i".to_string(), LogicalType::Int64)]
    );
    assert_eq!(result.len(), VECTOR_CAPACITY + 3);
    assert_eq!(result.batches().len(), 2);
    assert_eq!(result.batches()[0].size(), VECTOR_CAPACITY);
    assert_eq!(result.batches()[1].size(), 3);

    let integers = result.typed_column::<i64>("i").unwrap();
    assert_eq!(integers.get(0).unwrap(), 0);
    assert_eq!(
        integers.get(VECTOR_CAPACITY - 1).unwrap(),
        (VECTOR_CAPACITY - 1) as i64
    );
    assert_eq!(
        integers.get(VECTOR_CAPACITY).unwrap(),
        VECTOR_CAPACITY as i64
    );
    assert_eq!(
        integers.get(VECTOR_CAPACITY + 2).unwrap(),
        (VECTOR_CAPACITY + 2) as i64
    );
    assert!(integers.get(VECTOR_CAPACITY + 3).is_err());
    assert!(result.column(1).is_err());
    assert!(result.column("missing").is_err());
    assert!(result.typed_column::<String>(0).is_err());

    let boundary = result.rows().nth(VECTOR_CAPACITY).unwrap();
    assert_eq!(boundary.get::<i64>(0).unwrap(), VECTOR_CAPACITY as i64);
    assert_eq!(boundary.get::<i64>("i").unwrap(), VECTOR_CAPACITY as i64);
}

#[test]
fn named_result_access_rejects_ambiguous_columns() {
    let result = Database::new()
        .connect()
        .execute("RETURN 1 AS x, 2 AS x")
        .unwrap();
    let error = match result.column("x") {
        Err(error) => error,
        Ok(_) => panic!("duplicate result names must be ambiguous"),
    };
    assert!(error.to_string().contains("ambiguous"));
    assert!(result.rows().next().unwrap().get::<i64>("x").is_err());
}

#[test]
fn columnar_result_tail_preserves_distinct_order_and_union() {
    let db = Database::new();
    let connection = db.connect();

    let ordered = connection
        .execute("UNWIND [3, 1, 2, 2] AS x RETURN DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 2")
        .unwrap();
    assert_eq!(ordered.rendered_rows(), vec!["2", "1"]);

    let union = connection
        .execute("RETURN 1 AS x UNION RETURN 1 AS x UNION RETURN 2 AS x")
        .unwrap();
    assert_eq!(union.rendered_rows(), vec!["1", "2"]);
}

#[test]
fn union_result_types_are_canonical_across_null_and_dynamic_operands() {
    let connection = Database::new().connect();

    for query in [
        "RETURN 1 AS x UNION ALL RETURN null AS x",
        "RETURN null AS x UNION ALL RETURN 1 AS x",
        "RETURN 1 AS x UNION RETURN null AS x",
        "RETURN null AS x UNION RETURN 1 AS x",
        "RETURN null AS x UNION ALL RETURN 1 AS x UNION ALL RETURN null AS x",
    ] {
        let result = connection.execute(query).unwrap();
        assert_eq!(
            result.columns(),
            &[Column::new("x".to_string(), LogicalType::Int64)]
        );
    }

    let distinct_nulls = connection
        .execute("RETURN null AS x UNION RETURN null AS x")
        .unwrap();
    assert_eq!(
        distinct_nulls.columns(),
        &[Column::new("x".to_string(), LogicalType::Any)]
    );
    assert_eq!(distinct_nulls.rendered_rows(), vec![""]);
    let all_nulls = connection
        .execute("RETURN null AS x UNION ALL RETURN null AS x")
        .unwrap();
    assert_eq!(all_nulls.len(), 2);

    let positions = connection
        .execute("RETURN null AS i, 'left' AS s UNION ALL RETURN 7 AS i, null AS s")
        .unwrap();
    assert_eq!(
        positions
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect::<Vec<_>>(),
        [LogicalType::Int64, LogicalType::String]
    );
    assert_eq!(positions.rendered_rows(), ["|left", "7|"]);

    let explicit = connection
        .execute("RETURN CAST(null AS INT64) AS x UNION ALL RETURN 1 AS x")
        .unwrap();
    assert_eq!(explicit.columns()[0].logical_type(), &LogicalType::Int64);

    let dynamic_queries = [
        "RETURN 's' AS x UNION ALL \
         RETURN union_extract(union_value(a := 1), 'a') AS x",
        "RETURN union_extract(union_value(a := 1), 'a') AS x UNION ALL \
         RETURN 's' AS x",
        "RETURN 1 AS x UNION ALL \
         RETURN union_extract(union_value(a := 2), 'a') AS x UNION ALL \
         RETURN 'three' AS x",
        "UNWIND [] AS ignored \
         RETURN union_extract(union_value(a := 1), 'a') AS x UNION ALL \
         RETURN 's' AS x",
    ];
    for query in dynamic_queries {
        let result = connection.execute(query).unwrap();
        assert_eq!(
            result.columns(),
            &[Column::new("x".to_string(), LogicalType::Any)]
        );
    }

    let empty = connection
        .execute(
            "UNWIND [] AS i RETURN null AS x UNION ALL \
             UNWIND [] AS j RETURN 1 AS x",
        )
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.columns()[0].logical_type(), &LogicalType::Int64);

    assert_eq!(
        connection
            .execute("RETURN 1 AS x UNION ALL RETURN 's' AS x")
            .unwrap_err()
            .to_string(),
        "Binder exception: x has data type STRING but INT64 was expected."
    );
}

#[test]
fn empty_unlabeled_graph_results_publish_any_without_panicking() {
    let connection = Database::new().connect();
    for query in [
        "MATCH (n) RETURN n AS value",
        "MATCH ()-[r]->() RETURN r AS value",
    ] {
        let result = connection.execute(query).unwrap();
        assert!(result.is_empty());
        assert_eq!(
            result.columns(),
            &[Column::new("value".to_string(), LogicalType::Any)]
        );
    }
}

#[test]
fn union_prepared_parameters_infer_and_enforce_the_canonical_type() {
    let connection = Database::new().connect();
    for (query, conflict) in [
        (
            "RETURN $value AS x UNION ALL RETURN 1 AS x",
            "Binder exception: x has data type INT64 but STRING was expected.",
        ),
        (
            "RETURN 1 AS x UNION ALL RETURN $value AS x",
            "Binder exception: x has data type STRING but INT64 was expected.",
        ),
    ] {
        let mut prepared = connection.prepare(query).unwrap();
        assert_eq!(prepared.parameters()[0].logical_type(), &LogicalType::Int64);
        assert_eq!(prepared.columns()[0].logical_type(), &LogicalType::Int64);
        assert!(
            prepared
                .execute()
                .unwrap()
                .rendered_rows()
                .contains(&"1".to_string())
        );
        assert!(
            prepared
                .execute_with(params! { "value" => 2 })
                .unwrap()
                .rendered_rows()
                .contains(&"2".to_string())
        );
        assert_eq!(
            prepared
                .execute_with(params! { "value" => "wrong" })
                .unwrap_err()
                .to_string(),
            conflict
        );
    }

    let mut null_seeded = connection
        .prepare_with(
            "RETURN $value AS x UNION ALL RETURN 1 AS x",
            params! { "value" => Value::Null },
        )
        .unwrap();
    assert_eq!(
        null_seeded.parameters()[0].logical_type(),
        &LogicalType::Int64
    );
    assert_eq!(null_seeded.execute().unwrap().rendered_rows(), ["", "1"]);

    let matching = connection
        .prepare_with(
            "RETURN $value AS x UNION ALL RETURN 1 AS x",
            params! { "value" => 2 },
        )
        .unwrap();
    assert_eq!(matching.columns()[0].logical_type(), &LogicalType::Int64);
    let conflicting = connection.prepare_with(
        "RETURN $value AS x UNION ALL RETURN 1 AS x",
        params! { "value" => "wrong" },
    );
    assert_eq!(
        conflicting.err().unwrap().to_string(),
        "Binder exception: x has data type INT64 but STRING was expected."
    );
}

#[test]
fn numeric_extrema_constrain_prepared_parameters() {
    let connection = Database::new().connect();
    let mut prepared = connection
        .prepare("RETURN greatest($value, 1.5, 2) AS x")
        .unwrap();

    assert_eq!(
        prepared.parameters()[0].logical_type(),
        &LogicalType::Double
    );
    assert_eq!(prepared.columns()[0].logical_type(), &LogicalType::Double);
    assert_eq!(prepared.execute().unwrap().rendered_rows(), [""]);
    assert_eq!(
        prepared
            .execute_with(params! { "value" => 3 })
            .unwrap()
            .rendered_rows(),
        ["3.000000"]
    );
}

#[test]
fn union_canonical_schema_survives_ctas_and_copy() {
    let connection = Database::new().connect();
    connection
        .execute(
            "CREATE NODE TABLE UnionCtas AS \
             UNWIND [] AS i RETURN null AS id UNION ALL RETURN 1 AS id",
        )
        .unwrap();
    let ctas = connection
        .execute("MATCH (n:UnionCtas) RETURN n.id AS id")
        .unwrap();
    assert_eq!(ctas.columns()[0].logical_type(), &LogicalType::Int64);
    assert_eq!(ctas.rendered_rows(), ["1"]);

    connection
        .execute("CREATE NODE TABLE UnionCopy(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .execute(
            "COPY UnionCopy FROM \
             (UNWIND [] AS i RETURN null AS id UNION ALL RETURN 2 AS id)",
        )
        .unwrap();
    assert_eq!(
        connection
            .execute("MATCH (n:UnionCopy) RETURN n.id")
            .unwrap()
            .rendered_rows(),
        ["2"]
    );

    let path = interchange_temp_path("union-canonical.csv");
    connection
        .execute(&format!(
            "COPY (UNWIND [] AS i RETURN null AS id UNION ALL RETURN 3 AS id) \
             TO '{}' (HEADER=true)",
            path.display()
        ))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "id\n3\n");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn typed_results_use_bound_runtime_scalar_types() {
    let db = Database::new();
    let result = db
        .connect()
        .execute(
            "RETURN round(123, 0) AS rounded, \
     floor(2.5) AS floored, \
     list_reduce(['a:', 'b:', 'c:', 'd:'], (x, y) -> y + x) AS reduced",
        )
        .unwrap();
    assert_eq!(
        result
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect::<Vec<_>>(),
        vec![
            LogicalType::Double,
            LogicalType::Double,
            LogicalType::String,
        ]
    );
    assert_eq!(result.rendered_rows(), vec!["123.000000|2.000000|d:c:b:a:"]);
}

#[test]
fn nested_lambda_errors_restore_pipeline_local_bindings() {
    let connection = Database::new().connect();
    let error = connection
        .execute("RETURN list_transform([1], x -> list_reduce([], (a, b) -> a + b))")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Runtime exception: Cannot execute list_reduce on an empty list."
    );

    let result = connection
        .execute("RETURN list_transform([1,2], x -> list_filter([x,x+1], y -> y > x))")
        .unwrap();
    assert_eq!(result.rendered_rows(), vec!["[[2],[3]]"]);
}

#[test]
fn query_results_publish_duration_summaries() {
    let db = Database::new();
    let connection = db.connect();
    let direct = connection.execute("RETURN 42 AS answer").unwrap();
    let direct_summary = *direct.summary();
    assert!(direct_summary.compilation_time() + direct_summary.execution_time() > Duration::ZERO);

    let mut prepared = connection.prepare("RETURN $value AS answer").unwrap();
    let executed = prepared.execute_with(params! { "value" => 7 }).unwrap();
    assert_eq!(executed.rendered_rows(), vec!["7"]);
    assert!(
        executed
            .summary()
            .compilation_time()
            .as_secs_f64()
            .is_finite()
    );
    assert!(
        executed
            .summary()
            .execution_time()
            .as_secs_f64()
            .is_finite()
    );
}

/// Factorization (P3 step 6): an aggregate over a collapsible fan-out suffix
/// must fold the suffix into a multiplicity and produce the same numbers a
/// materialized cross-product would — `count(*)` sums the fan-out, `sum`/`avg`
/// scale the (flat) head value by it — while a *read* endpoint still fans out.
#[test]
fn factorized_aggregate_over_a_fanout() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, age INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE REL TABLE K(FROM P TO P)").unwrap();
    for i in 0..4 {
        c.execute(&format!("CREATE (:P {{id: {i}, age: {}}})", 10 + i))
            .unwrap();
    }
    // out-degrees: 0->{1,2,3} (3 edges), 1->{2} (1 edge); 4 edges total.
    for (a, b) in [(0, 1), (0, 2), (0, 3), (1, 2)] {
        c.execute(&format!(
            "MATCH (a:P), (b:P) WHERE a.id = {a} AND b.id = {b} CREATE (a)-[:K]->(b)"
        ))
        .unwrap();
    }

    // count(*): `b` and the rel are unread ⇒ the extend collapses the fan-out.
    assert_eq!(
        c.execute("MATCH (a:P)-[:K]->(b:P) RETURN count(*)")
            .unwrap()
            .get_row_i64(0, 0),
        4
    );
    // sum/avg fold the (flat) head value with the suffix multiplicity:
    // a=0 (age 10) ×3 + a=1 (age 11) ×1 = 41 over 4 tuples.
    assert_eq!(
        c.execute("MATCH (a:P)-[:K]->(b:P) RETURN sum(a.age)")
            .unwrap()
            .get_row_i64(0, 0),
        41
    );
    assert_eq!(
        c.execute("MATCH (a:P)-[:K]->(b:P) RETURN avg(a.age)")
            .unwrap()
            .rendered_rows(),
        vec!["10.250000"]
    );
    // Grouped by the flat head — `b` collapses into each group's count.
    assert_eq!(
        sorted_rows(&c, "MATCH (a:P)-[:K]->(b:P) RETURN a.id, count(*)"),
        vec!["0|3", "1|1"]
    );
    // The endpoint IS read here ⇒ no collapse ⇒ ordinary fan-out, still correct
    // (in-degrees: 1←{0}, 2←{0,1}, 3←{0}).
    assert_eq!(
        sorted_rows(&c, "MATCH (a:P)-[:K]->(b:P) RETURN b.id, count(*)"),
        vec!["1|1", "2|2", "3|1"]
    );
    // count(DISTINCT head) must ignore the multiplicity (one distinct `a`,
    // though it stands for 3 tuples).
    assert_eq!(
        c.execute("MATCH (a:P)-[:K]->(b:P) WHERE a.id = 0 RETURN count(DISTINCT a.id)")
            .unwrap()
            .get_row_i64(0, 0),
        1
    );
}

#[test]
fn load_from_with_headers_read_filter_create() {
    // Header row whose names match the declared columns (auto-skipped).
    let (path, p) = temp_csv("headers", "id,name,age\n1,Alice,35\n2,Bob,20\n3,Cara,40\n");
    let db = Database::new();
    let c = db.connect();

    // LOAD … RETURN streams the file's rows as the declared columns.
    let cols = "(id INT64, name STRING, age INT64)";
    assert_eq!(
        sorted_rows(
            &c,
            &format!("LOAD WITH HEADERS {cols} FROM \"{p}\" RETURN name, age"),
        ),
        vec!["Alice|35", "Bob|20", "Cara|40"]
    );
    // LOAD … WHERE … RETURN composes a filter over the loaded columns.
    assert_eq!(
        sorted_rows(
            &c,
            &format!("LOAD WITH HEADERS {cols} FROM \"{p}\" WHERE age > 30 RETURN name"),
        ),
        vec!["Alice", "Cara"]
    );
    // LOAD … CREATE bulk-loads nodes via Cypher.
    c.execute("CREATE NODE TABLE Person(id INT64, name STRING, age INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute(&format!(
        "LOAD WITH HEADERS {cols} FROM \"{p}\" \
         CREATE (:Person {{id: id, name: name, age: age}})"
    ))
    .unwrap();
    assert_eq!(
        c.execute("MATCH (p:Person) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["3"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_match_create_rel_and_header_parse_heuristic() {
    let (np, n) = temp_csv("rel_nodes", "id\n1\n2\n3\n");
    // Edge file has a header `from,to` that does NOT match the declared names
    // (`src`,`dst`) — it is detected as a header because `from` doesn't parse
    // as INT64, so it is skipped.
    let (ep, e) = temp_csv("rel_edges", "from,to\n1,2\n2,3\n");
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE REL TABLE E(FROM N TO N)").unwrap();
    c.execute(&format!(
        "LOAD WITH HEADERS (id INT64) FROM \"{n}\" CREATE (:N {{id: id}})"
    ))
    .unwrap();
    c.execute(&format!(
        "LOAD WITH HEADERS (src INT64, dst INT64) FROM \"{e}\" \
         MATCH (a:N), (b:N) WHERE a.id = src AND b.id = dst CREATE (a)-[:E]->(b)"
    ))
    .unwrap();
    assert_eq!(
        c.execute("MATCH (:N)-[:E]->(:N) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    let _ = std::fs::remove_file(&np);
    let _ = std::fs::remove_file(&ep);
}

#[test]
fn load_from_honors_delim_and_explicit_no_header() {
    // `;`-delimited, no header row; `header=false` suppresses auto-detection.
    let (path, p) = temp_csv("delim", "1;x\n2;y\n");
    let db = Database::new();
    let c = db.connect();
    assert_eq!(
        sorted_rows(
            &c,
            &format!(
                "LOAD WITH HEADERS (id INT64, s STRING) FROM \"{p}\" \
                 (delim = ';', header = false) RETURN id, s"
            ),
        ),
        vec!["1|x", "2|y"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_bare_detects_pipe_and_skips_header() {
    // Bare LOAD (no declared types): the pipe delimiter is auto-detected, the
    // header row is skipped, columns are named from it, and every column is
    // STRING (value-type inference is out of scope).
    let (path, p) = temp_csv("bare_pipe", "name|age\nAlice|30\nBob|25\n");
    let db = Database::new();
    let c = db.connect();
    // Columns are addressable by their header names; the raw strings come back.
    assert_eq!(
        sorted_rows(&c, &format!("LOAD FROM \"{p}\" RETURN name, age")),
        vec!["Alice|30", "Bob|25"]
    );
    // The header is not emitted as data (2 rows, not 3).
    assert_eq!(
        c.execute(&format!("LOAD FROM \"{p}\" RETURN count(*)"))
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_bare_no_header_names_columns_positionally() {
    // `header=false`: every row is data; columns are named column0, column1, …
    let (path, p) = temp_csv("bare_nohdr", "10,x\n20,y\n");
    let db = Database::new();
    let c = db.connect();
    assert_eq!(
        sorted_rows(
            &c,
            &format!("LOAD FROM \"{p}\" (header=false) RETURN column0, column1"),
        ),
        vec!["10|x", "20|y"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_bare_detects_quote_protecting_delimiter() {
    // `;`-delimited with single-quoted fields. Detection must pick ';' AND the
    // `'` quote, so the `;` inside the quoted field is not a separator and the
    // quotes are stripped — proving the ever-quoted tie-break works.
    let (path, p) = temp_csv("bare_squote", "a;b\n1;'hello;world'\n");
    let db = Database::new();
    let c = db.connect();
    assert_eq!(
        c.execute(&format!("LOAD FROM \"{p}\" RETURN a, b"))
            .unwrap()
            .rendered_rows(),
        vec!["1|hello;world"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_csv_options_serial_and_multiline_rows() {
    let (path, p) = temp_csv("copy_serial", "Alice\nBob\n");
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE S(id SERIAL, name STRING, PRIMARY KEY(id))")
        .unwrap();
    c.execute(&format!("COPY S FROM \"{p}\" (header=false)"))
        .unwrap();
    assert_eq!(
        sorted_rows(&c, "MATCH (s:S) RETURN s.id, s.name"),
        vec!["0|Alice", "1|Bob"]
    );
    let _ = std::fs::remove_file(&path);

    let (path, p) = temp_csv("copy_multiline", "id,txt\n1,\"hello\nworld\"\n");
    c.execute("CREATE NODE TABLE M(id INT64, txt STRING, PRIMARY KEY(id))")
        .unwrap();
    let copy_err = c.execute(&format!("COPY M FROM \"{p}\"")).unwrap_err();
    assert!(
        copy_err
            .to_string()
            .contains("Quoted newlines are not supported")
    );
    c.execute(&format!("COPY M FROM \"{p}\" (parallel=false)"))
        .unwrap();
    assert_eq!(
        c.execute("MATCH (m:M) RETURN m.txt")
            .unwrap()
            .rendered_rows(),
        vec!["hello\nworld"]
    );

    let load_err = c
        .execute(&format!(
            "LOAD WITH HEADERS (id INT64, txt STRING) FROM \"{p}\" RETURN count(*)"
        ))
        .unwrap_err();
    assert!(
        load_err
            .to_string()
            .contains("Quoted newlines are not supported")
    );
    assert_eq!(
        c.execute(&format!(
            "LOAD WITH HEADERS (id INT64, txt STRING) FROM \"{p}\" \
             (parallel=false) RETURN count(*)"
        ))
        .unwrap()
        .rendered_rows(),
        vec!["1"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_zero_input_serial_counts_physical_rows() {
    let (path, p) = temp_csv("copy_zero_serial_placeholders", "99\n100\n101");
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE Z(id SERIAL, PRIMARY KEY(id))")
        .unwrap();
    c.execute(&format!("COPY Z FROM \"{p}\"")).unwrap();
    assert_eq!(
        c.execute("MATCH (z:Z) RETURN count(*), sum(z.id)")
            .unwrap()
            .rendered_rows(),
        vec!["3|3"]
    );
    let _ = std::fs::remove_file(&path);

    let (path, p) = temp_csv("copy_zero_serial_blank_rows", "\n\n");
    c.execute("CREATE NODE TABLE B(id SERIAL, PRIMARY KEY(id))")
        .unwrap();
    c.execute(&format!("COPY B FROM \"{p}\"")).unwrap();
    assert_eq!(
        c.execute("MATCH (b:B) RETURN count(*), sum(b.id)")
            .unwrap()
            .rendered_rows(),
        vec!["2|1"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn csv_options_are_validated() {
    let (path, p) = temp_csv("copy_options", "id\n1\n");
    let tsv = std::env::temp_dir().join("koko_load_options.tsv");
    std::fs::write(&tsv, "1\n").unwrap();
    let t = tsv.to_string_lossy().replace('\\', "/");
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();

    assert!(
        c.execute(&format!("COPY P FROM \"{p}\" (unknown=true)"))
            .unwrap_err()
            .to_string()
            .contains("Unrecognized csv parsing option")
    );
    // IGNORE_ERRORS is accepted (bad rows skip with a warning) — into a
    // scratch table so the later COPY P isn't a duplicate-PK.
    c.execute("CREATE NODE TABLE P2(id INT64, PRIMARY KEY(id))")
        .unwrap();
    assert!(
        c.execute(&format!("COPY P2 FROM \"{p}\" (ignore_errors=true)"))
            .is_ok()
    );
    assert!(
        c.execute(&format!(
            "LOAD WITH HEADERS (id INT64) FROM \"{p}\" (null_strings='x') RETURN id"
        ))
        .unwrap_err()
        .to_string()
        .contains("STRING[]")
    );
    assert!(
        c.execute(&format!("COPY P FROM \"{t}\""))
            .unwrap_err()
            .to_string()
            .contains("Cannot load from file type tsv")
    );
    c.execute(&format!(
        "COPY P FROM \"{t}\" (file_format='csv', header=false)"
    ))
    .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&tsv);
}

#[test]
fn decorrelation_plan_contracts_cover_activation_and_every_safety_gate() {
    fn node_matches(node: &crate::result::PlanNode, operator: &str, detail: Option<&str>) -> bool {
        (node.operator() == operator
            && detail.is_none_or(|expected| {
                node.detail()
                    .iter()
                    .any(|(_, value)| value.contains(expected))
            }))
            || node
                .children()
                .iter()
                .any(|child| node_matches(child, operator, detail))
    }

    fn explain_has(
        connection: &Connection,
        query: &str,
        operator: &str,
        detail: Option<&str>,
    ) -> bool {
        let result = connection.execute(&format!("EXPLAIN {query}")).unwrap();
        result
            .plan()
            .expect("EXPLAIN returns a plan")
            .roots()
            .iter()
            .any(|root| node_matches(root, operator, detail))
    }

    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .execute("CREATE REL TABLE K(FROM P TO P)")
        .unwrap();
    connection
        .execute("UNWIND range(1, 1100) AS i CREATE (:P {id: i})")
        .unwrap();

    let optional = "MATCH (a:P) OPTIONAL MATCH (a)-[:K]->(b:P) RETURN count(*)";
    assert!(
        explain_has(&connection, optional, "HashJoin", Some("Left")),
        "a large single-key OPTIONAL must decorrelate to a Left hash join"
    );

    let exists = "MATCH (a:P) WHERE EXISTS { MATCH (a)-[:K]->(b:P) } RETURN count(*)";
    assert!(
        explain_has(&connection, exists, "HashJoin", Some("Mark")),
        "a large eligible EXISTS must decorrelate to a Mark hash join"
    );
    let count = "MATCH (a:P) WHERE COUNT { MATCH (a)-[:K]->(b:P) } > 0 RETURN count(*)";
    assert!(
        explain_has(&connection, count, "HashJoin", Some("Mark")),
        "a large eligible COUNT must decorrelate to a Mark hash join"
    );

    let selective = "MATCH (a:P {id: 1}) OPTIONAL MATCH (a)-[:K]->(b:P) RETURN count(*)";
    assert!(
        explain_has(&connection, selective, "Optional", None),
        "a selective outer probe stays seeded"
    );
    let optional_predicate =
        "MATCH (a:P) OPTIONAL MATCH (a)-[:K]->(b:P) WHERE b.id > a.id RETURN count(*)";
    assert!(
        explain_has(&connection, optional_predicate, "Optional", None),
        "an OPTIONAL predicate blocks decorrelation"
    );
    let optional_path = "MATCH (a:P) OPTIONAL MATCH p = (a)-[:K]->(b:P) RETURN count(*)";
    assert!(
        explain_has(&connection, optional_path, "Optional", None),
        "a named OPTIONAL path blocks decorrelation"
    );
    let optional_two_keys = "MATCH (a:P), (c:P) OPTIONAL MATCH (a)-[:K]->(c) RETURN count(*)";
    assert!(
        explain_has(&connection, optional_two_keys, "Optional", None),
        "a multi-key OPTIONAL correlation stays seeded"
    );
    let subquery_predicate =
        "MATCH (a:P) WHERE EXISTS { MATCH (a)-[:K]->(b:P) WHERE b.id > a.id } RETURN count(*)";
    assert!(
        explain_has(&connection, subquery_predicate, "Subquery", None),
        "a correlated subquery predicate blocks decorrelation"
    );
    let subquery_path = "MATCH (a:P) WHERE EXISTS { MATCH p = (a)-[:K]->(b:P) } RETURN count(*)";
    assert!(
        explain_has(&connection, subquery_path, "Subquery", None),
        "a named subquery path blocks decorrelation"
    );

    connection
        .execute("CALL enable_plan_optimizer=false")
        .unwrap();
    assert!(
        explain_has(&connection, optional, "Optional", None)
            && !explain_has(&connection, optional, "HashJoin", Some("Left")),
        "optimizer-disabled OPTIONAL must use the seeded plan"
    );
    assert!(
        explain_has(&connection, exists, "Subquery", None)
            && !explain_has(&connection, exists, "HashJoin", Some("Mark")),
        "optimizer-disabled EXISTS must use the seeded plan"
    );
}

#[test]
fn explain_preserves_reading_clause_order() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();

    let result = connection
        .execute(
            "EXPLAIN MATCH (p:P) UNWIND [p] AS q \
             CALL db_version() YIELD version RETURN q.id, version",
        )
        .unwrap();
    let roots = result.plan().expect("EXPLAIN returns a plan").roots();
    let query_part = &roots[0].children()[0];
    let cross_product = &query_part.children()[0];
    assert_eq!(cross_product.operator(), "CrossProduct");

    let [unwind, table_function] = cross_product.children() else {
        panic!("CALL must be the right source of the cross product");
    };
    assert_eq!(unwind.operator(), "Unwind");
    assert_eq!(table_function.operator(), "TableFunctionScan");
    assert_eq!(unwind.children()[0].operator(), "MaterializeValues");
    assert_eq!(
        unwind.children()[0].children()[0].operator(),
        "NodeScan",
        "MATCH must feed node materialization, then UNWIND, before CALL"
    );
}
