# Koko Execution Layer — Spec for Idiomatic Rust Redesign (P0: single-threaded, flat-only)

All C++ paths below are under `/Users/dai/code/koko/`. `DEFAULT_VECTOR_CAPACITY` is set from CMake option `KOKO_VECTOR_CAPACITY_LOG2` (default **11**, capped at 12), so `DEFAULT_VECTOR_CAPACITY = 1 << 11 = 2048` (`cmake/templates/system_config.h.in:21-25`, `CMakeLists.txt:143`). `using sel_t = uint64_t` (`src/include/common/types/types.h:30`).

---

## 1. ValueVector

Source: `src/include/common/vector/value_vector.h`, `src/common/vector/value_vector.cpp`.

A `ValueVector` holds up to `DEFAULT_VECTOR_CAPACITY` (2048) values of one `LogicalType`. Fields:

- `LogicalType dataType` (public).
- `std::shared_ptr<DataChunkState> state` (public) — the **shared** selection/size state (see §2). Multiple vectors in the same `DataChunk` share one `state`. `firstNonNull`, `forEachNonNull`, `countNonNull`, serialize all iterate via `state->getSelVector()`.
- `std::unique_ptr<uint8_t[]> valueBuffer` — the flat, fixed-width data buffer, always allocated `numBytesPerValue * DEFAULT_VECTOR_CAPACITY` (`initializeValueBuffer`, cpp:391). Indexed by *physical position* `pos` (not by selection index): `getValue<T>(pos) == ((T*)valueBuffer)[pos]`.
- `NullMask nullMask` — bit-per-value null bitmap, sized to capacity (`nullMask{DEFAULT_VECTOR_CAPACITY}`). 64-bit entries; `hasNoNullsGuarantee()` is a fast path meaning "no nulls anywhere" (`src/include/common/null_mask.h`).
- `uint32_t numBytesPerValue` — the *in-vector* element width.
- `std::unique_ptr<AuxiliaryBuffer> auxiliaryBuffer` — type-specific overflow/child storage (see below).

**numBytesPerValue / element layout** (`getDataTypeSize`, cpp:372): STRING/JSON store a 16-byte `string_t` inline; STRUCT stores `struct_entry_t` (an i64 position into child vectors, pre-filled with `iota`); LIST/ARRAY store a `list_entry_t {offset, size}`; everything else uses `PhysicalTypeUtils::getFixedTypeSize` (the native sizeof). So the value buffer is always **fixed-stride**; variable-length payload lives in the auxiliary buffer.

**Auxiliary buffer** (`src/include/common/vector/auxiliary_buffer.h`, used via `StringVector`/`ListVector`/`StructVector`):
- STRING/JSON → `StringAuxiliaryBuffer` with an `InMemOverflowBuffer`. Short strings (≤12 bytes, `string_t::isShortString`) are inlined in the `string_t`; long strings set `overflowPtr` into the overflow arena (`StringVector::addString`, cpp:485). The overflow buffer is reset/reused per chunk (`resetAuxiliaryBuffer`).
- LIST/ARRAY → `ListAuxiliaryBuffer` holding a child *data* `ValueVector` plus `(size, capacity)`. `list_entry_t{offset,size}` indexes into that child vector. `addList` bump-allocates in the child.
- STRUCT/NODE/REL/UNION → `StructAuxiliaryBuffer` holding one child `ValueVector` per field; `setState` propagates the parent state to all children (cpp:36-44). MAP is a LIST-of-STRUCT(key,val); UNION is a STRUCT with a tag field.

**Flat vs unflat / "single value":**
- The vector itself does not carry a flat flag — that lives in `DataChunkState` (`fStateType`). "Flat" historically meant the chunk is restricted to exactly one logical row at `state->currIdx` (the comment in `data_chunk.h` still references `currIdx`), but the current code expresses single-row selection through the **selection vector**: `setAsSingleNullEntry()` sets `selSize=1` (value_vector.h:85). A "single value" vector uses `DataChunkState::getSingleValueDataChunkState()` (capacity-1 state), e.g. for constant filters (`filter.cpp:20`) and the mark vector in ResultCollector (`result_collector.cpp:34`).
- Iteration is always `for i in 0..selSize { pos = selVector[i]; ... use getValue(pos) }`. **Selection index ≠ physical position**; the value buffer is addressed by `pos`.

---

## 2. DataChunk + DataChunkState

Sources: `src/include/common/data_chunk/data_chunk.h`, `data_chunk_state.h`, `sel_vector.h`, `src/common/data_chunk/sel_vector.cpp`.

**DataChunk** = `{ vector<shared_ptr<ValueVector>> valueVectors; shared_ptr<DataChunkState> state; }`. Move-only. All its vectors are constructed sharing the chunk's single `state`. So per-chunk, **size and selection are owned once** and aliased by every column — filtering one column re-selects all of them in lockstep.

**DataChunkState** (`data_chunk_state.h`):
- `shared_ptr<SelectionVector> selVector` — the shared selection + logical size.
- `FStateType fStateType ∈ {FLAT, UNFLAT}` — flattening flag; the code's own TODO says to merge this into `SelectionVector`.
- `getSingleValueDataChunkState()` — a singleton-style capacity-1 state for scalar vectors.
- Size lives in the selVector (`getSelSize`, `setSelSize`, `initOriginalAndSelectedSize`).

**SelectionVector / SelectionView** (`sel_vector.h`) — this is the key data-layer subtlety:
- `SelectionView` is a read-only view: `{ const sel_t* selectedPositions; sel_t selectedSize; State {DYNAMIC, STATIC} }`.
- **STATIC** mode: `selectedPositions` points into a shared global constant array `INCREMENTAL_SELECTED_POS = [0,1,2,…,2047]` (`sel_vector.cpp:14`). `isUnfiltered()` ⇔ STATIC and `positions[0]==0` ⇒ identity selection `0..selSize` (zero-cost, no per-chunk allocation).
- **DYNAMIC** mode: `selectedPositions` points at the owned `selectedPositionsBuffer` (a `unique_ptr<sel_t[]>` of `capacity`), which holds an explicit list of selected positions after a filter. `setToFiltered()`/`makeDynamic()` switch into this mode; `getMutableBuffer()` exposes it for writing.
- `slice(start, size)` produces a sub-`SelectionView` without copying.

---

## 3. PhysicalTypeID set and logical→physical mapping

`PhysicalTypeID` (`types.h:233`), with fixed sizes from `getFixedTypeSize` (`types.cpp:288`):

| PhysicalTypeID | Storage in value buffer | Bytes |
|---|---|---|
| ANY | (unresolved; error to materialize) | — |
| BOOL | `bool` | 1 |
| INT8 / INT16 / INT32 / INT64 | native ints | 1/2/4/8 |
| UINT8 / UINT16 / UINT32 / UINT64 | native uints | 1/2/4/8 |
| INT128 / UINT128 | 128-bit | 16 |
| FLOAT / DOUBLE | f32 / f64 | 4 / 8 |
| INTERVAL | `interval_t` (months,days,micros) | 16 |
| INTERNAL_ID | `internalID_t {offset:u64, tableID:u64}` | 16 |
| ALP_EXCEPTION_FLOAT / _DOUBLE | ALP codec exception records (storage-internal) | codec-defined |
| STRING / JSON | inline `string_t` (16B), payload in overflow buffer | 16 |
| LIST / ARRAY | `list_entry_t {offset:u64, size:u64}`; elems in child data vector | 16 |
| STRUCT | `struct_entry_t {pos:i64}`; fields in child vectors | 8 |
| POINTER | raw `uint64_t` pointer | 8 |

Logical→physical mapping (`LogicalType::getPhysicalType`, `types.cpp:818`). LogicalTypeID enum at `types.h:185`:

- `BOOL → BOOL`
- `INT64, TIMESTAMP, TIMESTAMP_SEC/MS/NS/TZ, SERIAL → INT64`
- `INT32, DATE → INT32`; `INT16 → INT16`; `INT8 → INT8`
- `UINT64 → UINT64`, `UINT32 → UINT32`, `UINT16 → UINT16`, `UINT8 → UINT8`
- `INT128 → INT128`; `UUID → INT128`; `UINT128 → UINT128`
- `DOUBLE → DOUBLE`; `FLOAT → FLOAT`
- `DECIMAL → INT16/INT32/INT64/INT128` chosen by precision (≤4 / ≤9 / ≤18 / ≤38)
- `INTERVAL → INTERVAL`
- `INTERNAL_ID → INTERNAL_ID`
- `STRING, BLOB → STRING`; `JSON → JSON`
- `LIST, MAP → LIST`; `ARRAY → ARRAY`
- `STRUCT, NODE, REL, RECURSIVE_REL, UNION → STRUCT`
- `POINTER → POINTER`; `ANY → ANY`

**P0 relevance:** flat-only single-thread P0 should implement the fixed-width set + STRING (with overflow). LIST/ARRAY/STRUCT/MAP/UNION/JSON can be deferred; their layout (entry-in-buffer + child/overflow) is documented above for forward-compat.

---

## 4. Operator pull model

Sources: `src/include/processor/operator/physical_operator.h`, `physical_operator.cpp`, `sink.h`, `result_set.h`, `data_pos.h`, plus `filter.cpp`, `projection.cpp`, `result_collector.cpp`, `scan_node_table.h`.

**Lifecycle (driver-controlled, top-down init then bottom-up pull):**
1. `initGlobalState(ctx)` — recurses to children first (unless `isSource()`), once per query (`physical_operator.cpp:184`).
2. `initLocalState(ResultSet*, ctx)` — recurses to children first, stores `resultSet`, registers metrics, calls `initLocalStateInternal` (cpp:191). The **same `ResultSet` pointer is threaded through the whole pipeline**; operators wire their I/O `ValueVector`s by `DataPos {dataChunkPos, valueVectorPos}` into it (`data_pos.h`).
3. Pull: a `Sink` drives `while (child->getNextTuple(ctx)) { ... }`.

**`getNextTuple(ctx)` (public, non-virtual, cpp:200):** checks interruption/timeout, starts timer, calls the virtual `getNextTuplesInternal(ctx)`, updates progress, stops timer. Returns `bool` — **true = a new chunk of tuples is ready in the shared ResultSet; false = exhausted.** It does *not* return data; data flows by mutation of the shared `ResultSet`/`DataChunk`/`ValueVector`s.

**`virtual bool getNextTuplesInternal(ExecutionContext*) = 0`** is the per-operator pull body. Pattern is a *volcano/pull* model returning one DataChunk-batch per call:
- **Scan (source)** `ScanNodeTable` (`scan_node_table.h:94 isSource`): no child; pulls the next morsel from the table into its output vectors; returns false when the table is drained.
- **Filter** (`filter.cpp:26`): loop — `restoreSelVector`, pull child; if child false return false; `saveSelVector`; `expressionEvaluator->select(selVector, unflat)` rewrites the shared `DataChunkState`'s selection in place; repeat until ≥1 row survives. Output = same vectors, narrowed selection. `NodeLabelFilter` does the same by hand over the sel buffer. The save/restore of the prior selection lives in `SelVectorOverWriter` (`filtering_operator.h`).
- **Projection** (`projection.cpp:27`): pull child; run each `ExpressionEvaluator->evaluate()`, writing results into pre-wired output vectors (`initLocalStateInternal` plants `evaluator.resultVector` into the ResultSet at `exprsOutputPos`). Tracks `multiplicity` for discarded chunks.
- **HashJoinProbe** (`hash_join_probe.h`): binary; build side (`children[1]`) is consumed by a `HashJoinBuild` sink into a shared hash table; probe side (`children[0]`) is pulled and matched. `flatProbe` distinguishes single-key vs batched-key probing.

**ResultSet** (`result_set.h`): `{ uint64_t multiplicity; vector<shared_ptr<DataChunk>> dataChunks; }`. `getValueVector(DataPos)` and `getDataChunk(pos)` are the addressing API. `multiplicity` carries factorization cardinality for chunks projected away.

**Sink / ResultCollector materialization** (`sink.h`, `result_collector.cpp`):
- `Sink::getNextTuplesInternal` is `final` and throws — sinks expose `executeInternal(ctx)` instead, and `isSink()==true`.
- `ResultCollector::initLocalStateInternal`: resolves `payloadVectors` from `info.payloadPositions` via `resultSet->getValueVector`, builds a local `FactorizedTable` from `info.tableSchema`.
- `executeInternal`: `while (child->getNextTuple(ctx)) { for i in 0..multiplicity { localTable->append(payloadAndMarkVectors); } }`, then `sharedState->mergeLocalTable(localTable)` under a mutex (the only place threads converge; trivial in single-thread P0).
- `FactorizedTable` (`factorized_table.h`) stores tuples row-major in `DataBlock`s; **flat** columns are stored inline per-tuple, **unflat** columns are stored as a pointer to an overflow list (factorization). `append(vector<ValueVector*>)` reads each vector through its selection vector and copies via `copyToRowData` (which handles string/list/struct overflow into the table's own `InMemOverflowBuffer`). In flat-only P0 every column is a flat column → pure fixed-stride row append.

---

## 5. Rust-idiomatic redesign recommendation (single-threaded, flat-only P0)

Goal: keep the columnar + selection-vector performance model, but replace C++'s `shared_ptr` aliasing and raw `uint8_t*` casts with Rust ownership. For single-threaded P0, avoid `Arc`/locks entirely.

**Constants.** `const VECTOR_CAPACITY: usize = 2048;` (`= 1 << 11`). Bake it as a `const` and size every buffer/null-mask/sel-buffer to it. Use it as the morsel size for scans and the loop stride in factorized-table appends.

**Physical type & storage.** Model the value buffer as a typed enum rather than `uint8_t[]` + casts — this kills the entire `getValue<T>` reinterpret-cast family at compile time:

```rust
enum ColumnData {
    Bool(Box<[bool]>),
    Int8(Box<[i8]>), Int16(..), Int32(..), Int64(Box<[i64]>),
    UInt8(..), .. UInt64(..),
    Int128(Box<[i128]>), UInt128(Box<[u128]>),
    Float(Box<[f32]>), Double(Box<[f64]>),
    Interval(Box<[Interval]>),
    InternalId(Box<[InternalId]>),   // {offset:u64, table_id:u64}
    String(StringColumn),            // inline StringSlot + overflow Bump arena
    // P1+: List(ListColumn), Struct(StructColumn{children: Vec<ValueVector>}), ...
}
```
Each variant is `Box<[T]>` of length `VECTOR_CAPACITY`. `PhysicalType` becomes a `#[repr(u8)]` enum; keep a `logical_to_physical(LogicalTypeId) -> PhysicalType` free fn mirroring §3. Strings: a `StringSlot { len: u32, prefix:[u8;4], data: SmallRepr }` where short strings inline and long strings index a per-vector `bumpalo` (or simple `Vec<u8>` arena) reset per chunk — this is the safe analogue of `InMemOverflowBuffer`, no `overflowPtr` raw pointers.

**Null mask.** A small newtype over `Box<[u64]>` of `VECTOR_CAPACITY/64` words, plus a cached `has_no_nulls: bool` fast-path flag (mirrors `hasNoNullsGuarantee`). All bit ops are safe `&`/`|`/shifts.

**Selection vector.** Encode the STATIC/DYNAMIC distinction as a Rust enum instead of a pointer that may alias a global:

```rust
enum Selection {
    /// identity 0..len  (the INCREMENTAL_SELECTED_POS fast path — zero allocation)
    Flat { len: usize },
    /// explicit selected positions after a filter
    Filtered(Vec<usize>),   // or Box<[u32]>; len() is the selected size
}
impl Selection { fn iter(&self) -> impl Iterator<Item=usize>; fn len(&self) -> usize; }
```
This makes `isUnfiltered`/`setToFiltered`/`makeDynamic` total and checked. No shared global constant array needed.

**Sharing model — the one real design decision.** C++ shares one `DataChunkState` across all columns via `shared_ptr`, and operators mutate that shared selection. The idiomatic single-threaded Rust replacement is to **invert ownership: the `DataChunk` owns the selection + size once, and the columns do NOT each hold a back-pointer to it.**

```rust
struct ValueVector {
    logical_type: LogicalType,
    data: ColumnData,
    nulls: NullMask,
}                       // no state field, no shared_ptr
struct DataChunk {
    columns: Vec<ValueVector>,   // owned, length VECTOR_CAPACITY each
    sel: Selection,              // the single shared selection
    // (flat flag folded into Selection::Flat{len==1} or a separate `flat: bool` for P0)
}
```
Operators take `&mut DataChunk`, read columns with `&chunk.columns[i]` and the selection with `&chunk.sel` simultaneously — the borrow checker allows this because `sel` and `columns` are disjoint fields (split borrow). A `Filter` mutates `chunk.sel` and leaves `columns` untouched; a `Projection` writes into a column while reading the shared `sel`. This removes all `shared_ptr` aliasing while preserving the "filter once, all columns follow" semantics. (When you genuinely need the C++ cross-DataChunk sharing — e.g. STRUCT children sharing the parent's state — model children as nested `ValueVector`s inside the struct column, which inherit the chunk's `sel` by being iterated with the same selection; no separate state object.)

`ResultSet` becomes `struct ResultSet { multiplicity: u64, chunks: Vec<DataChunk> }`, addressed by `DataPos { chunk: usize, vector: usize }` (a plain `Copy` struct).

**Operator trait — pull model.** Keep volcano/pull but make the data-flow explicit and the lifecycle type-safe. Two viable shapes:

```rust
trait Operator {
    fn op_type(&self) -> PhysicalOperatorType;
    fn init(&mut self, rs: &mut ResultSet, ctx: &ExecutionContext);
    /// returns true if a new batch is ready in the shared ResultSet, false when exhausted
    fn next(&mut self, rs: &mut ResultSet, ctx: &ExecutionContext) -> Result<bool>;
}
```
This mirrors C++ exactly (shared mutable `ResultSet`, `bool` return). It requires children to be reachable for `next()` — store children as `Vec<Box<dyn Operator>>` and have each operator drive `self.children[0].next(rs, ctx)`. Because P0 is single-threaded, `&mut ResultSet` threaded down the tree is fine; the split-borrow trick keeps Filter/Projection happy.

A more idiomatic alternative worth considering for P0 is an **iterator-style pull that returns the batch by reference** instead of mutating shared state:

```rust
trait Operator {
    /// None = exhausted; Some(chunk) borrows this operator's owned output DataChunk
    fn next(&mut self, ctx: &ExecutionContext) -> Result<Option<&mut DataChunk>>;
}
```
Here each operator owns its output `DataChunk`; a `Filter` borrows its child's chunk, narrows a selection it owns, and returns a view; the sink consumes `Some(chunk)` until `None`. This eliminates the global `ResultSet` + `DataPos` indirection entirely and reads like a fused iterator. Trade-off: harder to express operators that need to look at *multiple* upstream chunks at once (cross-product, multi-key hash join) — for those, fall back to the explicit-`ResultSet` form. **Recommendation: use the `&mut ResultSet` trait (first form) so the binder's `DataPos` wiring ports 1:1, and confine the iterator form to an internal convenience.**

**Sink / FactorizedTable.** `Sink` is a separate trait (`fn execute(&mut self, rs, ctx)`), not part of `Operator::next`, matching C++'s split. The `FactorizedTable` for flat-only P0 is just a row-major `Vec<u8>` (or `Vec<Row>`) with a fixed tuple stride computed from the flat column physical sizes, plus a per-table string arena; `append(&[&ValueVector], sel)` iterates `sel` and copies fixed-width values + interned strings. No factorization/unflat columns in P0.

**Where `unsafe` is genuinely needed:** essentially nowhere in P0 if columns are typed `Box<[T]>` enums. The only candidates are (a) bypassing bounds checks in the hot copy loop via `get_unchecked` once correctness is proven (optional perf), and (b) SIMD/bytemuck-style bulk casts for fixed-width copies into the factorized table — both isolated, both replaceable by safe `copy_from_slice`. Avoid the C++ pattern of one untyped `uint8_t[]` + `reinterpret_cast`; that is the main thing the Rust redesign should *not* transliterate.