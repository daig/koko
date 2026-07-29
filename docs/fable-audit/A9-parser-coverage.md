# A9 — Grammar / Parser Coverage Audit

**Surface:** which language productions the C++ ANTLR grammar accepts that the
Rust hand-written parser does not (and vice-versa).

**Sources**
- C++ grammar: `/Users/dai/code/koko/src/antlr4/Cypher.g4` (ANTLR4, Kùzu-derived).
- Rust parser: `/Users/dai/code/koko-rs/crates/koko-parser/src/{lexer.rs,parser.rs,ast.rs}`
  (hand-written lexer + recursive-descent + precedence-climbing).

**Method.** Read both grammars in full, enumerated productions, then probed each
suspected divergence through BOTH engines in isolation (one statement per
process) capturing the exact message. Runner + probe files in the scratchpad
(`run_probes.py`, `probes_*.txt`).

**Confirmed-vs-suspected classification (key distinction requested).**
The Rust engine tags errors by layer (`crates/koko-common/src/error.rs`):
- **`Parser exception: …`** → the **hand-written parser/lexer rejects it** = a real
  parser-coverage gap (**CONFIRMED**).
- **`Binder exception:` / `Not implemented exception:` / `Runtime`** → the statement
  **parsed successfully** and was rejected downstream = the parser *does* cover the
  syntax; the gap (if any) is semantic, not grammatical (**PARSES / semantic**).

For the C++ side, a `Binder`/`Runtime`/`Catalog` error (or success) on a probe with
no schema proves **the C++ grammar accepts the syntax** (it reached binding/exec);
a C++ `Parser exception` proves C++ *also* rejects it (parity).

Legend: ✅ = accepted by that parser · ✖ = rejected at parse layer · (bind) = parses,
rejected later.

---

## 1. Top-level statements (`oC_Statement`)

| Production | C++ | Rust | Confirmed outcome / notes |
|---|---|---|---|
| `oC_Query` (MATCH/RETURN/…) | ✅ | ✅ | Core surface. |
| `CREATE NODE TABLE` | ✅ | ✅ | incl. `IF NOT EXISTS`, CTAS `AS <query>`. |
| `CREATE REL TABLE [GROUP]` | ✅ | ✅ | incl. multi-pair, multiplicity, CTAS, `WITH(storage_direction=…)`. |
| `CREATE SEQUENCE` | ✅ | ✅ | full option set. |
| `CREATE TYPE … AS …` | ✅ | ✅ | |
| `CREATE MACRO … AS …` | ✅ | ✅ | incl. `name := default`. |
| `DROP TABLE / SEQUENCE / MACRO` | ✅ | ✅ | incl. `IF EXISTS`. |
| `ALTER TABLE` (add/drop/rename prop, rename table, add/drop FROM-TO) | ✅ | ✅ | |
| `COMMENT ON TABLE … IS …` | ✅ | ✅ | both `Binder: Table Person does not exist` (parity). |
| `BEGIN/COMMIT/ROLLBACK/CHECKPOINT` | ✅ | ✅ | incl. `READ ONLY`. |
| `COPY <t> FROM '<file>' [(opts)]` | ✅ | ✅ | single string path + option list only. |
| **`CREATE INDEX … FOR … ON …`** | ✅ | **✖** | C++ `Binder: Table Person does not exist` (parses); Rust `Parser: expected LParen but found Ident("INDEX")`. |
| **`CREATE … INDEX` (typed, e.g. FULLTEXT)** | ✅ | **✖** | C++ `Binder: Index type FULLTEXT does not exist`; Rust `Parser: … found Ident("FULLTEXT")`. |
| **`CREATE GRAPH <g> [ANY]`** | ✅ | **✖** | C++ **succeeds** (`Created graph successfully`); Rust `Parser: expected LParen but found Ident("GRAPH")`. |
| **`DROP GRAPH <g>`** | ✅ | **✖** | C++ `Binder: Graph g does not exist`; Rust `Parser: expected a MATCH, CREATE, or RETURN clause but found Ident("DROP")`. |
| **`USE <db>` / `USE GRAPH <g>`** | ✅ | **✖** | C++ `Runtime: No database named mydb` / `Binder: No graph named g`; Rust `Parser: … found Ident("USE")`. |
| **`EXPORT DATABASE '<path>'`** | ✅ | **✖** | C++ **succeeds** (`Exported database successfully`); Rust `Parser: … found Ident("EXPORT")`. |
| **`IMPORT DATABASE '<path>'`** | ✅ | **✖** | C++ **succeeds**; Rust `Parser: … found Ident("IMPORT")`. |
| **`ATTACH '<p>' AS <d> (DBTYPE …)`** | ✅ | **✖** | C++ `Runtime: No loaded extension can handle database type`; Rust `Parser: … found Ident("ATTACH")`. |
| **`DETACH <db>`** (standalone) | ✅ | **✖** | C++ `Runtime: Database: d doesn't exist`; Rust `Parser: … found Ident("DETACH")`. (Rust only has `DETACH DELETE`.) |
| **`LOAD [EXTENSION] <name>`** | ✅ | **✖** | C++ `Binder: Extension httpfs … not installed`; Rust `Parser: expected keyword FROM …` (Rust `LOAD` only starts `LOAD FROM`). |
| **`INSTALL / UNINSTALL / UPDATE <ext>`, `FORCE INSTALL`** | ✅ | **✖** | C++ all **succeed**; Rust `Parser: expected a MATCH, CREATE, or RETURN clause …`. |
| `CREATE USER … WITH PASSWORD` | (ext) | ✖ | **Both reject in core build.** C++ core: `Parser: Failed parse the statement. Do you forget to load the extension?` (grammar rule exists but is extension-gated); Rust: no rule. Net parity in shipped core. |
| `CREATE ROLE` | (ext) | ✖ | Same as USER — both reject in core. |

**COPY sub-forms (C++ `iC_ScanSource` / `iC_CopyTO` / `iC_CopyFromByColumn`):**

| Production | C++ | Rust | Confirmed |
|---|---|---|---|
| `COPY (<query>) TO '<file>'` | ✅ | **✖** | C++ succeeds (no rows); Rust `Parser: expected an identifier, found LParen`. |
| `COPY <t> FROM (<subquery>)` | ✅ | **✖** | C++ `Binder: Table … does not exist`; Rust `Parser: expected a quoted file path … found LParen`. |
| `COPY <t> FROM ['a.csv','b.csv']` (file list) | ✅ | **✖** | Rust `Parser: … found LBracket`. |
| `COPY <t> FROM glob('*.csv')` (function source) | ✅ | **✖** | Rust `Parser: … found Ident("glob")`. |
| `COPY <t> FROM ('a','b') BY COLUMN` | ✅ | **✖** | C++ `Binder: Table … does not exist`; Rust `Parser: … found LParen`. |

**CALL forms (`iC_StandaloneCall` / `iC_InQueryCall`):**

| Production | C++ | Rust | Confirmed |
|---|---|---|---|
| `CALL <key> = <value>` (config set) | ✅ | ✅ | |
| `CALL current_setting('k')` | ✅ | ✅ | |
| `CALL show_tables()/table_info()/show_sequences()/show_macros()` | ✅ | ✅ | Rust hard-codes this fixed set. |
| **`CALL <any-other-fn>()`** (generic table func, e.g. `db_version()`) | ✅ | **parses, not-impl** | C++ returns `0.17.0`; Rust `Not implemented exception: CALL db_version(...) table functions are not supported in this phase` — **parser accepts the form**, only the function set is limited. |
| **`CALL f() YIELD <items> [RETURN …]`** | ✅ | **✖** | C++ `Binder: Output variables must all appear in the yield clause` (YIELD parsed); Rust `Parser: expected Eof but found Ident("YIELD")`. **No `YIELD` in the Rust grammar at all.** |

---

## 2. Query clauses & modifiers

| Production | C++ | Rust | Notes |
|---|---|---|---|
| `MATCH` / `OPTIONAL MATCH` | ✅ | ✅ | |
| `WHERE` (in MATCH / WITH) | ✅ | ✅ | |
| `MATCH … HINT <join-tree>` | ✅ | ✅ | Rust parses & discards (planner-only). |
| `UNWIND … AS …` | ✅ | ✅ | |
| `WITH [DISTINCT] … [ORDER/SKIP/LIMIT] [WHERE]` | ✅ | ✅ | multi-part splitting works. |
| `RETURN [DISTINCT] … [ORDER/SKIP/LIMIT]` | ✅ | ✅ | incl. `RETURN *`, `RETURN a.*`. |
| `CREATE / MERGE / SET / DELETE / DETACH DELETE` | ✅ | ✅ | MERGE incl. `ON CREATE/ON MATCH SET`. |
| `SET a = {…}` / `a += {…}` (whole-value / merge) | ✅ | ✅ | |
| `UNION` / `UNION ALL` | ✅ | ✅ | |
| `LOAD FROM '<file>' [(opts)] [WHERE]`, `LOAD WITH HEADERS (…)` | ✅ | ✅ | Rust supports the CSV-scan `LOAD FROM`. |
| **`EXPLAIN [LOGICAL] <query>`** | ✅ | **✖** | C++ prints the plan; Rust `Parser: expected a MATCH, CREATE, or RETURN clause but found Ident("EXPLAIN")`. |
| **`PROFILE <query>`** | ✅ | **✖** | C++ prints the profile; Rust `Parser: … found Ident("PROFILE")`. |
| **Multiple `;`-separated statements in one input** | ✅ | **✖** | C++ runs both; Rust `parse_statement` eats one optional `;` then requires EOF → `Parser: expected Eof but found Ident("RETURN")`. (Rust CLI contract is one-statement-per-line, so mostly a driver detail.) |

---

## 3. Expression operators & atoms

| Production | C++ | Rust | Confirmed outcome |
|---|---|---|---|
| `OR / XOR / AND / NOT` | ✅ | ✅ | `true XOR false`→True both. |
| comparison `= <> < <= > >=` | ✅ | ✅ | |
| chained comparison `a=b=c` | ✖ | ✖ | Both reject (C++ specific msg, Rust `expected Eof but found Eq`). Parity. |
| `!=` (invalid `<>`) | ✖ | ✖ | Both reject. Parity. |
| `+ - * / %` | ✅ | ✅ | `6 % 4`→2 both. |
| unary `-` / unary `+` | ✅ | ✅ | |
| `IS NULL` / `IS NOT NULL` | ✅ | ✅ | |
| list index `x[i]`, slice `x[a..b]` / `x[a:b]` | ✅ | ✅ | |
| map/struct literal `{k: v}` | ✅ | ✅ | `STRUCT_PACK`; identical render. |
| list literal `[…]` incl. NULL holes | ✅ | ✅ | |
| `CASE` (simple + searched) | ✅ | ✅ | |
| `EXISTS { MATCH … }` / `COUNT { MATCH … }` subquery | ✅ | ✅ | both `Binder` (parse OK). |
| named parameter `$name` | ✅ | ✅ | Rust: `Binder: Parameter param not found` (parses). |
| function call, `COUNT(*)`, `CAST(x AS T)`, named args `f(a := 1)`, lambda `x -> …` | ✅ | ✅ | |
| **`IN` (list membership)** | ✅ | **✖** | C++ `LIST_CONTAINS(…)`→True; Rust `Parser: expected Eof but found Ident("IN")`. (Rust `IN` exists **only** inside `[x IN …]`.) |
| **`STARTS WITH`** | ✅ | **✖** | C++→True; Rust `Parser: expected Eof but found Ident("STARTS")`. |
| **`ENDS WITH`** | ✅ | **✖** | C++→True; Rust `Parser: … found Ident("ENDS")`. |
| **`CONTAINS`** | ✅ | **✖** | C++→True; Rust `Parser: … found Ident("CONTAINS")`. |
| **`=~` (regex match)** | ✅ | **✖** | C++ `REGEXP_FULL_MATCH`→True; Rust `Parser: unexpected character '~' in query` (lexer). |
| **`^` (power)** | ✅ | **✖** | C++→1024; Rust `Parser: unexpected character '^' in query` (lexer). |
| **`\|` (bitwise OR)** | ✅ | **✖** | C++ `BITWISE_OR`→7; Rust `Parser: expected Eof but found Pipe`. |
| **`&` (bitwise AND)** | ✅ | **✖** | C++ `BITWISE_AND`→1; Rust `Parser: unexpected character '&' in query` (lexer). |
| **`<<` / `>>` (bit shift)** | ✅ | **✖** | C++ `BITSHIFT_LEFT`→16 / `BITSHIFT_RIGHT`→64; Rust `Parser: unexpected token Lt/Gt in expression`. |
| **`!` (factorial, postfix)** | ✅ | **✖** | C++ `FACTORIAL(5)`→120; Rust `Parser: unexpected character '!' in query` (lexer). |
| **quantifier `ANY/ALL/NONE/SINGLE (x IN list WHERE p)`** | ✅ | **✖** | C++→True (all four); Rust `Parser: expected RParen but found Ident("IN")` (parsed as a function call, chokes on `IN`). |
| **positional parameter `$1`** | ✅ | **✖** | C++→`$_0_`; Rust `Parser: expected an identifier, found Int(1)`. |
| **pattern predicate `(a)-[:R]->(b)` as expression** (`oC_PathPatterns`) | ✅ | **✖** | C++ `Binder: Table … does not exist` (parses); Rust `Parser: unexpected token Colon in expression`. |

---

## 4. Literals & lexical details

| Item | C++ | Rust | Confirmed |
|---|---|---|---|
| int, double, `1e5`, `.5`, `5.0` | ✅ | ✅ | identical. |
| `5.` (trailing dot) | ✖ | ✖ | Both reject (grammar needs digits after `.`). Parity. |
| hex `0x1F` / binary `0b101` / octal `0o17` | ✖ | ✖ | **Neither** supports; both `Parser` error. Parity. |
| digit separators `1_000` | ✖ | ✖ | Both reject. Parity. |
| `>i128` integer (UINT128/INT128) | ✅ | ✅ | both echo `10^38`. |
| string escapes `\n \t \' \" \\ \b \f \r` | ✅ | ✅ | |
| `\uXXXX` (4-hex) / `\UXXXXXXXX` (8-hex) | ✅ | ✅ | `\U0001F600`→😀 both. |
| `\xAA` (byte escape, kept as source text) | ✅ | ✅ | both render `\xAA`. |
| invalid escape `\q` | ✖ | ✖ | Both reject. Parity. |
| keyword case-insensitivity | ✅ | ✅ | Rust uses `eq_ignore_ascii_case`. |
| backtick identifier `` `weird name` `` | ✅ | ✅ | |
| empty backtick `` `` `` rejected | ✅ | ✅ | both reject with the same message. |
| unicode whitespace (NBSP, etc.) | ✅ | ✅ | Rust replicates the full class. |
| line `//` & block `/* */` comments | ✅ | ✅ | |
| nested block comments `/* /* */ */` | ✖ | ✖ | **Neither** nests; both reject. Parity. |
| **unescaped unicode identifier** (`AS naïve`, `ID_Start`/`ID_Continue`) | ✅ | **✖** | C++→`naïve`; Rust `Parser: unexpected character 'Ã' in query` (Rust ident = ASCII `[A-Za-z_][A-Za-z0-9_]*` only). |
| **doubled-backtick escape inside `` `…` ``** (`` `has``tick` ``) | ✅ | **✖** | C++→`has``tick`; Rust stops at first closing backtick → `Parser: expected Eof but found Ident("tick")`. |

---

## 5. Recursive / shortest-path syntax (`iC_RecursiveDetail`)

| Production | C++ | Rust | Confirmed |
|---|---|---|---|
| `*`, `*N`, `*lo..hi`, `*lo..`, `*..hi` bounds | ✅ | ✅ | |
| `SHORTEST`, `ALL SHORTEST` | ✅ | ✅ | `* SHORTEST` → both `Binder` (parse OK). |
| `TRAIL`, `ACYCLIC`, `WALK` semantics | ✅ | ✅ | |
| recursive lambda `(r, n \| WHERE … \| {…},{…})` | ✅ | ✅ | Rust unit-tested. |
| **`WSHORTEST(<prop>)` (weighted shortest)** | ✅ | **✖** | C++ `Binder` (parses); Rust `Parser: expected RBracket but found Ident("WSHORTEST")`. |
| **`ALL WSHORTEST(<prop>)`** | ✅ | **✖** | C++ `Binder`; Rust `Parser: expected keyword SHORTEST but found Ident("WSHORTEST")` (Rust's `ALL` demands `SHORTEST`). |

---

## 6. Reverse divergences (Rust accepts / C++ does **not**)

| Production | C++ | Rust | Confirmed |
|---|---|---|---|
| **List comprehension `[x IN list WHERE p \| proj]`** | **✖** | ✅ | The provided C++ grammar has **no list-comprehension rule**. C++ `RETURN [x IN [1,2,3] WHERE x>1 \| x*2]` → `Parser exception: … expected rule oC_RegularQuery` (chokes at `WHERE`). Rust → `[4,6]`. **Semantic trap:** the no-`WHERE` form `[x IN range(1,5) \| x]` *parses* in C++ but as `list-membership \| bitwise-or` inside a 1-element list literal (→ `Binder: Variable x is not in scope`), whereas Rust evaluates it as a comprehension (`[1,2,3,4,5]`). Same text, **different result** where both parse. |

No other Rust-accepts / C++-rejects productions were found; the Rust grammar is a
strict subset elsewhere.

---

## 7. Summary — biggest missing productions (all CONFIRMED parser-layer gaps)

Ordered roughly by how commonly they appear in real Cypher:

1. **String predicates `STARTS WITH` / `ENDS WITH` / `CONTAINS`** and **`IN` list
   membership** — core WHERE-clause operators, all hard parser errors.
2. **`^` power, `&`/`|` bitwise, `<<`/`>>` shifts, `!` factorial** — whole operator
   tiers (`oC_PowerOfExpression`, bitwise/shift levels) are absent from the Rust
   expression grammar; several fail in the *lexer* (`^ & ! ~` are not even tokens).
3. **Quantified predicates `ANY/ALL/NONE/SINGLE(x IN list WHERE p)`** — misparsed as
   function calls.
4. **`EXPLAIN` / `PROFILE`** query prefixes.
5. **DDL/admin statements**: `CREATE INDEX`, `CREATE/DROP GRAPH`, `USE`,
   `EXPORT/IMPORT DATABASE`, `ATTACH`/`DETACH <db>`, extension mgmt
   (`LOAD`/`INSTALL`/`UNINSTALL`/`UPDATE`).
6. **`CALL … YIELD`** (no `YIELD` production) and generic `CALL <fn>()` beyond a
   hard-coded four-function set (the latter parses but is `Not implemented`).
7. **COPY sub-forms**: `COPY … TO`, and `COPY … FROM` with subquery / file-list /
   `glob()` / `BY COLUMN` scan sources.
8. **`=~` regex**, **pattern-predicate expressions** `(a)-[:R]->(b)`, **positional
   params `$1`**, **`WSHORTEST(prop)`** weighted shortest path.
9. **Lexical**: unescaped **unicode identifiers** and **doubled-backtick** escapes.

**Reverse gap:** Rust adds **list comprehension** (`[x IN … WHERE … | …]`), which the
C++ grammar lacks — a *semantic divergence* (`[x IN l | e]` means different things
in each engine).

**Good parity (both reject identically):** hex/binary/octal & underscore numeric
literals, trailing-dot `5.`, nested block comments, chained comparison `a=b=c`,
`!=`, invalid string escapes, empty backtick identifier. These are not gaps —
Rust matches C++'s rejection.

All outcomes above were confirmed live via the C++ shell
(`build/release/tools/shell/koko`) and the Rust CLI
(`target/release/examples/koko_cli`); probe files and the runner are in the
scratchpad next to this report.
