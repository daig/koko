# Koko — Agent Guidelines

Koko is an independent, Rust-first Cypher property-graph database. It began as a clean-room
implementation of a C++ reference engine; that migration is complete, and the project may now
evolve its public API, language surface, and internals on their own merits. Native on-disk storage
remains outside the active scope.

The current product and work authority is **`ROADMAP.md`**. Native durable storage is permanently
deferred unless the project owner explicitly restores it. Migration plans and the differential
corpus are historical evidence, not sequencing authority. Extracted C++ semantics (test format,
value formatting, execution model, front-end shapes) remain in `docs/cpp-reference/`.

## Status & where truth lives (2026-07-29)

**Koko is an independent in-memory product with a first-party CLI and idiomatic Rust API.**
Supported behavior is a Koko non-regression contract. Ladybug 0.17 and the differential tools are
historical compatibility evidence: useful when a change intentionally touches inherited semantics,
but not automatic specifications or universal release gates.

The product provides graph/database/connection/query/storage ownership; typed and `ANY` graphs
through one execution pipeline; ordered JSON; graph-scoped HASH/ART DDL; atomic database-wide
logical interchange; validated query-time local read-only `icebug-disk`; and connection-local
native scalar UDFs. The `koko` crate is a small composition root over a private runtime capsule and
explicit adapters. Historical closure counts and performance measurements live in
`docs/PROGRESS.md` and `docs/PERF_GATE.md`, not in this current-status section.

**Scope rule:** never start native persistence or work justified only as a prerequisite for it
without a new explicit project-owner decision. `Database::new()` and
`Database::with_config(...)` create the supported in-memory product. Native database files,
persistent catalog/indexes, WAL/recovery/checkpoint, durable MVCC, page buffering, and physical
storage introspection are deferred and are not completion gates. Arrow C Data/C Stream, every
extension/plugin mechanism and extension module, projected graphs, connectors, and foreign
bindings remain owner-deferred. Do not scaffold a deferred surface without an explicit owner
decision.

- **`ROADMAP.md`** — current product and architecture map, active and planned work, known
  limitations, evidence-gated opportunities, intentional behavioral decisions, and deferred scope.
- **`docs/FACADE_ARCHITECTURE.md`** — implemented facade/module boundaries, state ownership,
  dependency direction, execution flow, and public API.
- **`docs/CLI_UX.md`** — authoritative user-visible CLI behavior and acceptance criteria.
- **`docs/CLI_ARCHITECTURE.md`** — implemented CLI component boundaries, ownership, and data flow.
- **`docs/TESTING.md`** — standing regression taxonomy, fixture ownership rules, commands, and
  optional compatibility/performance evidence.
- **`docs/PROGRESS.md`**, **`docs/PERF_GATE.md`**, **`docs/TRIAGE.tsv`**,
  **`fable-audit.md`**, `docs/fable-audit/`, the completed IM plans and goal prompts, and
  **`docs/CLI_PLAN.md`** are historical evidence; they do not own current work.

Optional compatibility tools:

- Full external corpus:
  `KOKO_DATASET_DIR=../ladybug/dataset cargo run --release -p koko-test-runner -- ../ladybug/test/test_files/<dir>`
- Differential probe:
  `python3 docs/fable-audit/diffprobe.py <probe-file> [--dataset tinysnb]`
- Product-fixture differential inventory: `python3 docs/fable-audit/product_to_probe.py`

Run these when the change owns a compatibility or comparative-performance contract, or when
explicitly requested—not by default for unrelated Koko work.

## Build & test

```bash
cargo build --workspace
cargo test  --workspace          # complete unit/integration/doctest/product regression gate
python3 scripts/gen_fn_catalog.py --check
cargo clippy --workspace --all-targets
cargo fmt --all --check

# Focused parser/facade/CLI gate:
python3 scripts/cli_goal_gate.py --strict

# Run every manifested Cypher fixture:
cargo test -p koko-test-runner --test product
```

Operational env knobs: `KOKO_DATASET_DIR` (corpus datasets), `KOKO_ROOT_DIRECTORY` (corpus `${…}`
path expansion), `KOKO_NO_OPTIMIZE=1` (fully naive plan — the A/B escape hatch), `KOKO_THREADS`
(`1` = serial; A/B for parallelism), `KOKO_TIMING=1` (per-statement timing in the runner).

Keep every commit `clippy`- and `fmt`-clean. End commit messages with the
`Co-Authored-By: Claude …` trailer.

## Architecture (per-layer crate DAG — compile-time-enforced edges)

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

## Conventions & invariants

- **No `unsafe`** in the engine yet; typed `ColumnData` replaces C++'s `uint8_t*` + casts. No active
  milestone authorizes a new general `unsafe` subsystem. If typed strings need an overflow arena,
  quarantine its `unsafe` implementation and test it exhaustively.
- **`VECTOR_CAPACITY = 2048`** (mirrors C++ `DEFAULT_VECTOR_CAPACITY`). Operators build chunks via
  `ChunkBuilder`, which flushes at that bound.
- **Result rendering is a Koko contract:** floats use six decimal places, NULL renders empty in the
  historical list format, `INTERNAL_ID` is `tableID:offset`, nodes are
  `{_ID: …, _LABEL: …, prop: …}` with null properties omitted, and relationships are
  `(src)-{…}->(dst)`. Change it only as an explicit product decision with regressions for every
  public renderer that owns the format.
- **`Error` display prefixes** (`Binder exception:`, `Runtime exception:`, and peers) are established
  Koko API/fixture behavior. Ladybug wording is no longer automatically authoritative.
- **`.test` comparison** sorts both sides unless `-CHECK_ORDER`; `-CHECK_PRECISION` uses a float
  tolerance.
- The **materialized result is columnar**; `Row<'_>` is a borrowed view over the column buffers.
- Historical milestone evidence is frozen in `docs/PROGRESS.md`. Current work updates
  `ROADMAP.md` and the owning architecture or behavior document.
- When a change intentionally preserves or changes inherited Ladybug behavior, consult the
  historical audit's oracle-defect record before deciding. Never match a reference crash or wrong
  value merely to erase a differential.

## Adding a Cypher product fixture

Drop a `.test` file in `crates/koko-test-runner/tests/product/`, prefer `-DATASET CSV empty` with
explicit `CREATE`, and assert with `---- N` / `---- ok` / `---- error`. Add the fixture's category,
dataset, case count, and unique observable contract to `manifest.tsv`. The `product` integration test
requires every manifested case to run without skips. Expectations must come from an explicit Koko
contract or a confirmed bug reproduction; use Ladybug differential tools only when compatibility is
part of that contract. See `docs/TESTING.md` for placement and non-duplication rules.
