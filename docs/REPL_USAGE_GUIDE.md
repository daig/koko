# Koko `koko` REPL usage guide

This is a practical reference for exploring Koko manually through the first-party Rust `koko`
terminal client. New users should begin with the end-to-end
[main tutorial](#3-main-tutorial-build-save-and-explore-rich-data), then use the remaining sections
as a command and behavior reference. It applies to the completed CLI at commit `4457021`. The
normative behavior contract is [`CLI_UX.md`](CLI_UX.md); this guide
emphasizes runnable workflows rather than acceptance criteria.

## 1. The essential mental model

- Every `koko` process owns one **in-memory database** and one serial, connection-owning query worker.
- Every new process starts with an empty typed graph named `main`.
- Nothing is loaded implicitly from the current directory. There is no `.kokorc`, positional database
  path, or native database file.
- Exiting destroys in-memory state. Use logical `EXPORT DATABASE` before exit when you need to restore
  the session later.
- Named graphs share one in-memory database but keep their schemas, data, indexes, and transactions
  graph-scoped.
- A graph is either:
  - **typed**: tables and property types are declared before data is created; or
  - **`ANY`**: labels, relationship types, and properties are inferred from created values.
- Cypher remains the only state-changing language. Meta commands inspect or configure the client;
  there are deliberately no `:use`, `:begin`, `:commit`, or `:rollback` aliases.
- Native persistence, extensions/plugins, connectors, projected graphs, remote object access, and
  foreign-language bindings are outside the active product boundary.

## 2. Build, install, and start

From the repository root:

```bash
cargo build --release -p koko-cli
./target/release/koko
```

To install `koko` into Cargo's binary directory:

```bash
cargo install --path crates/koko-cli
koko
```

Useful startup forms:

```bash
koko                                      # normal interactive session
koko --quiet                              # prompt, but no greeting
koko --no-config --no-history             # isolated/manual test session
koko --color never                        # deterministic plain terminal text
NO_COLOR=1 koko                           # disable ANSI color by convention
TERM=dumb koko                            # simple line-oriented fallback
koko --help
koko --version
```

There is no positional database argument. `koko some.db` is intentionally an error.

A normal session starts as:

```text
Koko <version> · in-memory · graph main
Type :help for help; Ctrl-D or :quit to exit.

koko[main]>
```

Prompt states:

```text
koko[main]>                 selected graph, no explicit transaction
koko[analytics]>            another selected graph
koko[analytics|tx]>         active read-write transaction
koko[analytics|ro-tx]>      active read-only transaction
```

Run `:status` whenever you are unsure what the current session is using.

## 3. Main tutorial: build, save, and explore rich data

This is the main entry point for learning the application. Work through it in order: first build a
small graph by hand so every piece is visible, then load the feature-rich TinySNB dataset from the
companion repository and use it for more interesting queries.

The tutorial has two independent stages:

1. **Build and preserve your own database:** typed schema, data creation, querying, mutation,
   parameters, an index, a transaction, a schemaless graph, logical export, destructive changes,
   and atomic restore.
2. **Explore rich imported data:** source files, several node and relationship tables, traversal,
   aggregation, variable-length paths, optional matches, nested values, plans, machine output, and
   another logical export.

Paste each fenced Cypher block as one bracketed paste, or enter a complete one-line statement at a
time. The fenced `cypher` blocks deliberately remain pure Cypher so they also work in command files
and batch mode. When typing a displayed block one physical line at a time, append `\` and press
normal Enter after any line that would otherwise submit; Alt-Enter (or Escape then Enter) remains
the keyboard-only equivalent. In section 3.2, type the `MATCH` line as
`MATCH (ada:Person {id: 1}), (linus:Person {id: 2}) \`, then type the `CREATE` line normally.
Interactive continuation markers are not valid in files or other batch sources.

### 3.1 Declare a typed schema

```cypher
CREATE NODE TABLE Person(
    id INT64,
    name STRING,
    age INT64,
    PRIMARY KEY(id)
);

CREATE REL TABLE Knows(
    FROM Person TO Person,
    since INT64
);
```

Inspect it without parsing rendered `SHOW` output:

```text
:schema
:describe Person
:describe Knows
```

`:schema` emits deterministic executable Cypher. `:describe` emits structured logical rows.

### 3.2 Create nodes and a relationship

```cypher
CREATE (:Person {id: 1, name: 'Ada', age: 36}),
       (:Person {id: 2, name: 'Linus', age: 34}),
       (:Person {id: 3, name: 'Grace', age: 40});

MATCH (ada:Person {id: 1}), (linus:Person {id: 2})
CREATE (ada)-[:Knows {since: 2024}]->(linus);
```

### 3.3 Query it

```cypher
MATCH (p:Person)
RETURN p.id, p.name, p.age
ORDER BY p.age DESC;
```

```cypher
MATCH (a:Person)-[k:Knows]->(b:Person)
RETURN a.name AS person, k.since, b.name AS knows;
```

```cypher
MATCH (p:Person)
WHERE p.age >= 35
RETURN count(*) AS people, min(p.age) AS youngest, max(p.age) AS oldest;
```

### 3.4 Mutate and verify

```cypher
MATCH (p:Person {id: 2})
SET p.age = 35
RETURN p.name, p.age;
```

```cypher
MERGE (p:Person {id: 4})
ON MATCH SET p.age = 37
ON CREATE SET p.name = 'Margaret', p.age = 37
RETURN p.id, p.name, p.age;
```

`MERGE` uses the pattern in parentheses as its lookup key. Here it looks up `Person` by the
primary-key property `id`: `ON MATCH` updates the existing person, while `ON CREATE` initializes
the properties of a newly created person. Keeping `name` and `age` out of the `MERGE` pattern avoids
treating changed profile data as a different person.

```cypher
MATCH (p:Person)
RETURN p.id, p.name, p.age
ORDER BY p.id;
```

### 3.5 Inspect, parameterize, index, and plan

At this point `main` contains four people and one `Knows` relationship. Check the authoritative
session and catalog state:

```text
:status
:graphs
:schema
:describe Person
```

Bind a value instead of interpolating it into Cypher:

```text
:param min_age 35
:params
```

```cypher
MATCH (p:Person)
WHERE p.age >= $min_age
RETURN p.name, p.age
ORDER BY p.age DESC, p.name;
```

The result contains Grace, Margaret, Ada, and Linus. `:params` shows `min_age` and its logical type
without disclosing the value; `:params --values` reveals it explicitly.

Create and inspect a graph-scoped HASH index:

```cypher
CREATE HASH INDEX person_id_lookup FOR (p:Person) ON (p.id);
CALL show_indexes() RETURN index_name, table_name, property_names, index_type;
```

Ask the engine for a structured plan without running the query:

```cypher
EXPLAIN
MATCH (p:Person)
WHERE p.id = 2
RETURN p.name, p.age;
```

Then execute and profile the same query:

```cypher
PROFILE
MATCH (p:Person)
WHERE p.id = 2
RETURN p.name, p.age;
```

`EXPLAIN` is observational. `PROFILE` executes its inner statement and returns both its result and
available profile measurements.

### 3.6 Make a change transactionally, then roll it back

The prompt adds `|tx` while the transaction is active:

```cypher
BEGIN TRANSACTION;

MATCH (p:Person {id: 2})
SET p.age = 99
RETURN p.name, p.age;
```

The connection sees its own uncommitted value. Inspect the state, then roll it back:

```text
:status
```

```cypher
ROLLBACK;

MATCH (p:Person {id: 2})
RETURN p.name, p.age;
```

Linus is back to age 35. This is also the safe pattern for experimenting with DDL or mutations:
begin, inspect the changed state through the same connection, and explicitly commit or roll back.

### 3.7 Add a second, schemaless graph

Create a graph whose labels, relationship types, and properties come directly from values:

```cypher
CREATE GRAPH notes ANY;
USE GRAPH notes;

CREATE (:Topic {name: 'Cypher', tags: ['graphs', 'queries']}),
       (:Topic {name: 'Rust', tags: ['systems']});

MATCH (cypher:Topic {name: 'Cypher'}), (rust:Topic {name: 'Rust'})
CREATE (cypher)-[:IMPLEMENTED_IN {project: 'Koko'}]->(rust);
```

Query and inspect it:

```cypher
MATCH (a:Topic)-[r:IMPLEMENTED_IN]->(b:Topic)
RETURN a.name AS topic, r.project AS project, b.name AS language;
```

```text
:graphs
:schema notes
```

Switch back to the typed graph:

```cypher
USE GRAPH main;
```

The same connection now owns one typed graph and one `ANY` graph. Both use the same query,
transaction, result, and output path.

### 3.8 Export, deliberately damage, and atomically restore your database

Choose a **fresh, nonexistent directory** for the export. The relative path below is convenient when
running from a disposable working directory; replace it with an absolute path if you want to retain
the backup:

```cypher
EXPORT DATABASE './koko-tutorial-backup' (format='csv');
```

`EXPORT DATABASE` is intentionally database-wide. The manifest contains `main`, `notes`, both
graphs' schemas and data, and `person_id_lookup`. It is the supported logical interchange format,
not a native mutable database file.

Now make destructive changes:

```cypher
USE GRAPH notes;
MATCH (n) DETACH DELETE n;

USE GRAPH main;
DROP GRAPH notes;

MATCH (p:Person {id: 4})
DETACH DELETE p;
```

Confirm the damage:

```text
:graphs
```

```cypher
MATCH (p:Person) RETURN count(*) AS people;
```

There are now three people and no `notes` graph. Restore the complete exported database:

```cypher
IMPORT DATABASE './koko-tutorial-backup';
```

Verify every important layer rather than trusting a status message:

```text
:graphs
:schema main
:schema notes
```

```cypher
USE GRAPH notes;
MATCH (a:Topic)-[r:IMPLEMENTED_IN]->(b:Topic)
RETURN a.name, r.project, b.name;

USE GRAPH main;
MATCH (p:Person) RETURN count(*) AS people;
CALL show_indexes() RETURN index_name, index_type;
```

The expected count is four, the `notes` relationship is back, and the index is
`person_id_lookup | HASH`. The import replaced the database atomically; it did not merge rows into
the damaged state.

### 3.9 Load the feature-rich TinySNB dataset

The hand-built graph made ownership and state transitions explicit. For richer queries, use the
existing TinySNB fixture in the companion C++ oracle checkout:

```text
/Users/dai/code/koko/dataset/tinysnb/
```

TinySNB is intentionally small enough for an instant tutorial but broad in shape and types:

- 8 people with booleans, dates, timestamps, intervals, UUIDs, lists, nested lists, and arrays;
- 3 organisations with structs, nested structs, lists, and unions;
- 3 movies with Unicode names, blobs, maps, unions, timestamp variants, unsigned integers, and
  `INT128`;
- `knows`, `studyAt`, `workAt`, `meets`, and `marries` relationships with temporal, nested, list,
  map, union, fixed-array, and blob properties;
- 14 directed `knows` edges, enough cycles and branching for aggregation and multi-hop traversal.

Keep the first tutorial export, then leave the original process:

```text
:quit
```

Start a fresh process with TinySNB as the working directory so the fixture's relative `COPY` paths
resolve naturally:

```bash
cd /Users/dai/code/koko/dataset/tinysnb
/Users/dai/code/koko-rs/target/release/koko --no-config
```

If `koko` is installed, the second line can simply be `koko --no-config`.

Create a named typed graph and execute the fixture's real schema and import scripts:

```cypher
CREATE GRAPH tinysnb;
USE GRAPH tinysnb;
```

```text
:read schema.cypher
:read copy.cypher
```

The source runner executes both files through the same connection and engine path as typed input.
Inspect what arrived:

```text
:graphs
:schema tinysnb
:describe person
:describe knows
```

Take a quick inventory:

```cypher
MATCH (p:person) RETURN count(*) AS people;
MATCH (o:organisation) RETURN count(*) AS organisations;
MATCH (m:movies) RETURN count(*) AS movies;
MATCH ()-[k:knows]->() RETURN count(*) AS knows_edges;
```

The four results are `8`, `3`, `3`, and `14`.

### 3.10 Query people and rich scalar/list values

Start with ordinary filtering and ordering:

```cypher
MATCH (p:person)
RETURN p.fName AS person,
       p.age AS age,
       p.isStudent AS student,
       p.usedNames AS aliases
ORDER BY p.age DESC, p.fName;
```

The oldest row is the deliberately long-named Hubert at 83; the result also demonstrates booleans
and lists without flattening them into strings.

Bind the cutoff as session state:

```text
:param min_age 35
```

```cypher
MATCH (p:person)
WHERE p.age >= $min_age
RETURN p.fName AS person, p.age AS age
ORDER BY age DESC, person;
```

Expected people: Hubert (83), Carol (45), Greg (40), and Alice (35).

Expand a list into rows and aggregate it back per person:

```cypher
MATCH (p:person)
UNWIND p.workedHours AS hours
RETURN p.fName AS person, sum(hours) AS total_worked_hours
ORDER BY total_worked_hours DESC, person;
```

This produces one row per person; Hubert has the largest sum, 58.

### 3.11 Traverse and aggregate the social graph

Count each person's outgoing relationships:

```cypher
MATCH (p:person)-[:knows]->(friend:person)
RETURN p.fName AS person, count(friend) AS direct_friends
ORDER BY direct_friends DESC, person;
```

Expected result:

```text
Alice      3
Bob        3
Carol      3
Dan        3
Elizabeth  2
```

Explore one or two hops from Alice:

```cypher
MATCH (alice:person {fName: 'Alice'})-[:knows*1..2]->(reachable:person)
RETURN DISTINCT reachable.fName AS reachable
ORDER BY reachable;
```

Expected names are Alice, Bob, Carol, and Dan. Alice is reachable from herself through a directed
cycle; `DISTINCT` removes duplicate path endpoints.

Combine node, relationship, struct, nested-struct, and list properties:

```cypher
MATCH (p:person)-[k:knows]->(friend:person)
WHERE size(k.summary.locations) > 1
RETURN p.fName AS person,
       friend.fName AS friend,
       k.summary.locations AS places,
       k.summary.transfer.amount AS transfers
ORDER BY person, friend;
```

This is useful for seeing that rich values remain typed through pattern matching and projection;
the query does not parse a rendered relationship string.

### 3.12 Join different entities and preserve missing values

Follow relationship properties to organisations:

```cypher
MATCH (p:person)-[study:studyAt]->(school:organisation)
RETURN p.fName AS person,
       school.name AS organisation,
       study.year AS since
ORDER BY person;
```

Expected rows are Alice/ABFsUni/2021, Bob/ABFsUni/2020, and Farooq/ABFsUni/2020.

Use optional matches to retain every person:

```cypher
MATCH (p:person)
OPTIONAL MATCH (p)-[:studyAt]->(school:organisation)
OPTIONAL MATCH (p)-[:workAt]->(employer:organisation)
RETURN p.fName AS person,
       school.name AS school,
       employer.name AS employer
ORDER BY person;
```

All eight people remain. Missing school or employer values are real NULLs, distinct from empty
strings.

### 3.13 Inspect nested, union, map, blob, temporal, and Unicode values

Switch briefly to JSON so values that JSON cannot represent natively show their normative typed
wrappers:

```text
:format json
```

```cypher
MATCH (m:movies)
RETURN m.name AS movie,
       m.length AS minutes,
       m.description.rating AS rating,
       m.description.film AS release_date,
       m.audience AS audience
ORDER BY movie;
```

The three rows include accented text, an emoji-heavy title, typed dates, and maps represented as
ordered entry pairs.

```cypher
MATCH (o:organisation)
RETURN o.name AS organisation,
       o.state.revenue AS revenue,
       o.state.location AS locations,
       o.info AS info
ORDER BY organisation;
```

The `info` column preserves each union tag (`price`, `note`, or `movein`) rather than collapsing
different alternatives.

```cypher
MATCH (p:person)-[meeting:meets]->(friend:person)
RETURN p.fName AS person,
       friend.fName AS friend,
       meeting.location AS coordinates,
       meeting.data AS payload
ORDER BY person, friend;
```

Fixed arrays remain arrays and blobs use tagged base64 objects. Return to terminal-friendly output:

```text
:format auto
```

### 3.14 Use plans, timings, limits, completion, and history on real data

Enable timing and set explicit execution controls:

```text
:timing on
:progress auto
```

```cypher
CALL threads=4;
CALL timeout=5000;
CALL current_setting('threads') RETURN *;
CALL current_setting('timeout') RETURN *;
```

Plan and profile a traversal:

```cypher
EXPLAIN
MATCH (p:person)-[:knows]->(friend:person)
WHERE p.age >= $min_age
RETURN p.fName, friend.fName;
```

```cypher
PROFILE
MATCH (p:person)-[:knows]->(friend:person)
WHERE p.age >= $min_age
RETURN p.fName, friend.fName;
```

The first statement validates and plans; the second executes and adds profile measurements. Disable
the tutorial deadline afterward:

```cypher
CALL timeout=0;
```

Try completion against the now-populated catalog:

```text
MATCH (p:per<Tab>) RETURN p.<Tab>
RETURN $min<Tab>
:desc<Tab> per<Tab>
```

Then inspect the admitted entries:

```text
:history show 10
:functions count
:status
```

`:rows 3` is a safe display experiment: rerun the people query and only three human rows are shown,
but the engine still computes all eight. Restore the default with `:rows default`.

### 3.15 Export a report and the complete rich database

Capture one query as a valid atomic JSON document. `replace` is explicit so rerunning the tutorial
cannot accidentally append two documents:

```text
:format json
:output /tmp/koko-tinysnb-report.json replace
```

```cypher
MATCH (p:person)-[:knows]->(friend:person)
RETURN p.fName AS person, friend.fName AS friend
ORDER BY person, friend;
```

Close and publish that output transaction, then restore normal display:

```text
:output stdout
:format auto
```

For a delimited query artifact instead, choose a fresh filename:

```cypher
COPY (
    MATCH (p:person)-[:knows]->(friend:person)
    RETURN p.fName AS person, friend.fName AS friend
    ORDER BY person, friend
) TO '/tmp/koko-tinysnb-friends.csv' (header=true);
```

Finally export the **whole logical database**. The destination directory must not already exist:

```cypher
EXPORT DATABASE '/tmp/koko-tinysnb-database' (format='csv');
```

This export includes the empty `main` graph and populated `tinysnb` graph. Koko does not pretend
that a selected-graph export is a native database file; database-wide logical interchange is the
atomic ownership boundary.

Prove portability in a fresh process:

```text
:quit
```

```bash
koko --no-config
```

```cypher
IMPORT DATABASE '/tmp/koko-tinysnb-database';
```

```text
:graphs
```

```cypher
USE GRAPH tinysnb;
MATCH (p:person) RETURN count(*) AS people;
```

The restored count is eight. You have now exercised the complete core loop: start empty, define a
typed graph, create and mutate data, use transactions and parameters, add a schemaless graph,
inspect and plan, export and restore, import a rich fixture through source files, traverse and
aggregate it, inspect complex values, capture machine output, and move the complete database into a
new process.

The remaining sections are reference material for each capability, edge case, and operational
control used by this tutorial.

## 4. Entering and editing queries

### 4.1 Submission rules

Parser-aware multiline mode is on by default:

- Enter at the end of a parse-complete buffer submits it.
- Enter in an incomplete buffer inserts a newline and displays the continuation prompt.
- Enter away from the buffer end inserts a newline instead of submitting unexpectedly.
- `Ctrl-J` force-submits the normalized current buffer.
- Alt-Enter inserts a newline into an otherwise complete buffer. If the terminal cannot distinguish
  Alt-Enter, press Escape and then Enter.
- A trailing standalone `\` code token explicitly requests another line, even with `:multiline off`.
- Semicolons separate statements. Several statements in one submission execute in source order;
  omitting a semicolon does not itself request another line.
- A meta command starts with `:` as the first non-whitespace token and occupies its own logical line.

Completeness is syntactic, not semantic. The editor asks the canonical parser whether the current
buffer forms a statement; it does not bind or execute that statement speculatively. Consequently,
`MATCH (ada:Person {id: 1}), (linus:Person {id: 2})` is parse-complete. Normal Enter submits it, and
the binder then rejects it because it neither returns results nor writes data. A following
`CREATE (ada)-[:Knows]->(linus)` is a new statement with unbound endpoints, which explains the
second error.

Append a standalone backslash to override submission:

```text
MATCH (ada:Person {id: 1}), (linus:Person {id: 2}) \
CREATE (ada)-[:Knows {since: 2024}]->(linus);
```

The marker must be the final non-whitespace code token. The editor recognizes it through the
canonical lexer, removes it before history and execution, and retains the newline. Backslashes inside
single/double-quoted strings, backtick identifiers, line/block comments, and meta-command lines are
ordinary source text. The marker may be used redundantly after an already-incomplete expression; it
does not declare or validate a per-line grammar fragment.

For the following query, press normal Enter after every line; each of the first three backslashes
requests the continuation prompt:

```text
koko[main]> MATCH (p:Person) \
         ...> WHERE p.age >= 35 \
         ...> RETURN p.name, p.age \
         ...> ORDER BY p.age DESC;
```

A bracketed multiline paste is admitted as one edit; embedded newlines do not execute partial input.
Pasted tabs outside quoted strings become four spaces. Tabs/newlines inside quoted literals retain
their semantic content.

### 4.2 Editing key reference

| Keys | Action |
|---|---|
| `Ctrl-A`, Home | Start of current logical line |
| `Ctrl-E`, End | End of current logical line |
| `Ctrl-Home` / `Ctrl-End` | Start/end of the complete buffer |
| `Ctrl-B`, Left / `Ctrl-F`, Right | Move one grapheme |
| Alt-B / Alt-F | Move one word |
| Backspace, `Ctrl-H` | Delete previous grapheme |
| Delete, `Ctrl-D` with input | Delete following grapheme |
| `Ctrl-W`, Alt-Backspace | Delete previous word |
| `Ctrl-U` | Clear the complete buffer |
| `Ctrl-K` | Delete from cursor to end |
| `Ctrl-T` | Transpose adjacent graphemes |
| `Ctrl-L` | Clear and redraw |
| `Ctrl-P`, Up / `Ctrl-N`, Down | History or visual-line navigation |
| `Ctrl-R` | Reverse incremental history search |
| Tab / Shift-Tab | Complete/select next or previous candidate |
| `Ctrl-G` | Dismiss menu/search or cancel the edited buffer |
| `Ctrl-C` | Clear input or cancel a running query |
| `Ctrl-D` on empty input | Request a normal exit |

Editing, cursor movement, wrapping, deletion, and truncation are Unicode/grapheme aware.

## 5. Meta-command reference

Command names are ASCII case-insensitive. Arguments preserve case. Paths with spaces can be quoted.
A meta command needs no semicolon.

| Command | Purpose |
|---|---|
| `:help [topic/command]` | Show the live command/topic index or a topic pointer |
| `:quit [--rollback]` | Exit; optionally roll back an active transaction first |
| `:clear` | Clear and redraw the terminal |
| `:status` | Show effective database, graph, transaction, parameters, and UI state |
| `:graphs` | List graphs and mark the selected graph |
| `:schema [graph[.object]]` | Emit deterministic executable schema Cypher |
| `:describe <graph[.object]>` | Describe one table/index/schema object |
| `:functions [pattern]` | List visible functions, kinds, signatures, and result types |
| `:params [--values]` | List parameters; values are hidden unless requested |
| `:param <name> <json>` | Bind or replace a session parameter |
| `:param clear <name>` | Remove one parameter |
| `:param clear all` | Remove all parameters |
| `:format [auto/name]` | Show or choose `auto`, `box`, `table`, `csv`, `tsv`, `json`, `jsonl`, `markdown`, `line`, or `trash` |
| `:timing [on/off]` | Show or hide compile/execution timing |
| `:progress [auto/on/off]` | Control live progress on stderr |
| `:rows [N/all/default]` | Set human TTY display limit; `default` is 20 |
| `:width [N/auto]` | Set human table width |
| `:null [literal/empty]` | Set human NULL rendering |
| `:multiline [on/off]` | Toggle parser-aware multiline entry |
| `:highlight [auto/on/off]` | Control editor highlighting |
| `:completion [on/off]` | Control completion and ghost text |
| `:history` | Show current history state |
| `:history show [N]` | Show recent entries |
| `:history clear` | Confirm and erase persistent history |
| `:history on/off` | Enable/disable recording for this session |
| `:history skip` | Omit the next submitted Cypher statement from persistent history |
| `:read <path> [--keep-going]` | Execute a UTF-8 command file in the current session |
| `:output` | Show the current result destination |
| `:output stdout` | Send later result payloads back to stdout |
| `:output <path>` | Select a new, nonexisting destination |
| `:output <path> append` | Explicitly append to an existing destination |
| `:output <path> replace` | Explicitly replace an existing destination |

With no value, a setting command reports its current value and accepted values:

```text
:format
:rows
:width
:progress
```

High-value help entries:

```text
:help
:help queries
:help keys
:help completion
:help history
:help formats
:help parameters
:help batch
:help graphs
:help transactions
:help cancellation
```

## 6. Introspection: inspect without changing state

Use the CLI's structured metadata commands rather than scraping rendered `SHOW` output:

```text
:status
:graphs
:schema
:schema analytics
:schema analytics.Person
:describe Person
:describe analytics.Person
:functions
:functions count
:params
```

Properties:

- `:status`, `:graphs`, `:schema`, `:describe`, and `:functions` do not switch graphs or alter a
  transaction.
- `:schema` defaults to the selected graph and orders dependencies deterministically.
- In human formats, `:schema` is executable Cypher text.
- In machine formats, `:schema` is a result with one `statement STRING` column.
- `:describe` reports kind, columns, types, primary keys or endpoints, and source information.
- Ambiguous object names produce candidates rather than silently selecting one.

Engine table functions are also queryable when you want to filter or project their logical rows:

```cypher
CALL show_tables() RETURN *;
CALL show_macros() RETURN *;
CALL show_warnings() RETURN *;
CALL current_setting('threads') RETURN *;

CALL show_indexes()
YIELD index_type AS kind, index_name AS idx
WHERE idx = 'person_name_idx'
RETURN idx, kind;
```

A row-producing `CALL` without `YIELD` imports every declared output in declaration order. An
explicit `YIELD output [AS alias], ...` imports any nonempty named subset in caller-written order:
aliases replace their source names, omitted outputs do not enter scope, and the immediate `WHERE`
can reference incoming variables plus the yielded names. `YIELD` does not filter source rows or
change their order or cardinality; `RETURN`, `WITH`, and `WHERE` retain those responsibilities.
Duplicate or unknown outputs, duplicate exposed names, and collisions with incoming variables are
binder errors. `YIELD *` is not supported; omitting the clause already imports the complete schema.

Use `:functions` to discover the currently visible scalar, aggregate, algorithm, and table
functions and their signatures.

## 7. Parameters

Parameters are values, not string interpolation. Bind them once and reference them as `$name` in
ordinary Cypher.

```text
:param min_age 35
:param person_name "Ada"
:params
```

```cypher
MATCH (p:Person)
WHERE p.age >= $min_age AND p.name = $person_name
RETURN p.id, p.name, p.age;
```

JSON syntax is mandatory after `:param`:

```text
:param enabled true
:param missing null
:param ids [1, 2, 3]
:param options {"active":true,"limit":20}
:param title "A JSON string must retain its quotes"
```

Inspect or clear them:

```text
:params                 # name, logical type, and source; no values
:params --values        # explicit value disclosure
:param clear min_age
:param clear all
```

`:param` and `:params` are excluded from history because they may reveal values. Parameter names and
types can appear in completion, but their values do not.

### 7.1 Tagged non-JSON values

Use the same typed mapping as JSON/JSONL output. Common examples:

```text
:param huge {"$type":"INTEGER","logical_type":"INT128","value":"9007199254740993"}
:param amount {"$type":"DECIMAL","precision":18,"scale":2,"value":"1234.50"}
:param day {"$type":"DATE","value":"2026-07-23"}
:param id {"$type":"UUID","value":"550e8400-e29b-41d4-a716-446655440000"}
:param bytes {"$type":"BLOB","encoding":"base64","value":"SGVsbG8="}
```

A raw JSON object whose `$type` key would collide with a recognized tag can be wrapped explicitly:

```text
:param raw {"$type":"JSON","value":{"$type":"DATE","note":"ordinary JSON"}}
```

## 8. Graphs: typed and `ANY`

### 8.1 Typed named graph

```cypher
CREATE GRAPH analytics;
USE GRAPH analytics;
```

The prompt becomes `koko[analytics]>`. Define tables before creating data:

```cypher
CREATE NODE TABLE Event(
    id INT64,
    title STRING,
    occurred_at TIMESTAMP,
    PRIMARY KEY(id)
);
```

### 8.2 Schemaless `ANY` graph

```cypher
CREATE GRAPH scratch ANY;
USE GRAPH scratch;
```

Create labels, relationship types, and properties directly:

```cypher
CREATE (:User:Admin {name: 'Ada', score: 10}),
       (:User {name: 'Linus', active: true});

MATCH (a:User {name: 'Ada'}), (b:User {name: 'Linus'})
CREATE (a)-[:FOLLOWS {since: 2024}]->(b);

MATCH (a:User)-[r:FOLLOWS]->(b:User)
RETURN labels(a), a.name, r.since, b.name;
```

Typed and `ANY` graphs use the same parser, planner, executor, transaction machinery, formats, and
introspection commands.

### 8.3 Navigation and cleanup

```text
:graphs
```

```cypher
USE GRAPH main;
DROP GRAPH scratch;
```

Rules:

- You cannot switch graphs while an explicit transaction is active.
- Complete or roll back the transaction before graph management.
- Graph registry DDL is database-wide rather than a transactional savepoint. Do not use
  `CREATE GRAPH`/`DROP GRAPH` as rollback experiments.
- If another connection drops the selected graph, the session refreshes to authoritative state.

## 9. Typed schema management

### 9.1 Node and relationship tables

```cypher
CREATE NODE TABLE City(
    id INT64,
    name STRING,
    population INT64,
    PRIMARY KEY(id)
);

CREATE REL TABLE LivesIn(
    FROM Person TO City,
    since INT64
);
```

A relationship group can support several endpoint pairs:

```cypher
CREATE REL TABLE ConnectedTo(
    FROM Person TO Person,
    FROM Person TO City,
    weight DOUBLE
);
```

### 9.2 Alter schema

```cypher
ALTER TABLE Person ADD email STRING;
ALTER TABLE Person ADD score INT64 DEFAULT 0;
ALTER TABLE Person RENAME email TO email_address;
ALTER TABLE Person DROP email_address;
ALTER TABLE Person RENAME TO Account;
ALTER TABLE Account RENAME TO Person;
```

Primary-key properties cannot be dropped or updated in place. Delete/reinsert the node when its key
must change.

### 9.3 Indexes

HASH and ART indexes are graph-scoped. ART indexes currently apply only to node primary keys, and
only one explicit index may target a property at a time:

```cypher
CREATE HASH INDEX person_pk FOR (p:Person) ON (p.id);
CALL show_indexes() RETURN *;
DROP INDEX person_pk;

CREATE ART INDEX person_pk_art FOR (p:Person) ON (p.id);
CALL show_indexes() RETURN *;
DROP INDEX person_pk_art;
```

Index DDL participates in the selected graph's transaction semantics.

### 9.4 Sequences and `SERIAL`

```cypher
CREATE SEQUENCE ticket START WITH 1000 INCREMENT BY 1;
RETURN nextval('ticket');
RETURN currval('ticket');
DROP SEQUENCE ticket;
```

A `SERIAL` primary-key column uses an implicit sequence:

```cypher
CREATE NODE TABLE LogEntry(id SERIAL, message STRING, PRIMARY KEY(id));
CREATE (:LogEntry {message: 'started'});
MATCH (entry:LogEntry) RETURN entry.id, entry.message;
```

## 10. Query and mutation cookbook

### 10.1 Filtering, ordering, pagination

```cypher
MATCH (p:Person)
WHERE p.age >= 30 AND p.name <> 'Ada'
RETURN p.id, p.name, p.age
ORDER BY p.age DESC, p.name
SKIP 5
LIMIT 10;
```

### 10.2 Aggregation and `WITH`

```cypher
MATCH (p:Person)
WITH count(*) AS total, avg(p.age) AS average_age
RETURN total, average_age;
```

```cypher
MATCH (p:Person)-[:Knows]->(friend:Person)
WITH p, count(friend) AS degree
WHERE degree > 0
RETURN p.name, degree
ORDER BY degree DESC;
```

### 10.3 Optional patterns

```cypher
MATCH (p:Person)
OPTIONAL MATCH (p)-[:Knows]->(friend:Person)
RETURN p.name, collect(friend.name) AS friends
ORDER BY p.name;
```

### 10.4 Variable-length paths

```cypher
MATCH (start:Person {id: 1})-[:Knows*1..3]->(reachable:Person)
RETURN DISTINCT reachable.name
ORDER BY reachable.name;
```

### 10.5 `UNWIND`

```cypher
UNWIND [10, 20, 30] AS value
RETURN value, value * 2 AS doubled;
```

```cypher
UNWIND range(1000, 1099) AS id
CREATE (:Person {id: id, name: 'generated', age: 0});
```

### 10.6 `MERGE`

```cypher
MERGE (p:Person {id: 10})
ON CREATE SET p.name = 'New', p.age = 20
ON MATCH SET p.age = p.age + 1
RETURN p.id, p.name, p.age;
```

```cypher
MATCH (a:Person {id: 1}), (b:Person {id: 2})
MERGE (a)-[r:Knows {since: 2024}]->(b)
ON MATCH SET r.since = 2025
RETURN r.since;
```

### 10.7 Update and delete

```cypher
MATCH (p:Person {id: 1})
SET p.age = 37
RETURN p.age;
```

Whole-value assignment updates only listed non-key properties:

```cypher
MATCH (p:Person {id: 1})
SET p = {name: 'Ada Lovelace', age: 37};
```

Delete a relationship, then a node:

```cypher
MATCH (:Person {id: 1})-[r:Knows]->(:Person)
DELETE r;

MATCH (p:Person {id: 1})
DELETE p;
```

Or delete a node and its incident relationships together:

```cypher
MATCH (p:Person {id: 1})
DETACH DELETE p;
```

## 11. Transactions

### 11.1 Read-write transaction

```cypher
BEGIN TRANSACTION;

MATCH (p:Person {id: 1})
SET p.age = p.age + 1;

MATCH (p:Person {id: 1})
RETURN p.name, p.age;

COMMIT;
```

Rollback experiment:

```cypher
BEGIN TRANSACTION;
CREATE (:Person {id: 999, name: 'temporary', age: 0});
MATCH (p:Person {id: 999}) RETURN p;
ROLLBACK;
MATCH (p:Person {id: 999}) RETURN count(*) AS remaining;
```

### 11.2 Read-only transaction

```cypher
BEGIN TRANSACTION READ ONLY;
MATCH (p:Person) RETURN count(*) AS people;
COMMIT;
```

A write in a read-only transaction is rejected. Read-only transactions retain their snapshot until
commit/rollback.

### 11.3 Failure and exit rules

- Only one explicit transaction can be active on a connection.
- A statement error may abort the active transaction. Check the prompt or `:status` before issuing a
  follow-up `COMMIT`.
- `:quit` or Ctrl-D refuses to exit while a transaction is active.
- Use `COMMIT`, `ROLLBACK`, or `:quit --rollback` explicitly.
- The CLI never commits automatically during exit.

## 12. Plans, profiling, functions, and macros

### 12.1 Explain without executing

```cypher
EXPLAIN
MATCH (a:Person)-[:Knows]->(b:Person)
WHERE a.age > 30
RETURN a.name, b.name;
```

Regular queries produce a structured Rust plan. Non-query `EXPLAIN`, such as `EXPLAIN CREATE ...`,
validates the statement without applying it.

### 12.2 Profile while executing

```cypher
PROFILE
MATCH (a:Person)-[:Knows]->(b:Person)
RETURN a.name, b.name;
```

`PROFILE` executes the statement. This matters for writes and DDL: `PROFILE CREATE ...` performs the
creation rather than merely validating it.

### 12.3 Discover functions

```text
:functions
:functions count
:functions date
```

You can also query table functions; section 6 defines their name-based `YIELD` behavior:

```cypher
CALL show_tables() RETURN *;
CALL show_indexes()
YIELD index_name, index_type
RETURN index_name, index_type;
```

### 12.4 Scalar macros

Macros are the REPL-definable function mechanism:

```cypher
CREATE MACRO older_than(age, cutoff := 30) AS age > cutoff;
RETURN older_than(36), older_than(20, 18);
```

```cypher
CREATE MACRO display_name(person) AS person.name;
MATCH (p:Person) RETURN display_name(p);
```

```cypher
CALL show_macros() RETURN *;
DROP MACRO older_than;
```

Macro creation/removal participates in graph transaction snapshots.

Native Rust scalar UDF callbacks are available through the embedded `koko` API, not definable
from the REPL. A callback registered by an embedding application would appear in `:functions` on its
own connection.

## 13. Engine runtime settings

The most useful query/session controls use standalone `CALL` statements:

```cypher
CALL threads=4;
CALL timeout=5000;
CALL warning_limit=100;
CALL enable_plan_optimizer=true;
```

`timeout` is in milliseconds; `CALL timeout=0` disables the query deadline. Inspect values with:

```cypher
CALL current_setting('threads') RETURN *;
CALL current_setting('timeout') RETURN *;
CALL current_setting('warning_limit') RETURN *;
```

File resolution controls:

```cypher
CALL home_directory='/absolute/home';
CALL file_search_path='/data/first,/data/second';
```

Recursive path controls include:

```cypher
CALL var_length_extend_max_depth=20;
CALL recursive_pattern_semantic='TRAIL';
```

Some accepted C++-compatibility setting names concern deferred persistence subsystems. Their presence
in `current_setting` does not activate native database durability. Use `:status` for the effective CLI
view of timeout/workers and presentation state.

The tracked database memory limit is observable in `:status`, but the standalone first-party CLI has
no flag or meta command to change the database-level limit after startup.

## 14. Loading, copying, and saving data

### 14.1 Ad hoc file query with `LOAD FROM`

Given `people.csv`:

```csv
id,name,age
1,Ada,36
2,Linus,34
```

Query it without first creating a table:

```cypher
LOAD FROM 'people.csv'
RETURN id, name, age
ORDER BY id;
```

For a file without a header:

```cypher
LOAD FROM 'people.csv' (header=false)
RETURN column0, column1, column2;
```

### 14.2 Copy a file into a typed table

```cypher
CREATE NODE TABLE ImportedPerson(
    id INT64,
    name STRING,
    age INT64,
    PRIMARY KEY(id)
);

COPY ImportedPerson FROM 'people.csv' (header=true);
MATCH (p:ImportedPerson) RETURN p.id, p.name, p.age ORDER BY p.id;
```

Useful CSV options include `header`, `delim`, `parallel`, `ignore_errors`, and explicit
`file_format='csv'`. Invalid options are rejected rather than silently ignored.

Parquet is selected by file extension. NPY column loading uses one file per declared column:

```cypher
COPY Measurements FROM ('id_int64.npy', 'value_double.npy') BY COLUMN;
```

### 14.3 Copy query results out

```cypher
COPY (
    MATCH (p:Person)
    RETURN p.id AS id, p.name AS name, p.age AS age
    ORDER BY p.id
) TO 'people-out.csv' (header=true);
```

### 14.4 Logical database backup and restore

Logical interchange is the supported way to carry an in-memory database across processes. It is not
a native database file.

Export every graph, schema object, index, and row atomically into a fresh directory:

```cypher
EXPORT DATABASE './koko-backup' (format='csv');
```

Start a new `koko` process and restore it:

```cypher
IMPORT DATABASE './koko-backup';
:graphs
:schema
```

Important:

- `EXPORT DATABASE` is database-wide even when a named graph is selected.
- `IMPORT DATABASE` atomically replaces the complete database graph registry, not just the selected
  graph.
- Export before experimenting with destructive imports.
- CSV and Parquet logical interchange are supported; the generated manifest/schema layout is the
  interchange contract, not a native durable store.

### 14.5 Run explicit source files

A command file may mix Cypher and meta commands:

```text
# setup.cypher
CREATE GRAPH analytics;
USE GRAPH analytics;
CREATE NODE TABLE Event(id INT64, title STRING, PRIMARY KEY(id));
:param source "manual"
```

Execute it in the current session:

```text
:read setup.cypher
```

Or at process startup:

```bash
koko --init setup.cypher
```

Nested `:read` paths resolve relative to the including file. Include cycles are rejected with the
full chain. There is no shell expansion, command substitution, remote fetch, or implicit executable
file behavior.

### 14.6 Local read-only `icebug-disk`

`icebug-disk` is a narrow local immutable table format backed by a validated Parquet/CSR directory.
It is not a generic connector or remote backend.

A compatible schema looks like:

```cypher
CREATE NODE TABLE user(
    id INT32,
    name STRING,
    age INT64,
    PRIMARY KEY(id)
) WITH (storage='.', format='icebug-disk');

CREATE REL TABLE follows(
    FROM user TO user,
    since INT32
) WITH (storage='.', format='icebug-disk');
```

The source directory must contain the exact owned `icebug-disk` node/relationship files and metadata.
These tables are read-only: writes, ALTER, and COPY mutations are rejected. For a working local
example in the companion C++ checkout:

```bash
cd /Users/dai/code/koko/dataset/ice-disk-test
/Users/dai/code/koko-rs/target/release/koko
```

Then:

```text
:read schema.cypher
```

```cypher
MATCH (a:user {id: 100})-[:follows*1..2]->(b:user)
RETURN DISTINCT b.name
ORDER BY b.name;
```

## 15. Result formats and display controls

### 15.1 Format summary

| Format | Best use |
|---|---|
| `auto` | `box` on capable TTY, `table` on dumb TTY, `tsv` when redirected |
| `box` | Unicode human table |
| `table` | ASCII human table |
| `markdown` | Copying results into documentation |
| `line` | One labeled value per physical line |
| `csv` | RFC 4180-style exchange |
| `tsv` | Escaped one-record-per-line exchange |
| `json` | One versioned document containing all result sets |
| `jsonl` | Streaming typed records |
| `trash` | Execute and consume without row payload |

Switch interactively:

```text
:format box
:format markdown
:format json
:format auto
```

Display controls:

```text
:rows 50
:rows all
:rows default
:width 120
:width auto
:null literal
:null empty
:timing on
:progress auto
```

`:rows` and `:width` affect human TTY display only. They never add a Cypher `LIMIT`, and redirected
or batch output is not display-truncated.

### 15.2 NULL and empty strings

- Human tables show NULL as `NULL` by default.
- Human tables show an empty string as `''`.
- CSV/TSV use unquoted `\N` as the default NULL token.
- A literal string `\N`, empty string, and NULL remain distinct.
- Command-line `--null <TOKEN>` changes the CSV/TSV NULL token.

### 15.3 Multiple results

Human formats label multiple row-producing results as `Result 1 of N`, `Result 2 of N`, and so on.
JSON and JSONL are designed for several result sets. CSV/TSV allow only one row-producing result set
per submission/invocation and reject an ambiguous multi-result stream before disclosing row data.

### 15.4 JSON and JSONL guarantees

`json` produces one versioned envelope:

```json
{
  "version": 1,
  "complete": true,
  "results": [
    {
      "statement": 1,
      "columns": [{"name": "answer", "type": "INT64"}],
      "rows": [[42]],
      "summary": {"rows": 1}
    }
  ]
}
```

Rows are positional arrays, so duplicate column names remain lossless. Wide integers, decimals,
temporal values, blobs, graph values, maps, structs, unions, and nonfinite floats use explicit typed
wrappers where native JSON would lose information.

On error or cancellation, JSON remains syntactically valid with `"complete": false` and a structured
error. JSONL ends with an error record. The process still exits nonzero and writes the human
diagnostic to stderr.

## 16. Redirecting and capturing results

### 16.1 Change destination inside the REPL

```text
:format tsv
:output ./people.tsv
```

```cypher
MATCH (p:Person) RETURN p.id, p.name, p.age ORDER BY p.id;
```

```text
:output stdout
```

An existing path is refused unless you say `append` or `replace` explicitly:

```text
:output ./people.tsv append
:output ./people.tsv replace
```

Only result payload goes to the file. Prompts, progress, warnings, errors, and timing remain on
stderr. A successful replacement is published atomically; a failed write leaves the prior file
untouched and removes temporary output.

### 16.2 Redirect stdout while keeping an interactive editor

```bash
koko >people.tsv
```

Stdin is still a TTY, so the REPL, prompts, and editor remain on stderr. Query payload goes to
`people.tsv`, defaults to complete TSV, and is never display-truncated.

### 16.3 One-off batch capture

```bash
koko --command "RETURN 42 AS answer" --format csv
```

```bash
koko --file report.cypher --format json --output report.json --force
```

The REPL reference is the focus of this guide, but the same session runner, parser, engine path,
parameters, formats, and output transactions back batch mode.

## 17. Completion, highlighting, and history

### 17.1 Completion

Tab completion covers:

- meta commands and valid arguments;
- Cypher keywords;
- graph names;
- node and relationship labels;
- variables in scope;
- bound parameter names;
- properties valid for resolved variables;
- scalar, aggregate, and table functions;
- setting names and values;
- local paths for `:read`, input, init, and output destinations.

Tab accepts a sole candidate or opens a menu. Typing narrows; Tab/Down moves forward; Shift-Tab/Up
moves backward; Enter accepts; Escape or `Ctrl-G` dismisses. Catalog candidates refresh after schema
or graph changes.

Useful drills:

```text
:sch<Tab>
USE GRAPH ana<Tab>
MATCH (p:Per<Tab>) RETURN p.<Tab>
RETURN $mi<Tab>
```

Disable completion or highlighting independently:

```text
:completion off
:highlight off
```

`NO_COLOR`, `--color never`, or `:highlight off` removes styling without removing diagnostics.

### 17.2 History and sensitive input

Interactive history is on by default; batch history is off. Locations:

- XDG: `$XDG_STATE_HOME/koko/history`, or `~/.local/state/koko/history`
- macOS: `~/Library/Application Support/Koko/history`
- Windows: `%LOCALAPPDATA%\Koko\history`

History behavior:

- A multiline submission is one entry.
- Consecutive duplicates are normalized and suppressed.
- `:param`, `:params`, and history-control commands are not recorded.
- `:history skip` suppresses the next Cypher statement.
- `:history off` stops new recording without erasing old entries.
- `--no-history` disables it for the whole process.
- The default bound is 10,000 complete entries.

History is private local state, not a secret store. Before typing sensitive literals:

```text
:history skip
```

or start with:

```bash
koko --no-history
```

Reverse search:

- `Ctrl-R`: open search; repeat for older matches.
- `Ctrl-S` or Down: newer match.
- Enter: accept and submit.
- Right/End: accept for further editing.
- Escape or `Ctrl-G`: cancel and restore the prior buffer.

## 18. Configuration

User configuration is TOML:

- XDG: `$XDG_CONFIG_HOME/koko/config.toml`, or `~/.config/koko/config.toml`
- macOS: `~/Library/Application Support/Koko/config.toml`
- Windows: `%APPDATA%\Koko\config.toml`

Representative configuration:

```toml
format = "auto"
timing = true
progress = "auto"
color = "auto"
rows = 20
width = "auto"
null_display = "literal"
multiline = true
highlight = "auto"
completion = true
history = true
history_limit = 10000
```

Precedence from lowest to highest:

1. built-in defaults;
2. user configuration;
3. explicit `--init` commands;
4. command-line options;
5. interactive meta-command changes.

Use `--no-config` for reproducible experiments. It does not suppress an explicit `--init`.
Unknown keys and invalid values fail before the data session opens; they are never ignored silently.

## 19. Cancellation, progress, errors, and recovery

### 19.1 At the prompt

- `Ctrl-G` cancels a menu/search or clears the current edit; it never exits.
- `Ctrl-C` clears nonempty input.
- First Ctrl-C on empty input prints `Press Ctrl-D or :quit to exit`.
- A second empty-input Ctrl-C within two seconds exits with status 130 only when no transaction is
  active.
- Ctrl-D on empty input requests normal exit and follows transaction protection.

### 19.2 During execution

- Ctrl-C requests cancellation and prints `Cancelling…`.
- The engine acknowledges with `Query cancelled after <duration>.` and returns to a usable prompt
  when the session remains usable.
- Cancellation, deadline expiration, and tracked-memory exhaustion remain distinct failures.
- A cancelled query never prints a success summary.
- Progress starts only after a short delay, is rate-limited, stays on stderr, and never invents a
  percentage when the engine has none.

A cancellation drill:

```cypher
UNWIND range(0, 100000000) AS value
RETURN sum(value);
```

Press Ctrl-C after `Running…` appears, then prove the connection remains usable:

```cypher
RETURN 42 AS still_usable;
```

### 19.3 Diagnostics

The first line is the authoritative engine message. When a real source span exists, the CLI adds
file/input, line, Unicode-aware column, source text, and a caret marker. Interactive command/query
errors normally return to the prompt.

Warnings stay out of CSV/TSV row data. Inspect retained warnings with:

```cypher
CALL show_warnings() RETURN *;
```

## 20. Manual exploration playbooks

Run each playbook in a fresh process unless it explicitly says otherwise. Playbooks that mention
`Person`/`Knows` assume the schema and data from section 3.

### Playbook A: typed graph, rollback, and plan

```cypher
CREATE NODE TABLE Item(id INT64, name STRING, price DECIMAL(10,2), PRIMARY KEY(id));
CREATE (:Item {id: 1, name: 'book', price: 12.50});
BEGIN TRANSACTION;
MATCH (item:Item {id: 1}) SET item.price = 10.00 RETURN item;
ROLLBACK;
MATCH (item:Item) RETURN item.id, item.name, item.price;
EXPLAIN MATCH (item:Item) WHERE item.id = 1 RETURN item;
PROFILE MATCH (item:Item) WHERE item.id = 1 RETURN item;
```

### Playbook B: graph isolation

```cypher
CREATE GRAPH typed_lab;
USE GRAPH typed_lab;
CREATE NODE TABLE N(id INT64, PRIMARY KEY(id));
CREATE (:N {id: 1});

CREATE GRAPH any_lab ANY;
USE GRAPH any_lab;
CREATE (:N {id: 'schemaless', extra: [1,2,3]});

:graphs
:schema typed_lab
:schema any_lab

USE GRAPH typed_lab;
MATCH (n:N) RETURN n;
USE GRAPH any_lab;
MATCH (n:N) RETURN n;
```

### Playbook C: parameters and formats

```text
:param threshold 30
:param label "adult"
:format box
```

```cypher
MATCH (p:Person)
WHERE p.age >= $threshold
RETURN $label AS category, p.name, p.age
ORDER BY p.age DESC;
```

```text
:format json
MATCH (p:Person) RETURN p.id, p.name ORDER BY p.id;
:format auto
```

### Playbook D: logical save, destructive experiment, restore

```cypher
EXPORT DATABASE './before-experiment' (format='csv');
DROP TABLE Knows;
:schema
IMPORT DATABASE './before-experiment';
:schema
MATCH (a:Person)-[r:Knows]->(b:Person) RETURN a.name, r.since, b.name;
```

Remember that import replaces the whole database graph registry.

### Playbook E: source file and atomic result destination

```text
:format json
:output ./exploration.json replace
:read ./queries/explore.cypher
:output stdout
```

If any result write fails, the replacement destination is not published as a successful partial
file.

## 21. Common surprises

| Surprise | Explanation / action |
|---|---|
| Everything disappeared after exit | Product mode is in-memory. Use `EXPORT DATABASE` before exit. |
| `koko my.db` fails | There is no positional native database path. |
| Enter inserted a newline | The parser considers the buffer incomplete, or the cursor was not at the end. Use `Ctrl-J` only when forcing submission is intentional. |
| Enter submitted before my next clause | The current prefix was parse-complete. End it with `\`, use Alt-Enter/Escape-then-Enter, or paste the whole query. |
| A query shows only 20 rows | Human TTY display truncation. Use `:rows all`; redirected/batch output is complete. |
| `:rows all` did not speed the query | It changes display only, not execution. Add Cypher `LIMIT` to reduce work. |
| `:output file` refused | The path exists. Choose a new path or say `append`/`replace`. |
| CSV/TSV rejected a submission | It contained more than one row-producing result. Use JSON/JSONL or separate submissions. |
| `:quit` refused | A transaction is active. Commit, roll back, or use `:quit --rollback`. |
| `USE GRAPH` failed | Finish the active transaction before switching graphs. |
| `SET p.id = ...` failed | Primary keys are immutable; delete and reinsert. |
| Plain `DELETE` failed | The node still has edges. Delete relationships first or use `DETACH DELETE`. |
| Completion is absent | Check `:completion`, terminal capability, and `TERM`; `TERM=dumb` intentionally uses a simple editor. |
| Styling is absent | Check `NO_COLOR`, `--color`, `:highlight`, and TTY capability. |
| A parameter string failed to parse | Parameter input is JSON. Write `:param name "Ada"`, including JSON quotes. |
| `PROFILE` changed data | `PROFILE` executes its inner statement, including DDL/writes. Use `EXPLAIN` to validate without execution. |
| A config key was rejected | Only the explicit TOML key registry is accepted; typos are not ignored. |
| A persistence-related setting exists | Compatibility metadata does not activate deferred native durability. |

## 22. Fast cheat sheet

```text
# Session
:status
:graphs
:quit
:quit --rollback

# Metadata
:schema [graph[.object]]
:describe <graph[.object]>
:functions [pattern]

# Parameters
:param name <json>
:params
:params --values
:param clear name
:param clear all

# Display
:format auto|box|table|csv|tsv|json|jsonl|markdown|line|trash
:rows N|all|default
:width N|auto
:null literal|empty
:timing on|off
:progress auto|on|off

# Editor
:multiline on|off
# A trailing backslash is interactive-only and forces another physical line:
MATCH (p:Person) \
RETURN p;
:highlight auto|on|off
:completion on|off
:history show N
:history skip
:history on|off

# Files
:read <path> [--keep-going]
:output
:output stdout
:output <path> [append|replace]

# Transactions
BEGIN TRANSACTION;
BEGIN TRANSACTION READ ONLY;
COMMIT;
ROLLBACK;

# Graphs
CREATE GRAPH name;
CREATE GRAPH name ANY;
USE GRAPH name;
DROP GRAPH name;

# Plans
EXPLAIN <statement>;
PROFILE <statement>;

# Runtime
CALL threads=4;
CALL timeout=5000;
CALL current_setting('threads') RETURN *;

# Logical save/restore
EXPORT DATABASE './backup' (format='csv');
IMPORT DATABASE './backup';
```

## 23. Authoritative references

- Current product, work, limitations and intentional decisions: [`../ROADMAP.md`](../ROADMAP.md)
- Observable CLI behavior: [`CLI_UX.md`](CLI_UX.md)
- CLI boundaries and data flow: [`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md)
- Completed CLI landing evidence: [`CLI_PLAN.md`](CLI_PLAN.md)
- Chronological project evidence: [`PROGRESS.md`](PROGRESS.md)

Koko's implementation, current documents, and product regressions define supported Cypher
semantics. The upstream corpus is optional compatibility evidence when a change explicitly owns
that contract. For ordinary exploration, prefer this guide, live `:schema`/`:describe`/`:functions`,
and the first-party REPL rather than the historical C++ shell.
