use super::*;

#[test]
fn writer_sees_own_writes_reader_does_not() {
    // Snapshot isolation across two connections over one shared database.
    let db = Database::new();
    let w = db.connect();
    let r = db.connect();
    w.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    w.execute("CREATE (:P {id: 1})").unwrap();

    w.execute("BEGIN").unwrap();
    w.execute("CREATE (:P {id: 2})").unwrap();
    // The writer sees its own uncommitted insert...
    assert_eq!(
        w.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    // ...while another connection still sees only committed state.
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    w.execute("COMMIT").unwrap();
    // After COMMIT the reader sees it.
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
}

#[test]
fn read_only_txn_is_frozen_at_begin() {
    // A READ ONLY transaction reads a snapshot frozen at BEGIN: a write another
    // connection commits while it is open stays invisible until it ends. This is
    // the version-record MVCC property (the removed clone gave read-write txns the
    // same isolation; MVCC extends it to READ ONLY with no clone) — and the corpus
    // doesn't pin it (the `basic.test` cases are checkpoint-gated), so pin it here.
    let db = Database::new();
    let ro = db.connect();
    let w = db.connect();
    w.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    w.execute("CREATE (:P {id: 1})").unwrap();

    ro.execute("BEGIN READ ONLY").unwrap();
    let count = |q: &Connection| {
        q.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows()
    };
    assert_eq!(count(&ro), vec!["1"]);

    // Another connection commits a new node (auto-commit) mid-transaction.
    w.execute("CREATE (:P {id: 2})").unwrap();
    // The READ ONLY transaction still sees its BEGIN snapshot, not the new commit.
    assert_eq!(count(&ro), vec!["1"]);

    ro.execute("COMMIT").unwrap();
    // Once the READ ONLY transaction ends, a fresh read sees the latest commit.
    assert_eq!(count(&ro), vec!["2"]);
}

#[test]
fn rollback_discards_writes() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, age INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:P {id: 1, age: 10})").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("CREATE (:P {id: 2, age: 20})").unwrap();
    c.execute("MATCH (p:P) WHERE p.id = 1 SET p.age = 99")
        .unwrap();
    // Within the transaction: two nodes, and id=1's age is now 99.
    assert_eq!(
        sorted_rows(&c, "MATCH (p:P) RETURN p.id, p.age"),
        vec!["1|99", "2|20"]
    );
    c.execute("ROLLBACK").unwrap();
    // After ROLLBACK: back to the single committed node with age 10.
    assert_eq!(
        c.execute("MATCH (p:P) RETURN p.id, p.age")
            .unwrap()
            .rendered_rows(),
        vec!["1|10"]
    );
}

#[test]
fn ddl_in_txn_rolls_back() {
    // DDL inside a transaction is isolated and fully rolled back when the
    // snapshot is dropped: a table created in the txn vanishes, a table dropped
    // in the txn comes back, and a committed DDL persists.
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE Keep(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:Keep {id: 1})").unwrap();

    // CREATE TABLE in a txn, then roll back: the table is gone.
    c.execute("BEGIN").unwrap();
    c.execute("CREATE NODE TABLE Tmp(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:Tmp {id: 9})").unwrap();
    assert_eq!(
        c.execute("MATCH (t:Tmp) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    c.execute("ROLLBACK").unwrap();
    let err = c.execute("MATCH (t:Tmp) RETURN count(*)").unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");

    // DROP TABLE in a txn, then roll back: the table and its row come back.
    c.execute("BEGIN").unwrap();
    c.execute("DROP TABLE Keep").unwrap();
    c.execute("ROLLBACK").unwrap();
    assert_eq!(
        c.execute("MATCH (k:Keep) RETURN k.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );

    // COMMIT of a DDL-in-txn persists it.
    c.execute("BEGIN").unwrap();
    c.execute("CREATE NODE TABLE Persisted(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.execute("MATCH (p:Persisted) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn auto_commit_write_is_atomic() {
    // A single auto-commit statement that fails partway leaves no partial
    // effect: the duplicate-PK error rolls back the rows created before it.
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let err = c
        .execute("UNWIND [1, 2, 1] AS x CREATE (:P {id: x})")
        .unwrap_err();
    assert!(
        err.to_string().contains("duplicated primary key value"),
        "{err}"
    );
    // None of the three creates persisted.
    assert_eq!(
        c.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn read_only_txn_rejects_writes() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("BEGIN READ ONLY").unwrap();
    // Reads are fine inside a READ ONLY transaction.
    c.execute("MATCH (p:P) RETURN count(*)").unwrap();
    // A write is rejected with the C++ engine's exact (prefix-less) wording.
    let err = c.execute("CREATE (:P {id: 1})").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Can not execute a write query inside a read-only transaction."
    );
    // The rejection does NOT abort the transaction (nothing ran): it's still
    // active, so a nested BEGIN errors rather than starting a new one.
    let err = c.execute("BEGIN READ ONLY").unwrap_err();
    assert_eq!(err.to_string(), ACTIVE_TRANSACTION_MSG);
}

#[test]
fn nextval_is_a_write_rejected_in_read_only() {
    // `nextval` advances sequence state, so it's a write: rejected in a READ
    // ONLY transaction (the `currval` read is fine, and is undefined since the
    // rejected nextval never ran).
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE SEQUENCE s").unwrap();
    c.execute("BEGIN READ ONLY").unwrap();
    let err = c.execute("RETURN nextval('s')").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Can not execute a write query inside a read-only transaction."
    );
    c.execute("ROLLBACK").unwrap();
    // Outside a READ ONLY txn, nextval works (auto-commit write).
    assert_eq!(
        c.execute("RETURN nextval('s')").unwrap().rendered_rows(),
        vec!["1"]
    );
}

#[test]
fn delete_on_empty_database_is_a_noop() {
    // An unlabeled `MATCH (n)` over a database with no node tables matches
    // nothing — DELETE is a no-op, not an error.
    let db = Database::new();
    let c = db.connect();
    c.execute("MATCH (n) DELETE n").unwrap();
    c.execute("MATCH (n) DETACH DELETE n").unwrap();
    assert_eq!(
        c.execute("MATCH (n) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn single_writer_across_connections() {
    let db = Database::new();
    let c1 = db.connect();
    let c2 = db.connect();
    c1.execute("CREATE NODE TABLE T(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c1.execute("BEGIN").unwrap();
    // A second connection can't start a write transaction while c1 holds the slot.
    let err = c2.execute("BEGIN").unwrap_err();
    assert!(
        err.to_string()
            .contains("Only one write transaction at a time"),
        "{err}"
    );
    // A READ ONLY transaction is exempt.
    c2.execute("BEGIN READ ONLY").unwrap();
    c2.execute("ROLLBACK").unwrap();
    // After c1 commits, c2 can take a write transaction.
    c1.execute("COMMIT").unwrap();
    c2.execute("BEGIN").unwrap();
    c2.execute("ROLLBACK").unwrap();
    // An auto-commit write also takes the slot: blocked while c2's write txn is open.
    c2.execute("BEGIN").unwrap();
    let err = c1.execute("CREATE (:T {id: 1})").unwrap_err();
    assert!(
        err.to_string()
            .contains("Only one write transaction at a time"),
        "{err}"
    );
    c2.execute("ROLLBACK").unwrap();
}

#[test]
fn multi_writes_bypasses_single_writer() {
    let db = Database::new();
    let c1 = db.connect();
    let c2 = db.connect();
    c1.execute("CALL debug_enable_multi_writes=true").unwrap();
    c1.execute("BEGIN").unwrap();
    // With the knob on, a second writer is allowed.
    c2.execute("BEGIN").unwrap();
    c1.execute("ROLLBACK").unwrap();
    c2.execute("ROLLBACK").unwrap();
}

#[test]
fn multi_writes_hide_uncommitted_rows_and_conflict_on_same_pk() {
    let db = Database::new();
    let c1 = db.connect();
    let c2 = db.connect();
    let reader = db.connect();
    c1.execute("CREATE NODE TABLE P(id INT64, v INT64, PRIMARY KEY(id))")
        .unwrap();
    c1.execute("CALL debug_enable_multi_writes=true").unwrap();
    c1.execute("BEGIN").unwrap();
    c2.execute("BEGIN").unwrap();
    c1.execute("CREATE (:P {id: 1, v: 10})").unwrap();

    assert_eq!(
        c1.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    assert_eq!(
        c2.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    assert_eq!(
        reader
            .execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );

    let err = c2.execute("CREATE (:P {id: 1, v: 20})").unwrap_err();
    assert!(
        err.to_string().contains("duplicated primary key")
            || err.to_string().contains("Write-write conflict"),
        "{err}"
    );
    c1.execute("COMMIT").unwrap();
    assert_eq!(
        reader
            .execute("MATCH (p:P) RETURN p.id, p.v")
            .unwrap()
            .rendered_rows(),
        vec!["1|10"]
    );
}

#[test]
fn multi_writes_detect_same_row_update_conflict() {
    let db = Database::new();
    let c1 = db.connect();
    let c2 = db.connect();
    c1.execute("CREATE NODE TABLE P(id INT64, v INT64, PRIMARY KEY(id))")
        .unwrap();
    c1.execute("CREATE (:P {id: 1, v: 0})").unwrap();
    c1.execute("CALL debug_enable_multi_writes=true").unwrap();
    c1.execute("BEGIN").unwrap();
    c2.execute("BEGIN").unwrap();
    c1.execute("MATCH (p:P {id: 1}) SET p.v = 1").unwrap();
    let err = c2.execute("MATCH (p:P {id: 1}) SET p.v = 2").unwrap_err();
    assert!(err.to_string().contains("Write-write conflict"), "{err}");
    c1.execute("ROLLBACK").unwrap();
}

#[test]
fn pure_dml_multi_write_commit_does_not_replace_catalog_snapshot() {
    let db = Database::new();
    let c1 = db.connect();
    let c2 = db.connect();
    c1.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c1.execute("CALL debug_enable_multi_writes=true").unwrap();
    c1.execute("BEGIN").unwrap();
    c2.execute("BEGIN").unwrap();
    c1.execute("CREATE (:P {id: 1})").unwrap();
    c2.execute("CREATE NODE TABLE Q(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c2.execute("COMMIT").unwrap();
    c1.execute("COMMIT").unwrap();
    assert_eq!(
        c1.execute("MATCH (q:Q) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn nested_nextval_positions_are_read_only_writes() {
    let (path, p) = temp_csv("nextval_load_where", "id\n1\n");
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE SEQUENCE s").unwrap();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:P {id: 1})").unwrap();
    c.execute("BEGIN READ ONLY").unwrap();

    let pred = c
        .execute("MATCH (p:P) WHERE nextval('s') > 0 RETURN p.id")
        .unwrap_err();
    assert_eq!(pred.to_string(), READ_ONLY_WRITE_MSG);

    let order = c
        .execute("MATCH (p:P) RETURN p.id ORDER BY nextval('s')")
        .unwrap_err();
    assert_eq!(order.to_string(), READ_ONLY_WRITE_MSG);

    let load = c
        .execute(&format!(
            "LOAD WITH HEADERS (id INT64) FROM \"{p}\" WHERE nextval('s') > 0 RETURN id"
        ))
        .unwrap_err();
    assert_eq!(load.to_string(), READ_ONLY_WRITE_MSG);
    c.execute("ROLLBACK").unwrap();
    assert_eq!(
        c.execute("RETURN currval('s')").unwrap_err().to_string(),
        "Catalog exception: currval: sequence \"s\" is not yet defined. To define the sequence, call nextval first."
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregates_and_nulls() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(name STRING, age INT64, PRIMARY KEY(name))")
        .unwrap();
    c.execute("CREATE (:P {name: 'a', age: 10})").unwrap();
    c.execute("CREATE (:P {name: 'b', age: 20})").unwrap();
    c.execute("CREATE (:P {name: 'c'})").unwrap(); // null age
    let r = c
        .execute("MATCH (p:P) RETURN count(*), count(p.age), sum(p.age), avg(p.age)")
        .unwrap();
    assert_eq!(r.rendered_rows(), vec!["3|2|30|15.000000".to_string()]);
}

#[test]
fn order_by_scope_and_key_type_compatibility() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, v INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:P {id: 1, v: 10})").unwrap();
    c.execute("CREATE (:P {id: 2, v: 20})").unwrap();

    let graph_key = c.execute("MATCH (p:P) RETURN p ORDER BY p").unwrap_err();
    assert!(graph_key.to_string().contains("Cannot order by p"));
    let container_key = c.execute("RETURN [1] AS xs ORDER BY xs").unwrap_err();
    assert!(
        container_key
            .to_string()
            .contains("Order by INT64[] is not supported"),
        "{container_key}"
    );
    let id_key = c
        .execute("MATCH (p:P) RETURN id(p) AS pid ORDER BY pid")
        .unwrap_err();
    assert!(
        id_key
            .to_string()
            .contains("Order by INTERNAL_ID is not supported"),
        "{id_key}"
    );
    let distinct_scope = c
        .execute("MATCH (p:P) RETURN DISTINCT p.id AS id ORDER BY p.v")
        .unwrap_err();
    assert!(
        distinct_scope
            .to_string()
            .contains("Variable p is not in scope"),
        "{distinct_scope}"
    );
    assert_eq!(
        c.execute("MATCH (p:P) RETURN count(*) AS c ORDER BY c + 1")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
}

#[test]
fn owned_parameter_execution() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(name STRING, age INT64, PRIMARY KEY(name))")
        .unwrap();
    c.execute("CREATE (:P {name: 'Alice', age: 35})").unwrap();
    c.execute("CREATE (:P {name: 'Bob', age: 20})").unwrap();
    let r = c
        .execute_with(
            "MATCH (p:P) WHERE p.age >= $min RETURN p.name",
            params! { "min" => 30 },
        )
        .unwrap();
    assert_eq!(r.rendered_rows(), vec!["Alice".to_string()]);
    // A referenced parameter with no provided value binds NULL (C++/tck:
    // `RETURN $age + avg(…)` yields one NULL row, not a binder error).
    let r = c.execute("RETURN $missing").unwrap();
    assert_eq!(r.rendered_rows(), vec!["".to_string()]);
}

#[test]
fn extend_and_node_return() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE U(name STRING, PRIMARY KEY(name))")
        .unwrap();
    c.execute("CREATE REL TABLE Follows(FROM U TO U, since INT64)")
        .unwrap();
    c.execute("CREATE (:U {name: 'Adam'})").unwrap();
    c.execute("CREATE (:U {name: 'Zoe'})").unwrap();
    c.execute("MATCH (a:U), (b:U) WHERE a.name = 'Adam' AND b.name = 'Zoe' CREATE (a)-[:Follows {since: 1990}]->(b)")
        .unwrap();
    let r = c
        .execute("MATCH (a:U)-[e:Follows]->(b:U) RETURN a.name, b.name, e.since")
        .unwrap();
    assert_eq!(r.rendered_rows(), vec!["Adam|Zoe|1990".to_string()]);
}

// --- D-6: PreparedStatement + RAII transaction + params! ---

#[test]
fn prepared_statement_reuse() {
    // Prepare once, execute many times with different parameter values — both
    // a write statement and a read statement.
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, name STRING, PRIMARY KEY(id))")
        .unwrap();

    let mut insert = c.prepare("CREATE (:P {id: $id, name: $name})").unwrap();
    insert
        .execute_with(params! { "id" => 1, "name" => "Alice" })
        .unwrap();
    insert
        .execute_with(params! { "id" => 2, "name" => "Bob" })
        .unwrap();
    insert
        .execute_with(params! { "id" => 3, "name" => "Cara" })
        .unwrap();

    // A read prepared once and run with different params returns different rows.
    let mut by_id = c
        .prepare("MATCH (p:P) WHERE p.id = $id RETURN p.name")
        .unwrap();
    assert_eq!(
        by_id
            .execute_with(params! { "id" => 2 })
            .unwrap()
            .rendered_rows(),
        vec!["Bob"]
    );
    assert_eq!(
        by_id
            .execute_with(params! { "id" => 3 })
            .unwrap()
            .rendered_rows(),
        vec!["Cara"]
    );
    assert!(
        by_id
            .execute_with(params! { "id" => 99 })
            .unwrap()
            .rendered_rows()
            .is_empty()
    );
}

#[test]
fn prepare_reports_parse_error_and_execute_reports_missing_param() {
    let db = Database::new();
    let c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    // A parse error surfaces at prepare time (before any execute).
    let err = c.prepare("MATCH (((").unwrap_err();
    assert!(matches!(err, Error::Parser(_)), "{err}");
    // Binding happens per execute; an unsupplied parameter binds NULL like
    // the C++ engine, so the filter drops every row rather than erroring.
    let mut stmt = c
        .prepare("MATCH (p:P) WHERE p.id = $id RETURN p.id")
        .unwrap();
    let r = stmt.execute().unwrap();
    assert_eq!(r.rendered_rows(), Vec::<String>::new());
}

#[test]
fn prepared_metadata_rebinds_after_catalog_change() {
    let db = Database::new();
    let connection = db.connect();
    connection
        .execute("CREATE NODE TABLE P(id INT64, name STRING, PRIMARY KEY(id))")
        .unwrap();
    connection
        .execute("CREATE (:P {id: 1, name: 'Alice'})")
        .unwrap();

    let mut statement = connection
        .prepare_with(
            "MATCH (p:P) WHERE p.id = $id OR p.id = $id RETURN p.name AS name",
            params! { "id" => 1 },
        )
        .unwrap();
    assert_eq!(
        statement.parameters(),
        vec![ParameterInfo::new("id".to_string(), LogicalType::Int64)]
    );
    assert_eq!(
        statement.columns(),
        vec![Column::new("name".to_string(), LogicalType::String,)]
    );
    assert_eq!(statement.kind(), StatementKind::Query);
    assert!(statement.is_read_only());
    assert_eq!(
        statement
            .execute_with(params! { "id" => 1 })
            .unwrap()
            .rendered_rows(),
        vec!["Alice"]
    );

    connection.execute("DROP TABLE P").unwrap();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let error = statement.execute_with(params! { "id" => 1 }).unwrap_err();
    assert!(matches!(error, Error::Binder(_)), "{error}");
}

#[test]
fn prepared_statement_reports_write_classification() {
    let db = Database::new();
    let connection = db.connect();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let insert = connection.prepare("CREATE (:P {id: $id})").unwrap();
    assert_eq!(insert.kind(), StatementKind::Query);
    assert!(!insert.is_read_only());
}

#[test]
fn prepared_metadata_infers_parameters_and_validates_execution_values() {
    let db = Database::new();
    let connection = db.connect();
    connection
        .execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.execute("CREATE (:P {id: 7})").unwrap();

    let mut statement = connection
        .prepare(
            "MATCH (p:P) WHERE p.id = $key OR p.id = $key \
             RETURN p.id AS id, $free AS free",
        )
        .unwrap();
    assert_eq!(
        statement.parameters(),
        vec![
            ParameterInfo::new("key".to_string(), LogicalType::Int64),
            ParameterInfo::new("free".to_string(), LogicalType::Any),
        ]
    );
    assert_eq!(
        statement.columns(),
        vec![
            Column::new("id".to_string(), LogicalType::Int64),
            Column::new("free".to_string(), LogicalType::Any),
        ]
    );

    let extra = statement
        .execute_with(params! { "key" => 7, "free" => true, "unused" => 1 })
        .unwrap_err();
    assert!(extra.to_string().contains("Unexpected"), "{extra}");
    assert!(
        statement
            .execute_with(params! { "key" => "seven", "free" => true })
            .is_err()
    );
    assert_eq!(
        statement
            .execute_with(params! { "key" => 7, "free" => true })
            .unwrap()
            .rendered_rows(),
        vec!["7|True"]
    );
    assert_eq!(statement.parameters()[1].logical_type(), &LogicalType::Bool);
    assert_eq!(statement.columns()[1].logical_type(), &LogicalType::Bool);

    let conflict = connection
        .prepare("MATCH (p:P) WHERE p.id = $same RETURN NOT $same")
        .unwrap_err();
    assert!(matches!(conflict, Error::Binder(_)), "{conflict}");
}

#[test]
fn params_macro_mixed_types() {
    // The params! macro converts bare Rust scalars via Value's From impls.
    let db = Database::new();
    let c = db.connect();
    c.execute(
        "CREATE NODE TABLE P(name STRING, age INT64, ratio DOUBLE, ok BOOL, PRIMARY KEY(name))",
    )
    .unwrap();
    c.execute_with(
        "CREATE (:P {name: $n, age: $a, ratio: $r, ok: $b})",
        params! { "n" => "Alice", "a" => 30, "r" => 1.5, "b" => true },
    )
    .unwrap();
    let r = c
        .execute("MATCH (p:P) RETURN p.name, p.age, p.ratio, p.ok")
        .unwrap();
    assert_eq!(r.rendered_rows(), vec!["Alice|30|1.500000|True"]);
}

#[test]
fn raii_transaction_commit_visible_to_others() {
    let db = Database::new();
    let mut w = db.connect();
    let r = db.connect();
    w.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let tx = w.transaction().unwrap();
    tx.execute("CREATE (:P {id: 1})").unwrap();
    tx.execute("CREATE (:P {id: 2})").unwrap();
    // Another connection sees nothing until commit.
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    tx.commit().unwrap();
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
}

#[test]
fn raii_transaction_drop_rolls_back() {
    // Dropping the guard without committing rolls the transaction back.
    let db = Database::new();
    let mut c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:P {id: 1})").unwrap();
    {
        let tx = c.transaction().unwrap();
        tx.execute("CREATE (:P {id: 2})").unwrap();
        // The write is visible within the transaction...
        assert_eq!(
            tx.execute("MATCH (p:P) RETURN count(*)")
                .unwrap()
                .rendered_rows(),
            vec!["2"]
        );
        // ...then the guard drops here without a commit.
    }
    // Rolled back to the single committed node.
    assert_eq!(
        c.execute("MATCH (p:P) RETURN p.id")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
}

#[test]
fn raii_transaction_explicit_rollback() {
    let db = Database::new();
    let mut c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let tx = c.transaction().unwrap();
    tx.execute("CREATE (:P {id: 1})").unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        c.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn raii_transaction_frees_writer_slot_on_drop() {
    // Dropping a transaction guard releases the single-writer slot, so another
    // connection can immediately start its own write transaction.
    let db = Database::new();
    let mut c1 = db.connect();
    let mut c2 = db.connect();
    c1.execute("CREATE NODE TABLE T(id INT64, PRIMARY KEY(id))")
        .unwrap();
    {
        let tx = c1.transaction().unwrap();
        tx.execute("CREATE (:T {id: 1})").unwrap();
        // While c1 holds the slot, c2 cannot start a write transaction.
        let err = c2.transaction().unwrap_err();
        assert!(
            err.to_string()
                .contains("Only one write transaction at a time"),
            "{err}"
        );
        // tx drops here (rollback, slot freed).
    }
    // Now c2 can take the slot.
    let tx2 = c2.transaction().unwrap();
    tx2.rollback().unwrap();
    assert_eq!(
        c1.execute("MATCH (t:T) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
}

#[test]
fn raii_read_transaction() {
    let db = Database::new();
    let mut c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let tx = c.read_transaction().unwrap();
    // Reads work; a write is rejected with the engine's exact wording.
    tx.execute("MATCH (p:P) RETURN count(*)").unwrap();
    let err = tx.execute("CREATE (:P {id: 1})").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Can not execute a write query inside a read-only transaction."
    );
    // The rejection did not abort the txn; commit ends it cleanly.
    tx.commit().unwrap();
    // A read-only transaction took no writer slot, so writes work afterwards.
    c.execute("CREATE (:P {id: 1})").unwrap();
}

#[test]
fn prepared_statement_within_raii_transaction() {
    // A prepared statement composes with an RAII transaction: its executes run
    // inside the transaction (isolated from other connections) until commit.
    let db = Database::new();
    let mut w = db.connect();
    let r = db.connect();
    w.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let tx = w.transaction().unwrap();
    let mut ins = tx.prepare("CREATE (:P {id: $id})").unwrap();
    ins.execute_with(params! { "id" => 1 }).unwrap();
    ins.execute_with(params! { "id" => 2 }).unwrap();
    // The writer sees its own uncommitted inserts...
    assert_eq!(
        tx.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
    // ...the reader sees committed (empty) state.
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["0"]
    );
    tx.commit().unwrap();
    assert_eq!(
        r.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["2"]
    );
}

#[test]
fn raii_transaction_drop_after_inner_error_is_safe() {
    // A statement error inside the transaction auto-aborts it (freeing the
    // writer slot). The guard's drop then issues a no-op ROLLBACK that must not
    // panic, double-free the slot, or disturb committed state.
    let db = Database::new();
    let mut c = db.connect();
    c.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    c.execute("CREATE (:P {id: 1})").unwrap();
    {
        let tx = c.transaction().unwrap();
        tx.execute("CREATE (:P {id: 2})").unwrap();
        // Duplicate PK → runtime error → aborts the transaction.
        let err = tx.execute("CREATE (:P {id: 1})").unwrap_err();
        assert!(
            err.to_string().contains("duplicated primary key value"),
            "{err}"
        );
        // tx drops here; its ROLLBACK is a no-op (the txn is already aborted).
    }
    // The aborted transaction discarded id=2; only the committed id=1 remains.
    assert_eq!(
        c.execute("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .rendered_rows(),
        vec!["1"]
    );
    // The writer slot was freed: a fresh auto-commit write succeeds.
    c.execute("CREATE (:P {id: 3})").unwrap();
    assert_eq!(sorted_rows(&c, "MATCH (p:P) RETURN p.id"), vec!["1", "3"]);
}
