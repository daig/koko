# Koko testing

Koko's standing regression suite protects Koko behavior. Ladybug comparisons are optional evidence
for changes that intentionally own inherited compatibility; they do not define expected results for
ordinary product work.

## Standing gates

Run the complete behavioral gate with:

```bash
cargo test --workspace
```

This is the authoritative regression command. It runs crate unit tests, public integration tests,
CLI subprocess and supported PTY tests, doctests, and every manifested Cypher product fixture.
There is no separate dormant fixture tier.

Quality and generated-source checks are separate because they do not execute product behavior:

```bash
python3 scripts/gen_fn_catalog.py --check
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

For a focused CLI change, run the parser/facade/CLI surfaces once each:

```bash
python3 scripts/cli_goal_gate.py --strict
```

The CI workflow runs the workspace regression suite on Linux, macOS, and Windows and runs the
quality checks on Linux with the declared Rust 1.85 minimum toolchain.

## Test layers and ownership

| Layer | Location | Owns |
|---|---|---|
| Crate unit tests | `crates/*/src/**` | Private algorithms, data structures, type rules, invariants, and failure boundaries |
| Rust integration tests | `crates/*/tests/*.rs` | Public crate contracts and cross-component behavior; especially facade and CLI process boundaries |
| Cypher product fixtures | `crates/koko-test-runner/tests/product/*.test` | End-to-end accepted Cypher, results, errors, ordering, transactions, planning safety, and loader behavior |
| Public examples and doctests | Public crate documentation | User-visible compile-time usage contracts |
| Focused gate scripts | `scripts/` | Aggregation or external protocols that are not naturally one Rust test target |

A behavior has one primary owner. Duplicate it at another layer only when that layer adds a distinct
observable boundary—for example, an engine error fixture plus a CLI exit-status assertion.

### Workspace suite map

| Crate or target | Primary regression contracts |
|---|---|
| `koko-common` | Logical/value types, typed vectors/chunks, hashes, memory/statistics primitives, temporal/decimal helpers, and CSV dialect parsing |
| `koko-catalog` | Schema definitions, table/sequence namespaces, IDs, column generation, relationship groups, and catalog invariants |
| `koko-storage` | Versioned columns/adjacency, primary-key maps, relationship routing, undo, snapshots, and storage-side constraints |
| `koko-algorithm` | Pure graph-kernel semantics, cycle/failure boundaries, cancellation cadence, dense-ID dispatch, and tracked-memory admission/release |
| `koko-parser` | Lexing, AST parsing/rendering, syntax status, spans, diagnostics, and completion contexts |
| `koko-function` | Generated registry/signatures, casts, scalar/aggregate values, NULL policy, ordering, overflow, and function errors |
| `koko-ir` | Shared bound-expression, row-layout, query-graph, and logical/physical plan contracts |
| `koko-binder` | Name/scope/type resolution, parameters, patterns, clauses, defaults, macros, UDFs, and binder diagnostics |
| `koko-expr` | Compilation and columnar evaluation of bound expressions |
| `koko-planner` | Plan building, cost choices, pushdown, pruning, joins, factorization, decorrelation, and activation barriers |
| `koko-loader` | CSV/Parquet/NPY decoding, file resolution, output adapters, and external scan validation |
| `koko-processor` | Pull operators, chunks, joins/aggregates, graph-algorithm storage adapters/caching, parallel morsels, cancellation, memory control, and execution errors |
| `koko` unit plus `public_api`/`tooling` integrations | Database/graph/connection ownership, transactions/concurrency, prepared statements, algorithm result integration, Arrow/interchange, UDFs, and public tooling |
| `koko-test-runner` unit plus `product` integration | Fixture parsing/directives/comparison, dataset orchestration, fresh-case isolation, manifest integrity, and end-to-end Cypher contracts |
| `koko-cli` unit plus `bootstrap`, `session`, `batch`, `process`, `presentation`, and `interactive` integrations | Configuration, editor/history/rendering, facade-only sessions, real processes/files, machine formats, cancellation, and PTY behavior |

## Cypher product fixtures

`crates/koko-test-runner/tests/product/manifest.tsv` maps every fixture to one behavioral category,
its dataset, case count, and unique contract. The `product` integration test fails if a fixture and
manifest entry differ, a dataset or category is unknown, a case count changes silently, a case is
skipped, or any case fails.

All fixture data is deterministic and checked in under
`crates/koko-test-runner/tests/datasets/`. For statements that read a checked-in file directly,
the integration runner resolves `${KOKO_ROOT_DIRECTORY}` to the workspace root for checked-in
fixture paths, independent of the directory from which Cargo was invoked. Each case runs against a
fresh in-memory database. Run the surface directly with:

```bash
cargo test -p koko-test-runner --test product

# Verbose per-case output, using the same bundled data:
KOKO_DATASET_DIR="$PWD/crates/koko-test-runner/tests/datasets" \
  cargo run -p koko-test-runner -- crates/koko-test-runner/tests/product
```

Manifest categories are intentionally product-oriented:

- `language`: clauses, expressions, scoping, patterns, and result semantics;
- `types-functions`: value types, casts, scalar/aggregate functions, and rendering;
- `schema-writes`: DDL, DML, catalog objects, defaults, and multiplicity constraints;
- `transactions`: explicit transaction state and visibility;
- `execution`: optimizer and processor correctness, including parallel/serial equivalence;
- `loading`: bundled data ingestion and typed loaded-data queries;
- `decisions`: focused intentional Koko behavior decisions;
- `regressions`: narrow reproductions of previously observed Koko defects.

When adding or changing a fixture:

1. Start from an observable Koko contract or a reproduced Koko bug—not an unexplained reference
   output.
2. Prefer `-DATASET CSV empty` plus explicit `CREATE`; add small checked-in data only when loading or
   a representative typed graph is itself part of the contract.
3. Assert exact row counts, values, errors, and order only where the contract requires order.
4. Add or update the manifest row in the same change. Do not add `-SKIP` to the product directory.
5. Use a crate unit test instead when the contract is a private invariant, a Rust integration test
   for a public API boundary, and a CLI integration test for terminal/process behavior.

## Optional compatibility, robustness, and performance evidence

These tools are not universal landing gates:

| Tool | Use when | Prerequisite |
|---|---|---|
| `docs/fable-audit/diffprobe.py <probe> [--dataset tinysnb]` | One change intentionally preserves or changes inherited Ladybug semantics | Built Ladybug reference shell |
| `docs/fable-audit/product_to_probe.py` | Re-diff every eligible hermetic product case and validate named active divergences | Built Koko and Ladybug shells |
| `cargo run --release -p koko-test-runner -- <Ladybug test_files path>` | Broad external compatibility investigation | Ladybug corpus and datasets |
| `scripts/goal_sweep.sh` / `scripts/goal_gate.py` | Reproduce the historical M1–M5 compatibility-close protocol | Ladybug checkout; long-running external sweep |
| `scripts/arity_sweep.sh` | Historical scalar-call panic robustness audit | Release example binary |
| `scripts/perf_gate.py` | A change owns the paired LSQB comparative-performance contract | Ladybug LSQB data and both release runners |

`KOKO_NO_OPTIMIZE=1` and `KOKO_THREADS=1` are diagnostic A/B modes. Use them when a change owns
optimizer or parallel-execution equivalence; the fixed product suite already contains focused
optimizer-barrier, plan-shape, and serial/parallel correctness coverage.
