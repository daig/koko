use super::*;

#[test]
fn end_to_end_create_match_return() {
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))")
        .unwrap();
    c.query("CREATE (:Person {name: 'Alice', age: 35})")
        .unwrap();
    c.query("CREATE (:Person {name: 'Bob', age: 20})").unwrap();
    let r = c
        .query("MATCH (p:Person) WHERE p.age > 30 RETURN p.name, p.age")
        .unwrap();
    assert_eq!(r.num_rows(), 1);
    assert_eq!(r.value(0, 0).unwrap().as_str(), Some("Alice"));
    assert_eq!(r.get_row_i64(0, 1), 35);
}

#[test]
fn query_result_preserves_typed_batches_and_checked_views() {
    let db = Database::in_memory();
    let connection = db.connect();
    let result = connection
        .query(&format!(
            "UNWIND range(0, {}) AS i RETURN i",
            VECTOR_CAPACITY + 2
        ))
        .unwrap();

    assert_eq!(
        result.schema(),
        &[ColumnSchema::new("i".to_string(), LogicalType::Int64)]
    );
    assert_eq!(result.num_rows(), VECTOR_CAPACITY + 3);
    assert_eq!(result.batches().len(), 2);
    assert_eq!(result.batches()[0].size(), VECTOR_CAPACITY);
    assert_eq!(result.batches()[1].size(), 3);

    let integers = result.typed_column_by_name::<i64>("i").unwrap();
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
    assert!(result.column_by_name("missing").is_err());
    assert!(result.typed_column::<String>(0).is_err());

    let boundary = result.rows().nth(VECTOR_CAPACITY).unwrap();
    assert_eq!(boundary.get::<i64>(0).unwrap(), VECTOR_CAPACITY as i64);
    assert_eq!(
        boundary.get_by_name::<i64>("i").unwrap(),
        VECTOR_CAPACITY as i64
    );
}

#[test]
fn named_result_access_rejects_ambiguous_columns() {
    let result = Database::in_memory()
        .connect()
        .query("RETURN 1 AS x, 2 AS x")
        .unwrap();
    let error = match result.column_by_name("x") {
        Err(error) => error,
        Ok(_) => panic!("duplicate result names must be ambiguous"),
    };
    assert!(error.to_string().contains("ambiguous"));
    assert!(
        result
            .rows()
            .next()
            .unwrap()
            .get_by_name::<i64>("x")
            .is_err()
    );
}

#[test]
fn columnar_result_tail_preserves_distinct_order_and_union() {
    let db = Database::in_memory();
    let connection = db.connect();

    let ordered = connection
        .query("UNWIND [3, 1, 2, 2] AS x RETURN DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 2")
        .unwrap();
    assert_eq!(ordered.to_result_strings(), vec!["2", "1"]);

    let union = connection
        .query("RETURN 1 AS x UNION RETURN 1 AS x UNION RETURN 2 AS x")
        .unwrap();
    assert_eq!(union.to_result_strings(), vec!["1", "2"]);
}

#[test]
fn typed_results_use_bound_runtime_scalar_types() {
    let db = Database::in_memory();
    let result = db
        .connect()
        .query(
            "RETURN round(123, 0) AS rounded, \
             floor(2.5) AS floored, \
             list_reduce(['a:', 'b:', 'c:', 'd:'], (x, y) -> y + x) AS reduced",
        )
        .unwrap();
    assert_eq!(
        result
            .schema()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect::<Vec<_>>(),
        vec![
            LogicalType::Double,
            LogicalType::Double,
            LogicalType::String,
        ]
    );
    assert_eq!(
        result.to_result_strings(),
        vec!["123.000000|2.000000|d:c:b:a:"]
    );
}

#[test]
fn query_results_publish_duration_summaries() {
    let db = Database::in_memory();
    let connection = db.connect();
    let direct = connection.query("RETURN 42 AS answer").unwrap();
    let direct_summary = *direct.summary();
    assert_eq!(
        direct_summary.compilation_time(),
        direct_summary.compiling_time()
    );
    assert_eq!(
        direct_summary.compiling_time_ms(),
        direct_summary.compiling_time().as_secs_f64() * 1_000.0
    );
    assert_eq!(
        direct_summary.execution_time_ms(),
        direct_summary.execution_time().as_secs_f64() * 1_000.0
    );

    let prepared = connection.prepare("RETURN $value AS answer").unwrap();
    let executed = prepared.execute(params! { "value" => 7 }).unwrap();
    assert_eq!(executed.to_result_strings(), vec!["7"]);
    assert!(executed.summary().compiling_time_ms().is_finite());
    assert!(executed.summary().execution_time_ms().is_finite());
}

/// Factorization (P3 step 6): an aggregate over a collapsible fan-out suffix
/// must fold the suffix into a multiplicity and produce the same numbers a
/// materialized cross-product would — `count(*)` sums the fan-out, `sum`/`avg`
/// scale the (flat) head value by it — while a *read* endpoint still fans out.
#[test]
fn factorized_aggregate_over_a_fanout() {
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE P(id INT64, age INT64, PRIMARY KEY(id))")
        .unwrap();
    c.query("CREATE REL TABLE K(FROM P TO P)").unwrap();
    for i in 0..4 {
        c.query(&format!("CREATE (:P {{id: {i}, age: {}}})", 10 + i))
            .unwrap();
    }
    // out-degrees: 0->{1,2,3} (3 edges), 1->{2} (1 edge); 4 edges total.
    for (a, b) in [(0, 1), (0, 2), (0, 3), (1, 2)] {
        c.query(&format!(
            "MATCH (a:P), (b:P) WHERE a.id = {a} AND b.id = {b} CREATE (a)-[:K]->(b)"
        ))
        .unwrap();
    }

    // count(*): `b` and the rel are unread ⇒ the extend collapses the fan-out.
    assert_eq!(
        c.query("MATCH (a:P)-[:K]->(b:P) RETURN count(*)")
            .unwrap()
            .get_row_i64(0, 0),
        4
    );
    // sum/avg fold the (flat) head value with the suffix multiplicity:
    // a=0 (age 10) ×3 + a=1 (age 11) ×1 = 41 over 4 tuples.
    assert_eq!(
        c.query("MATCH (a:P)-[:K]->(b:P) RETURN sum(a.age)")
            .unwrap()
            .get_row_i64(0, 0),
        41
    );
    assert_eq!(
        c.query("MATCH (a:P)-[:K]->(b:P) RETURN avg(a.age)")
            .unwrap()
            .to_result_strings(),
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
        c.query("MATCH (a:P)-[:K]->(b:P) WHERE a.id = 0 RETURN count(DISTINCT a.id)")
            .unwrap()
            .get_row_i64(0, 0),
        1
    );
}

#[test]
fn load_from_with_headers_read_filter_create() {
    // Header row whose names match the declared columns (auto-skipped).
    let (path, p) = temp_csv("headers", "id,name,age\n1,Alice,35\n2,Bob,20\n3,Cara,40\n");
    let db = Database::in_memory();
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
    c.query("CREATE NODE TABLE Person(id INT64, name STRING, age INT64, PRIMARY KEY(id))")
        .unwrap();
    c.query(&format!(
        "LOAD WITH HEADERS {cols} FROM \"{p}\" \
         CREATE (:Person {{id: id, name: name, age: age}})"
    ))
    .unwrap();
    assert_eq!(
        c.query("MATCH (p:Person) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
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
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.query("CREATE REL TABLE E(FROM N TO N)").unwrap();
    c.query(&format!(
        "LOAD WITH HEADERS (id INT64) FROM \"{n}\" CREATE (:N {{id: id}})"
    ))
    .unwrap();
    c.query(&format!(
        "LOAD WITH HEADERS (src INT64, dst INT64) FROM \"{e}\" \
         MATCH (a:N), (b:N) WHERE a.id = src AND b.id = dst CREATE (a)-[:E]->(b)"
    ))
    .unwrap();
    assert_eq!(
        c.query("MATCH (:N)-[:E]->(:N) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["2"]
    );
    let _ = std::fs::remove_file(&np);
    let _ = std::fs::remove_file(&ep);
}

#[test]
fn load_from_honors_delim_and_explicit_no_header() {
    // `;`-delimited, no header row; `header=false` suppresses auto-detection.
    let (path, p) = temp_csv("delim", "1;x\n2;y\n");
    let db = Database::in_memory();
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
    let db = Database::in_memory();
    let c = db.connect();
    // Columns are addressable by their header names; the raw strings come back.
    assert_eq!(
        sorted_rows(&c, &format!("LOAD FROM \"{p}\" RETURN name, age")),
        vec!["Alice|30", "Bob|25"]
    );
    // The header is not emitted as data (2 rows, not 3).
    assert_eq!(
        c.query(&format!("LOAD FROM \"{p}\" RETURN count(*)"))
            .unwrap()
            .to_result_strings(),
        vec!["2"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_bare_no_header_names_columns_positionally() {
    // `header=false`: every row is data; columns are named column0, column1, …
    let (path, p) = temp_csv("bare_nohdr", "10,x\n20,y\n");
    let db = Database::in_memory();
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
    let db = Database::in_memory();
    let c = db.connect();
    assert_eq!(
        c.query(&format!("LOAD FROM \"{p}\" RETURN a, b"))
            .unwrap()
            .to_result_strings(),
        vec!["1|hello;world"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_csv_options_serial_and_multiline_rows() {
    let (path, p) = temp_csv("copy_serial", "Alice\nBob\n");
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE S(id SERIAL, name STRING, PRIMARY KEY(id))")
        .unwrap();
    c.query(&format!("COPY S FROM \"{p}\" (header=false)"))
        .unwrap();
    assert_eq!(
        sorted_rows(&c, "MATCH (s:S) RETURN s.id, s.name"),
        vec!["0|Alice", "1|Bob"]
    );
    let _ = std::fs::remove_file(&path);

    let (path, p) = temp_csv("copy_multiline", "id,txt\n1,\"hello\nworld\"\n");
    c.query("CREATE NODE TABLE M(id INT64, txt STRING, PRIMARY KEY(id))")
        .unwrap();
    let copy_err = c.query(&format!("COPY M FROM \"{p}\"")).unwrap_err();
    assert!(
        copy_err
            .to_string()
            .contains("Quoted newlines are not supported")
    );
    c.query(&format!("COPY M FROM \"{p}\" (parallel=false)"))
        .unwrap();
    assert_eq!(
        c.query("MATCH (m:M) RETURN m.txt")
            .unwrap()
            .to_result_strings(),
        vec!["hello\nworld"]
    );

    let load_err = c
        .query(&format!(
            "LOAD WITH HEADERS (id INT64, txt STRING) FROM \"{p}\" RETURN count(*)"
        ))
        .unwrap_err();
    assert!(
        load_err
            .to_string()
            .contains("Quoted newlines are not supported")
    );
    assert_eq!(
        c.query(&format!(
            "LOAD WITH HEADERS (id INT64, txt STRING) FROM \"{p}\" \
             (parallel=false) RETURN count(*)"
        ))
        .unwrap()
        .to_result_strings(),
        vec!["1"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn copy_zero_input_serial_counts_physical_rows() {
    let (path, p) = temp_csv("copy_zero_serial_placeholders", "99\n100\n101");
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE Z(id SERIAL, PRIMARY KEY(id))")
        .unwrap();
    c.query(&format!("COPY Z FROM \"{p}\"")).unwrap();
    assert_eq!(
        c.query("MATCH (z:Z) RETURN count(*), sum(z.id)")
            .unwrap()
            .to_result_strings(),
        vec!["3|3"]
    );
    let _ = std::fs::remove_file(&path);

    let (path, p) = temp_csv("copy_zero_serial_blank_rows", "\n\n");
    c.query("CREATE NODE TABLE B(id SERIAL, PRIMARY KEY(id))")
        .unwrap();
    c.query(&format!("COPY B FROM \"{p}\"")).unwrap();
    assert_eq!(
        c.query("MATCH (b:B) RETURN count(*), sum(b.id)")
            .unwrap()
            .to_result_strings(),
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
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();

    assert!(
        c.query(&format!("COPY P FROM \"{p}\" (unknown=true)"))
            .unwrap_err()
            .to_string()
            .contains("Unrecognized csv parsing option")
    );
    // IGNORE_ERRORS is accepted (bad rows skip with a warning) — into a
    // scratch table so the later COPY P isn't a duplicate-PK.
    c.query("CREATE NODE TABLE P2(id INT64, PRIMARY KEY(id))")
        .unwrap();
    assert!(
        c.query(&format!("COPY P2 FROM \"{p}\" (ignore_errors=true)"))
            .is_ok()
    );
    assert!(
        c.query(&format!(
            "LOAD WITH HEADERS (id INT64) FROM \"{p}\" (null_strings='x') RETURN id"
        ))
        .unwrap_err()
        .to_string()
        .contains("STRING[]")
    );
    assert!(
        c.query(&format!("COPY P FROM \"{t}\""))
            .unwrap_err()
            .to_string()
            .contains("Cannot load from file type tsv")
    );
    c.query(&format!(
        "COPY P FROM \"{t}\" (file_format='csv', header=false)"
    ))
    .unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&tsv);
}
