# P3 step 10b — historical root-cause analysis and optimization sequence

> **Status (2026-07-19): P3 INTERIM GATE COMPLETE; DOCUMENT HISTORICAL.** The L1–L4 sequence
> identified here shipped in `fbb0a38..c224602` (L3b dense adjacency was reverted, then re-landed
> after the per-pair rel-group refactor). `PERF_GATE.md` records the resulting ≤5× interim gate.
> The analysis and proposed layering below explain those decisions; they are **not a current task
> checklist**. Current performance opportunities and activation requirements live in
> `ROADMAP.md`. Native durability is permanently deferred and has no performance gate.

> This plan came from the 10a baseline by comparing Rust and C++ `EXPLAIN`/`PROFILE` output,
> intermediate cardinalities, and a sampled CPU profile. Two findings overturned the original
> baseline ranking.

## Two corrections to the baseline-ranked priority

1. **WCOJ is *not* the top lever — it isn't even what the reference uses.** The C++
   `EXPLAIN` for q3 (the triangle) and q2 contains **no `INTERSECT`/WCOJ**. C++ hits its
   q3=63ms / q2=24ms with **binary hash joins keyed on a *composite* endpoint-id pair**
   (e.g. close the triangle by hashing `(person1._ID, person3._ID)`) plus a cost-based
   join order that builds the *selective* path first. So the actionable lever for the
   cyclic/shared-endpoint queries is **join-order + a multi-key hash join on bound
   endpoint ids**, not WCOJ. (WCOJ stays a deeper / higher-scale option, not required to
   close the observed gap.)
2. **q1/q6 are *not* fully factorized.** On a linear path every intermediate node is read
   (it is the next extend's `from_id_col`), so multiplicity-collapse fires only on the
   **last** hop; the other N−1 hops fan out and materialize millions of rows. The 11× is
   therefore a *materialization + per-row execution* cost, not "pure constant factor over
   a collapsed plan." (The star shape of q4/q5 *is* fully collapsible — that's why we win
   there; see below.)

## The four cross-cutting root causes

Each query's slowdown decomposes into these; most queries hit several.

### RC-1 — Planner: join order + how cycles / shared endpoints are closed  *(q2, q3; taxes q1/q6/q9)*
`build_match` (`koko-planner/src/lib.rs:794-818`) always prefers **extend-to-a-new-node**
over **closing a both-bound edge**, and when it finally closes one it uses a *single-key*
`ExtendTarget::Existing` filter (`lib.rs:1155-1184` → `processor/src/lib.rs:1271-1279`)
applied *after* the full multi-way product is materialized. The cost-based hash join only
fires on an equi-`Filter` above a `CrossProduct` (`optimize.rs:344-419`, `as_join_key`
`:619-642`) — which a *connected* pattern never produces — so it can't recognize a shared-
`(p1,p2)` or cyclic `(p1,p3)` join. Evidence: q3 materializes **Σ_c n_c³ = 32.0M** rows
(C++ peak 363K, 88×); q2 materializes **6.24M** `(p1,p2,comment,post)` rows (C++ 143K,
44×). Greedy anchor + forced connected traversal (no DP) also inflates q1's early
intermediates (ours millions vs C++ 30–130K). **Algorithmic.**

### RC-2 — Correlated subqueries run as per-row nested loops  *(q7, q8, q9)*
`Exec::Optional` and `Exec::Subquery` rebuild a fresh sub-pipeline via `build_exec`
**per input row** (`processor/src/lib.rs:1626`, `:1658`) — a correlated nested loop with no
anti/mark/left-join operator. Sub-pipeline builds = base-match cardinality: **~0.8M (q7),
1.08M (q8), 51M+ (q9)**. C++ lowers OPTIONAL→LEFT hash join and NOT EXISTS→anti/mark hash
join, **built once**. Two amplifiers in `optimize.rs`: (a) **factorization defeat** — any
`Optional`/`Subquery` clears the collapse `chain_ok` (`:248-250`), so the base can't fold
to a multiplicity; (b) **no sinking** — the subquery is pinned *above* the whole base incl.
the fan-out tail (`:454-485`), so q9's anti-test runs on **51M** rows instead of the **2.39M**
triples C++ tests *before* the `hasInterest→tag` fan-out. (Decisive: q9's base *without*
NOT EXISTS is q6 = 0.93s; adding it both defeats the collapse and runs 51M subqueries →
timeout.) **Algorithmic.**

### RC-3 — Constant-factor execution: row-major `Value` round-trip + per-call allocation + HashMap adjacency  *(q1, q6; a tax on every query after RC-1/RC-2 shrink intermediates)*
Columns are typed-columnar (`vector.rs:146-153`), but the fan-out path throws that away per
row: `expand_extend_row` allocates `row = vec![Value::Null; width]` (`processor/src/lib.rs:1280`),
fills each cell columnar→owned `Value` (`get_value`→clone), buffers the `Vec<Value>`, then
writes each back columnar (`set_value`) and drops them. The `sample` profile of q1 (serial):
**~65% in the boxed-`Value` round-trip** (`drop`/`set_value`/`clone`/`get_value`), **~15%
allocator**, ~6% adjacency. Adjacency is `HashMap<InternalId, Vec<u64>>` + SipHash with a
**fresh `Vec<Neighbor>` allocated per `extend` call** (`storage/src/lib.rs:673-708`) — for the
dense `Person_knows_Person` graph this dominates q6 (~35% alloc + ~14% extend). C++ gathers
columnar→columnar (vectorized int64) over **CSR** adjacency. **Constant-factor, but ~3–5×,
and multiplicative with everything else.**

### RC-4 — Parallelism can't reach these shapes  *(q1, q6; any small-scan/big-middle query)*
The step-9 morsel model parallelizes the **driving scan**; `parallel_scan_source` requires
`≥ 4·VECTOR_CAPACITY` source rows (`processor/src/lib.rs:2074, 2218-2223`). But cost-anchoring
picks the *smallest* node table (TagClass = 71, Person = 1700 for q1/q6) ≪ 8192 → **serial,
10 cores idle**, while all the work is in the fan-out *above* that tiny scan. Morseling the
source fundamentally can't partition a huge intermediate sitting over a small scan.

### What to PRESERVE — the q4/q5 win
q4/q5 are **star** patterns (several branches from a shared `message` node); every branch's
introduced node is unread, so **all branches collapse** and we compute
`mult = ∏ degree(branch)` per center without materializing the branch cross-product —
beating C++'s materializing hash joins ~15×. Any execution rewrite (RC-3) must keep
`factorized_extend`/`count_extend_row` (`processor/src/lib.rs:1323-1380`) and the
stacked-extend `chain_ok` marking (`optimize.rs:226-237`): **count neighbors, fold into
`mult`** for collapsible branches rather than reverting to fan-out.

## Per-query attribution (which RCs drive each)

| q | gap | dominant RC(s) |
|---|---|---|
| q1 | 11× | RC-3 (65% value round-trip) + RC-4 (idle cores) + RC-1 (join order) |
| q2 | 130× | RC-1 (6.24M intermediate; needs multi-key hash join + order) |
| q3 | 288× | RC-1 (32M = Σ n_c³; multi-key cycle-close + order) |
| q6 | 11× | RC-3 (CSR adjacency / alloc, dense graph) + RC-4 |
| q7 | 9× | RC-2 (~0.8M per-row OPTIONAL → LEFT join) |
| q8 | 84× | RC-2 (1.08M per-row NOT EXISTS → anti join) |
| q9 | timeout | RC-2 (51M per-row + factorization defeat + sink below fan-out) + RC-1 (order) |
| q4/q5 | **we win** | preserve factorization star-collapse |

## Proposed layering at the 10a baseline (historical)

The unifying observation: **RC-1 and RC-2 both want one thing — a generalized hash-join
operator family + a planner smart enough to emit it.** A multi-key hash join keyed on bound
endpoint ids *is* how you close a cycle (q2/q3); the same operator with anti/mark/left
semantics *is* how you unnest a subquery (q7/q8/q9). So they consolidate.

- **Layer 1 — Generalized join operator + the planner rules to emit it.** Extend the
  existing `HashJoin` (step 7) to **composite keys** + **left / anti / mark** semantics
  (build once, probe set-based). Planner: recognize a both-bound closing edge → multi-key
  hash join on endpoint ids (q2/q3); unnest `OPTIONAL`→left join and `NOT EXISTS`→anti join
  (q7/q8/q9); place the unnested join *below* the trailing fan-out and re-enable
  factorization after it (q9). Biggest algorithmic wins (q3 288×, q2 130×, q8 84×, q9
  timeout), and the two clusters share machinery. Builds on step-7 HashJoin + step-8 cost.
- **Layer 2 — Cost-based join order** (DP, or at least "selective path first + close
  cycles early"). Shrinks the intermediates Layer 1 then joins (q1/q2/q3/q6/q9). Uses the
  step-8 stats/cost model.
- **Layer 3 — Constant-factor execution rewrite** (RC-3): vectorized columnar extend
  (gather typed columns, no per-row `Value` round-trip / no per-row `Vec`), **CSR
  adjacency** returning a borrowed slice/iterator (no per-call `Vec<Neighbor>`, no SipHash).
  ~3–5× on what remains, and a multiplicative tax-cut on *every* query — foundational, so it
  can run early / in parallel with Layer 2 (different part of the stack: execution vs
  planning). Must preserve the q4/q5 factorization path.
- **Layer 4 — Parallelism over intermediates** (RC-4): extend the step-9 morsel model to
  partition a large *intermediate* (e.g. the hash-join probe / a big extend frontier), not
  just the driving scan, so small-scan/big-middle shapes use the cores. Last, once the
  serial path is efficient.

**Sequencing rationale:** Layer 1 attacks the largest, most numerous gaps and unifies two
clusters into one operator family → first. Layer 3 is foundational and independent (execution
layer) → can precede or parallel Layer 2. Layer 2 shrinks what 1+3 process. Layer 4 uses the
cores once the serial path is lean. Each layer is independently measurable against the gate
(re-run `scripts/perf_gate.py` after each) — the whole point of standing it up first.

**Demoted:** WCOJ (not what the reference uses here), the persistent thread pool, parallel
sort/recursive-frontier — revisit only if a later baseline says so.
