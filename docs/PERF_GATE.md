# Performance gates and recorded baselines

> **Status (2026-07-26): completed historical LSQB comparison protocol and measurements.**
> The 2026-07-25 idiomatic Rust cutover run produced nine correct, timeout-free answers, every
> median Rust/Ladybug ratio below 2×, and the q4/q5/q7 Rust wins recorded below. The harness still
> enforces that protocol when intentionally invoked, but comparative ratios are no longer universal
> post-v0 landing gates.
>
> The comparison is in-memory-only and does not reactivate native durability.
> [`ROADMAP.md`](../ROADMAP.md) owns current measurement-triggered opportunities;
> [`PERF_10B_PLAN.md`](PERF_10B_PLAN.md) is the historical root-cause record.

**Historical IM4 protocol:** release binaries were built once. Each engine/query pair received one
warm-up and three measured isolated-process runs with alternating engine order. Dataset load was
excluded. The harness validates answers and timeouts and rejects a wrong answer, timeout, ratio
above 2.00, or q4/q5/q7 ratio at least 1.00. A query within 10% of a boundary receives two extra
paired samples; the five-sample median is final.

## Historical comparison workload

- **Workload:** `…/test/test_files/lsqb/lsqb_queries.test` — nine deterministic `count(*)`
  queries over `lsqb-sf01` (heavy joins, a triangle, multi-label/multi-rel-type patterns,
  `OPTIONAL MATCH`, and `NOT EXISTS`). Each has an expected count, so the file validates
  correctness at the comparison scale as well as timing.
- **Data:** `lsqb-sf01` — approximately 52 MB of CSV. It fits in RAM for both engines. Process
  isolation reloads it for each query; load time is excluded, so the comparison makes no bulk-load
  performance claim.
- **Harness:** `scripts/perf_gate.py` runs each query in its own process on the Koko runner
  (`KOKO_TIMING=1`) and Ladybug reference shell (`koko :memory:`). It checks both answers, excludes
  dataset load, prints hardware/toolchain/binary metadata and raw/median timings, and applies the
  historical thresholds. The reference baseline is skipped cleanly if `KOKO_CPP_BIN` is absent;
  a paired comparison requires it.

Run it: `KOKO_ROOT_DIRECTORY=/path/to/koko python3 scripts/perf_gate.py`
(build the C++ baseline once with `GEN=Ninja make release` in the koko checkout →
`build/release/tools/shell/koko`).

## Idiomatic Rust cutover final run (2026-07-25, Apple M5, both in-memory)

| q | median Rust (ms) | median C++ (ms) | Rust/C++ |
|---|---:|---:|---:|
| q1 | 395 | 847.760 | 0.465934 |
| q2 | 20 | 55.080 | 0.363108 |
| q3 | 167 | 118.730 | 1.406553 |
| q4 | 20 | 3356.610 | 0.005958 |
| q5 | 70 | 4398.100 | 0.015916 |
| q6 | 149 | 136.820 | 1.089022 |
| q7 | 853 | 2621.840 | 0.325344 |
| q8 | 386 | 252.920 | 1.526174 |
| q9 | 524 | 653.690 | 0.801603 |

`scripts/perf_gate.py` returned `IM4 PERF GATE: PASS`. Every answer matched, every final measured
process completed within 90 seconds, every ratio remained below 2×, and q4/q5/q7 remained below 1×.
An immediately preceding run recorded one isolated q8 extra-sample timeout after three successful
samples. A five-sample q8 diagnostic then passed at 1.278239, followed by the complete passing run
above; the final gate did not require boundary resampling.

## First-party CLI final preserved-engine run (2026-07-23, Apple M5, both in-memory)

| q | median Rust/C++ |
|---|---:|
| q1 | 0.598122 |
| q2 | 0.967742 |
| q3 | 1.379251 |
| q4 | 0.013137 |
| q5 | 0.020362 |
| q6 | 1.295932 |
| q7 | 0.430368 |
| q8 | 1.951483 |
| q9 | 0.932479 |

`scripts/perf_gate.py` returned `IM4 PERF GATE: PASS` (the retained harness label). All answers
matched, every process completed within the timeout, all ratios are below 2×, and q4/q5/q7 remain
below 1×. Q8 was within 10% of the 2× boundary, so the required two extra paired samples ran and the
five-sample median passed.

## IM5 final close run (2026-07-22, Apple M5, both in-memory)

| q | median Rust/C++ |
|---|---:|
| q1 | 0.559096 |
| q2 | 0.450113 |
| q3 | 0.990719 |
| q4 | 0.004810 |
| q5 | 0.015467 |
| q6 | 0.843355 |
| q7 | 0.277126 |
| q8 | 1.217283 |
| q9 | 0.576137 |

`scripts/perf_gate.py` returned `IM4 PERF GATE: PASS` (the retained harness label). All answers
matched, every process completed within the timeout, all ratios are below 2×, and q4/q5/q7 remain
below 1×. No query triggered boundary resampling.

## IM5 provisional close run (2026-07-21, Apple M5, both in-memory)

| q | Rust samples (ms) | C++ samples (ms) | median Rust | median C++ | Rust/C++ |
|---|---|---|---:|---:|---:|
| q1 | 318, 337, 344 | 607.250, 681.010, 875.070 | 337 | 681.010 | 0.494853 |
| q2 | 13, 13, 13 | 34.540, 35.220, 40.730 | 13 | 35.220 | 0.369108 |
| q3 | 94, 95, 93 | 89.200, 108.420, 83.530 | 94 | 89.200 | 1.053812 |
| q4 | 11, 10, 10 | 2402.640, 2452.070, 2262.870 | 10 | 2402.640 | 0.004162 |
| q5 | 31, 32, 31 | 2965.510, 2752.190, 2883.300 | 31 | 2883.300 | 0.010752 |
| q6 | 70, 71, 73 | 105.910, 105.130, 121.820 | 71 | 105.910 | 0.670381 |
| q7 | 686, 676, 673 | 2431.050, 2249.060, 2391.030 | 676 | 2391.030 | 0.282723 |
| q8 | 296, 299, 295 | 226.210, 219.700, 258.270 | 296 | 226.210 | 1.308519 |
| q9 | 414, 419, 417 | 615.620, 640.770, 600.940 | 417 | 615.620 | 0.677366 |

`scripts/perf_gate.py` returned `IM4 PERF GATE: PASS` (the retained harness label). All answers
matched, every process completed within the 90-second timeout, all ratios are below 2×, and
q4/q5/q7 remain below 1×. No query triggered boundary resampling.

## IM4 close run (2026-07-20, Apple M5, both in-memory)

| q | Rust samples (ms) | C++ samples (ms) | median Rust | median C++ | Rust/C++ |
|---|---|---|---:|---:|---:|
| q1 | 383, 375, 349 | 1531.660, 811.390, 702.170 | 375 | 811.390 | 0.462170 |
| q2 | 15, 13, 13 | 38.840, 36.240, 49.570 | 13 | 38.840 | 0.334706 |
| q3 | 99, 132, 105 | 92.520, 107.160, 101.450 | 105 | 101.450 | 1.034993 |
| q4 | 11, 11, 39 | 2438.280, 9024.910, 9569.460 | 11 | 9024.910 | 0.001219 |
| q5 | 180, 105, 75 | 12181.430, 12232.650, 9066.050 | 105 | 12181.430 | 0.008620 |
| q6 | 305, 147, 204 | 175.500, 165.490, 187.390 | 204 | 175.500 | 1.162393 |
| q7 | 1394, 1110, 1503 | 7213.450, 7772.860, 8811.560 | 1394 | 7772.860 | 0.179342 |
| q8 | 290, 279, 285 | 189.380, 186.490, 183.080 | 285 | 186.490 | 1.528232 |
| q9 | 376, 529, 394 | 472.830, 401.530, 483.320 | 394 | 472.830 | 0.833280 |

The harness returned `IM4 PERF GATE: PASS`; all nine expected counts matched, every measured process
completed, every ratio is below 2×, and q4/q5/q7 preserve the required Rust wins. No query was close
enough to a boundary to trigger the two-extra-pair rule.

## First baseline (2026-06-28, 10-core Apple Silicon, both in-memory)

| q | shape | Rust exec | Rust ✓ | C++ exec | C++ ✓ | rust/cpp |
|---|---|---|---|---|---|---|
| q1 | 9-hop linear path | 6.19 s | OK | 541 ms | OK | 11× |
| q2 | 2-path join | 3.12 s | OK | 24 ms | OK | 130× |
| q3 | **triangle** | 18.22 s | OK | 63 ms | OK | **288×** |
| q4 | multi-label / multi-rel-type | **116 ms** | OK | 1.80 s | OK | **0.06×** |
| q5 | multi-label / multi-rel-type | **146 ms** | OK | 1.89 s | OK | **0.08×** |
| q6 | 55 M-row fan-out, factorized | 922 ms | OK | 84 ms | OK | 11× |
| q7 | `OPTIONAL MATCH` ×2 | 16.55 s | OK | 1.80 s | OK | 9× |
| q8 | `NOT EXISTS` subquery | 15.68 s | OK | 186 ms | OK | 84× |
| q9 | `NOT EXISTS` + 3-hop, 51 M | **TIMEOUT (>90 s)** | — | 415 ms | OK | ≫ |

(rust/cpp < 1 ⇒ Rust faster; numbers vary run-to-run — these are single-run.)

### Correctness at scale — the headline
**8 of 9 queries produce the exact expected answer on real lsqb-sf01 data** — a far
stronger correctness signal than the small `.test` corpus, exercising multi-label
patterns, multi-rel-type edges, undirected rels, `OPTIONAL MATCH`, `NOT EXISTS`, and
cycles, all at scale. **q9 is the one unconfirmed case** — it doesn't finish under 90 s,
so we don't yet know whether ours is *correct-but-slow* or *wrong-and-slow* (the C++
baseline confirms the expected 51 009 398). Confirming q9 is part of 10b.

### Where we already win
**q4 and q5 — we beat C++ by ~15×** (116 ms vs 1.80 s, 146 ms vs 1.89 s). These are the
multi-label / multi-rel-type fan-out patterns; our factorization (step 6) collapses the
unread fan-out into a multiplicity, which is a very good fit for these shapes. Worth
preserving as the optimizer evolves — and a sign the in-memory core is sound.

## Historical 10b interpretation of the first baseline

> The original per-query root-cause analysis compared plans, cardinalities, and a sample profile;
> [`PERF_10B_PLAN.md`](PERF_10B_PLAN.md) preserves the full analysis and landing sequence. It
> concluded that WCOJ was not the first lever: the reference used composite-key hash joins and join
> ordering for q2/q3, while q1/q6 spent time in row-value conversion, allocation, and adjacency
> lookup with idle cores. The four priorities below record that 2026-06-28 diagnosis.

This is not a current backlog. Several layers subsequently landed; `ROADMAP.md` owns any future
performance activation.

1. **A generalized hash-join family + the planner rules to emit it** — composite-key
   (close cycles/shared endpoints by hashing bound endpoint ids: q3 288×, q2 130×) **and**
   left/anti/mark semantics (unnest `OPTIONAL`→left join, `NOT EXISTS`→anti join, built once
   instead of a per-row nested loop: q8 84×, q7 9×, q9 timeout). The two clusters share one
   operator family — the single biggest, most consolidated win.
2. **Cost-based join order** (DP / selective-first + close-cycles-early) — shrinks the
   intermediates the joins process (q1/q2/q3/q6/q9; ours build 32M/6.24M where C++ stays
   ≤363K).
3. **Constant-factor execution rewrite** — vectorized columnar extend (kill the per-row
   boxed-`Value` round-trip, ~65% of q1) + **CSR adjacency** (no per-call `Vec<Neighbor>` /
   SipHash, dominant for the dense-graph q6). ~3–5×, multiplicative across every query;
   foundational, can run early.
4. **Parallelism over intermediates** (not just the driving scan) — q1/q6/q7/q8/q9 ran
   **serial** (small anchor scan / excluded shapes), 10 cores idle.

*(Demoted from the first guess: WCOJ — not what the reference uses at this scale.)*

Caveats: C++ `:memory:` vs Rust in-memory is the fairest engine-vs-engine comparison, but
C++'s normal lsqb mode is disk+buffer-pool; both engines used multiple cores; single-run
numbers. The *ratios* are large enough that run-to-run noise doesn't change the ranking.

## Historical follow-up observations

- `ldbc-sf01` was identified as a possible second workload, but its interactive short/complex-read
  format requires a harness extension.
- A load-once benchmark could separate steady-state query cost from the current process-isolated
  reload cost.

Neither item is active by default. `ROADMAP.md` owns activation conditions for current performance
work.


## Topological levels point-in-time diagnostic (2026-08-08)

This is implementation evidence, not a standing release threshold. A release `koko` CLI built with
`rustc 1.94.0` ran on the Apple M5 workstation. Each in-memory topology was loaded once, then the
same aggregate algorithm query ran three times as independent statements:

```cypher
CALL topological_levels(['N'], ['E'])
YIELD node, level
RETURN count(*), max(level)
```

CLI `QuerySummary::execution_time` excludes DDL and graph construction but includes the complete
algorithm scan, result-chunk production and aggregation. Every run returned the expected vertex
count and maximum level.

| Topology | Vertices | Edges | Execution samples (ms) | Median (ms) |
|---|---:|---:|---|---:|
| Two wide layers | 100,000 | 50,000 | 4.193, 4.048, 3.897 | 4.048 |
| Single long chain | 100,000 | 99,999 | 5.626, 5.710, 5.739 | 5.710 |
| Long chain with forward degree up to four | 100,000 | 399,990 | 8.233, 7.831, 7.936 | 7.936 |

For context, two ordinary typed node-scan aggregates over the same 100,000-node chain took 1.137
and 1.051 ms. The topological path performs one narrow endpoint pass, one forward-adjacency pass and
one output scan; it allocates no edge copy.