# Koko

Koko is an in-memory, embeddable, columnar **property-graph database** written in Rust, with a
Cypher query language and an idiomatic Rust API.

The name **Koko** comes from Hawaiian *koko*: blood—not only the physical fluid, but a living
connection among people, ancestors, ʻāina (land), knowledge, mana, and kuleana (responsibility).
It also echoes *kōkō*, a Polynesian cognate for the traditional carrying net used to bear
calabashes and fruit. Living relationships and a net carrying connected things both fit a graph
database; the name also carries a responsibility to treat that cultural lineage with care.

Koko began as a clean-room Rust implementation of an existing C++ engine. That migration is
complete; Koko is now an independent project. Native on-disk storage remains outside the active
scope. See [`ROADMAP.md`](ROADMAP.md) for the current product map, remaining work, intentional
decisions, and deferred scope; [`docs/cpp-reference/`](docs/cpp-reference) retains historical C++
semantics notes.

## Status (2026-07-27)

**The in-memory v0, first-party CLI, facade decomposition, and idiomatic Rust API cutover are
complete.** Typed and schemaless named graphs use one Cypher/MVCC pipeline; ordered JSON,
graph-scoped HASH/ART DDL, atomic database-level logical interchange, validated query-time local
`icebug-disk`, and connection-local native scalar functions are implemented. The engine retains
typed columnar storage/results, snapshot transactions, concurrent connections, cancellation and
deadlines, tracked memory, and eager borrowed result views.

Koko is now an independent project. Supported Koko behavior is the non-regression contract. The
Ladybug 0.17 corpus, differential probes, and historical parity scorecards remain useful evidence
when a change intentionally owns compatibility, but they are not automatic specifications or
universal release gates.

**Native durability is intentionally out of scope.** The former P4 is permanently deferred until
the project owner explicitly restores it. `Database::new()` and `Database::with_config(...)` create
the in-memory product; `CHECKPOINT` is a deliberate no-op. Native database files, persistent
catalog/indexes, WAL/recovery, and physical storage introspection are not active work.

Arrow C, extension/plugin support and every extension module, projected graphs, connectors, and
foreign bindings remain owner-deferred. The first-party Rust `koko` CLI is complete.
[`docs/REPL_USAGE_GUIDE.md`](docs/REPL_USAGE_GUIDE.md) is the practical terminal-client guide;
[`docs/CLI_UX.md`](docs/CLI_UX.md) owns its observable contract.

Current sources:

1. [`ROADMAP.md`](ROADMAP.md) — current product and architecture map, remaining work, intentional
   decisions, verification policy, and deferred scope.
2. [`docs/FACADE_ARCHITECTURE.md`](docs/FACADE_ARCHITECTURE.md) — implemented crate/facade
   ownership and public API.
3. [`docs/CLI_UX.md`](docs/CLI_UX.md) and
   [`docs/CLI_ARCHITECTURE.md`](docs/CLI_ARCHITECTURE.md) — CLI behavior and architecture.
4. [`docs/REPL_USAGE_GUIDE.md`](docs/REPL_USAGE_GUIDE.md) — practical terminal workflows.

## Workspace layout

The workspace DAG makes layer ownership a compile-time property:

```
koko-common      types · Value · typed chunks/vectors · memory/statistics primitives       (leaf)
koko-catalog     private node/rel schema and catalog invariants                       → common
koko-storage     versioned typed/chunked MVCC columns/adjacency/PK/undo                → common,catalog
koko-parser      hand-written lexer + recursive-descent/Pratt parser → AST             → common
koko-function    generated function identities/signatures + scalar/aggregate execution → common
koko-ir          bound semantics · typed variable IDs · row layouts · logical plans    → common,function
koko-binder      name/type resolution and query graph                                  → common,catalog,parser,function,ir
koko-expr        compile bound expressions → column evaluator                          → common,function,ir
koko-planner     planning + pushdown/join/cost optimization                            → common,catalog,function,ir
koko-loader      CSV/Parquet/NPY input · CSV/Parquet output · external scan protocols  → common,catalog,function,storage
koko-processor   pull execution · typed chunks · controls/accounting · parallelism     → common,catalog,expr,function,ir,loader,storage
koko             public Database/Connection/Transaction/Prepared/Result facade         → all engine crates
koko-test-runner `.test` parser + hermetic/external corpus runners                      → koko,common
koko-cli         first-party interactive/batch `koko` terminal client                  → koko
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

## Embedded API quick start

```rust
use koko::function::{NullPolicy, ScalarFunction};
use koko::{Database, DatabaseConfig, LogicalType, Result, Value};

fn main() -> Result<()> {
    let config = DatabaseConfig::new().with_max_threads(4)?;
    let database = Database::with_config(config);
    let connection = database.connect();

    connection.execute(
        "CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))",
    )?;
    connection.execute("CREATE (:Person {name: 'Alice', age: 35})")?;

    let function = ScalarFunction::new(
        "double_age",
        [LogicalType::Int64],
        LogicalType::Int64,
        |arguments| Ok(Value::Int64(arguments[0].as_i64().unwrap() * 2)),
    )
    .with_null_policy(NullPolicy::Propagate);
    connection.register_scalar_function(function)?;
    assert_eq!(
        connection
            .execute("RETURN double_age(21) AS value")?
            .row(0)
            .unwrap()
            .get::<i64>("value")?,
        42
    );

    let result =
        connection.execute("MATCH (p:Person) WHERE p.age > 30 RETURN p.name, p.age")?;
    assert_eq!(result.columns()[0].logical_type(), &LogicalType::String);
    let ages = result.typed_column::<i64>("p.age")?;
    assert_eq!(ages.get(0)?, 35);
    for row in &result {
        println!(
            "{} is {}",
            row.get::<String>("p.name")?,
            row.get::<i64>("p.age")?
        );
    }
    Ok(())
}
```

## Building & testing

```bash
cargo build --workspace
cargo test --workspace
cargo test -p koko-test-runner --test product
python3 scripts/gen_fn_catalog.py --check

# Optional historical external corpus (requires the Ladybug checkout):
KOKO_DATASET_DIR=../ladybug/dataset \
  cargo run --release -p koko-test-runner -- ../ladybug/test/test_files/<dir>
```

See [`docs/TESTING.md`](docs/TESTING.md) for test ownership, fixture rules, focused gates, and
optional compatibility/performance evidence.
