# Koko

Koko is an in-memory, embeddable, columnar **property-graph database** written in Rust, with a
Cypher query language and an idiomatic Rust API, inspired by [Kuzu](https://github.com/kuzudb/kuzu)


## Workspace layout

The per-layer crate graph enforces the architecture's dependency layering at compile time:

```
koko-common      types · Value · typed DataChunk vectors · memory/statistics primitives       (leaf)
koko-catalog     node/rel table entries, columns, primary keys, FROM/TO                    → common
koko-storage     shared versioned typed/chunked MVCC columns/adjacency/PK/undo API       → common,catalog
koko-parser      hand-written lexer + Pratt/recursive-descent parser → AST                 → common
koko-function    generated signatures + scalar/aggregate implementation                    → common
koko-binder      name/type resolution, query graph, bound expressions                     → common,catalog,parser,function
koko-expr        expression evaluator over DataChunks                                     → common,binder,function
koko-planner     logical plan + optimizer (pushdown/join/cost)                            → common,binder,catalog
koko-loader      bounded CSV/Parquet/NPY typed-batch readers and writers                 → common,catalog,storage
koko-processor   pull operators + typed batches + controls/accounting + parallelism      → planner,binder,expr,function,storage,…
koko          Database/Config/Connection/PreparedStatement/QueryResult API             → all
koko-test-runner `.test` parser + local/full-corpus runners                               → koko
koko-cli         first-party interactive/batch `koko` terminal client                    → koko
```

## CLI quick start

```bash
cargo install --path crates/koko-cli
koko

koko --command "RETURN 42 AS answer" --format box
koko --file query.cypher --format json > report.json
printf 'RETURN 42 AS answer;\n' | koko --format jsonl
```

The no-argument TTY mode is an in-memory interactive session. Batch data is written to stdout;
diagnostics, timing, and progress are written to stderr. `koko --help` lists the canonical options
and `:help` lists interactive commands.

For a full walkthrough (build a graph, export/import, TinySNB exploration) and command reference,
see the **[REPL usage guide](docs/REPL_USAGE_GUIDE.md)**.

## Embedded API quick start

```rust
use koko::{
    Database, DatabaseConfig, LogicalType, ScalarUdfNullPolicy, Value,
};

let config = DatabaseConfig::new().with_max_workers(4)?;
let db = Database::in_memory_with_config(config)?;
let conn = db.connect();
conn.query("CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))")?;
conn.query("CREATE (:Person {name: 'Alice', age: 35})")?;

conn.register_scalar_function(
    "double_age",
    vec![LogicalType::Int64],
    LogicalType::Int64,
    ScalarUdfNullPolicy::Propagate,
    |args| Ok(Value::Int64(args[0].as_i64().unwrap() * 2)),
)?;
assert_eq!(
    conn.query("RETURN double_age(21)")?.to_result_strings(),
    vec!["42"]
);

let result = conn.query("MATCH (p:Person) WHERE p.age > 30 RETURN p.name, p.age")?;
assert_eq!(result.schema()[0].logical_type(), &LogicalType::String);
let ages = result.typed_column_by_name::<i64>("p.age")?;
assert_eq!(ages.get(0)?, 35);
for row in result.rows() {
    println!(
        "{} is {}",
        row.get_by_name::<String>("p.name")?,
        row.get_by_name::<i64>("p.age")?
    );
}
```

## Building & testing

```bash
cargo build
cargo test --workspace # unit tests + 52 hermetic P0 `.test` fixtures
cargo run -p koko-test-runner -- crates/koko-test-runner/tests/p0

# Full upstream corpus (requires the C++ checkout's datasets):
KOKO_DATASET_DIR=../koko/dataset \
  cargo run --release -p koko-test-runner -- ../koko/test/test_files/<dir>
```

`crates/koko-test-runner/tests/p1/` contains 23 historical fixtures but has no integration-test
driver and is not run by `cargo test`; use it only as source material until it is reconciled.
