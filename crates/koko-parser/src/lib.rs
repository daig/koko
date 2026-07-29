//! `koko-parser` — Koko's hand-written Cypher lexer, recursive-descent statement
//! parser, and Pratt expression parser. It produces an owned [`ast`] without a
//! generated-parser build step.

pub mod ast;
pub mod lexer;
mod parser;
pub mod render;
pub mod tooling;

pub use ast::*;
pub use parser::parse_statement;
pub use render::expr_to_cypher;

#[cfg(test)]
mod tests {
    use super::ast::*;
    use super::parse_statement;
    use koko_common::Value;

    /// Extract the single (non-`UNION`) query from a parsed statement.
    fn single(s: Statement) -> SingleQuery {
        match s {
            Statement::Query(mut rq) => {
                assert_eq!(rq.singles.len(), 1, "expected a non-UNION query");
                rq.singles.remove(0)
            }
            _ => panic!("expected a query statement"),
        }
    }

    #[test]
    fn parse_create_node_table() {
        let s =
            parse_statement("CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))")
                .unwrap();
        match s {
            Statement::CreateNodeTable(t) => {
                assert_eq!(t.name, "Person");
                assert_eq!(t.columns.len(), 2);
                assert_eq!(t.columns[1].name, "age");
                assert_eq!(t.columns[1].type_name, "INT64");
                assert_eq!(t.primary_key, "name");
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parse_create_rel_table_with_multiplicity() {
        let s = parse_statement(
            "CREATE REL TABLE Knows(FROM Person TO Person, since INT64, MANY_MANY)",
        )
        .unwrap();
        match s {
            Statement::CreateRelTable(t) => {
                assert_eq!(t.name, "Knows");
                assert_eq!(t.pairs, vec![("Person".to_string(), "Person".to_string())]);
                assert_eq!(t.columns.len(), 1);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parse_create_rel_table_group() {
        // `GROUP` is an optional alias keyword: `CREATE REL TABLE GROUP <name>(…)`
        // parses identically to a multi-pair `CREATE REL TABLE` (the C++
        // `transformCreateRelGroup` ignores the keyword).
        let check_two_pairs = |sql: &str| match parse_statement(sql).unwrap() {
            Statement::CreateRelTable(t) => {
                assert_eq!(t.name, "knows");
                assert_eq!(
                    t.pairs,
                    vec![
                        ("person".to_string(), "person".to_string()),
                        ("person".to_string(), "person1".to_string()),
                    ]
                );
                assert_eq!(t.columns.len(), 1);
                assert_eq!(t.columns[0].name, "age");
                assert_eq!(t.columns[0].type_name, "INT64");
            }
            _ => panic!("wrong statement"),
        };
        check_two_pairs(
            "CREATE REL TABLE GROUP knows(FROM person TO person, FROM person TO person1, age INT64)",
        );
        // Lowercase `group` is accepted too (keyword match is case-insensitive).
        check_two_pairs(
            "CREATE REL TABLE group knows(FROM person TO person, FROM person TO person1, age INT64)",
        );

        // `GROUP IF NOT EXISTS <name>` flows through the same path.
        match parse_statement(
            "CREATE REL TABLE GROUP IF NOT EXISTS knows(FROM person TO person, FROM person TO person1, age INT64)",
        )
        .unwrap()
        {
            Statement::CreateRelTable(t) => {
                assert_eq!(t.name, "knows");
                assert_eq!(t.pairs.len(), 2);
                assert!(t.if_not_exists);
            }
            _ => panic!("wrong statement"),
        }

        // A `GROUP … AS <query>` CTAS form flows through the same path.
        match parse_statement(
            "CREATE REL TABLE GROUP knows(FROM person TO person) AS MATCH (a)-[e]->(b) RETURN a, b",
        )
        .unwrap()
        {
            Statement::CreateTableAs(t) => {
                assert_eq!(t.name, "knows");
                assert!(!t.is_node);
                assert_eq!(t.pairs, vec![("person".to_string(), "person".to_string())]);
            }
            _ => panic!("expected a CTAS statement"),
        }
    }

    #[test]
    fn parse_create_macro_with_defaults() {
        let s = parse_statement("CREATE MACRO addDefault(x, y := 40, z:=7) AS x + y + z").unwrap();
        match s {
            Statement::CreateMacro(m) => {
                assert_eq!(m.name, "addDefault");
                assert_eq!(m.positional, vec!["x".to_string()]);
                assert_eq!(m.defaults.len(), 2);
                assert_eq!(m.defaults[0].0, "y");
                assert_eq!(m.defaults[0].1, Expr::Literal(Value::Int64(40)));
                assert_eq!(m.defaults[1].0, "z");
                // The body renders back to canonical Cypher (parens dropped, spaced).
                assert_eq!(super::expr_to_cypher(&m.body), "x + y + z");
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn render_macro_case_body() {
        // `(a+b)` → `a + b`; CASE renders with canonical single spacing.
        let s = parse_statement("CREATE MACRO m(a, b) AS (a+b)").unwrap();
        let Statement::CreateMacro(m) = s else {
            panic!("wrong statement")
        };
        assert_eq!(super::expr_to_cypher(&m.body), "a + b");

        let s = parse_statement("CREATE MACRO c(x) AS CASE x WHEN 35 THEN x + 1 ELSE x - 5 END")
            .unwrap();
        let Statement::CreateMacro(m) = s else {
            panic!("wrong statement")
        };
        assert_eq!(
            super::expr_to_cypher(&m.body),
            "CASE x WHEN 35 THEN x + 1 ELSE x - 5 END"
        );
    }

    #[test]
    fn parse_drop_macro() {
        match parse_statement("DROP MACRO add2").unwrap() {
            Statement::DropMacro { name, if_exists } => {
                assert_eq!(name, "add2");
                assert!(!if_exists);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parse_match_where_return() {
        let s = parse_statement(
            "MATCH (a:Person)-[r:Knows]->(b:Person) WHERE a.age >= 30 AND b.name = 'Bob' \
             RETURN a.name, b.age + 1 AS x ORDER BY a.name DESC LIMIT 10",
        )
        .unwrap();
        let q = single(s);
        assert_eq!(q.reading.len(), 1);
        let m = match &q.reading[0] {
            ReadingClause::Match(m) => m,
            _ => panic!("expected MATCH"),
        };
        assert_eq!(m.patterns.len(), 1);
        let pe = &m.patterns[0];
        assert_eq!(pe.head.var.as_deref(), Some("a"));
        assert_eq!(pe.chains.len(), 1);
        assert_eq!(pe.chains[0].0.direction, Direction::Right);
        assert!(m.where_clause.is_some());
        let r = q.ret.unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.order_by.len(), 1);
        assert!(!r.order_by[0].1); // DESC
        assert_eq!(r.limit, Some(Expr::Literal(Value::Int64(10))));
    }

    #[test]
    fn parse_match_join_hint_parse_and_ignore() {
        // A `HINT` after the optional WHERE is parsed and discarded (planner-only
        // join order): one MATCH, WHERE present, RETURN with 3 items.
        let q = single(
            parse_statement(
                "MATCH (a:person)-[e:knows]->(b:person) WHERE a.ID > 6 \
                 HINT a JOIN (e JOIN b) RETURN a.ID, b.ID, ID(e)",
            )
            .unwrap(),
        );
        assert_eq!(q.reading.len(), 1);
        let ReadingClause::Match(m) = &q.reading[0] else {
            panic!("expected MATCH");
        };
        assert!(m.where_clause.is_some());
        assert_eq!(q.ret.unwrap().items.len(), 3);

        // HINT with no preceding WHERE; `(a JOIN e) JOIN b`.
        let q = single(
            parse_statement(
                "MATCH (a:person)-[e:knows]->(b:person) HINT (a JOIN e) JOIN b RETURN COUNT(*)",
            )
            .unwrap(),
        );
        let ReadingClause::Match(m) = &q.reading[0] else {
            panic!("expected MATCH");
        };
        assert!(m.where_clause.is_none());
        assert!(q.ret.is_some());

        // A nested MULTI_JOIN form (immune to JOIN-associativity ambiguity).
        let q = single(
            parse_statement(
                "MATCH (a:person)-[e:knows]->(b:person) \
                 HINT (((a JOIN e) JOIN b) MULTI_JOIN e2 MULTI_JOIN e3) JOIN c RETURN a",
            )
            .unwrap(),
        );
        assert!(matches!(&q.reading[0], ReadingClause::Match(_)));
        assert!(q.ret.is_some());

        // Guard: a bare single-atom hint terminates the loop cleanly at RETURN.
        let q = single(parse_statement("MATCH (a:person) HINT a RETURN a.ID").unwrap());
        assert!(matches!(&q.reading[0], ReadingClause::Match(_)));
        assert_eq!(q.ret.unwrap().items.len(), 1);
    }

    /// Parse one MATCH pattern's first rel from `query` (a `MATCH … RETURN …`).
    fn first_rel(query: &str) -> (Option<String>, RelPattern) {
        let q = single(parse_statement(query).unwrap());
        let m = match &q.reading[0] {
            ReadingClause::Match(m) => m,
            _ => panic!("expected MATCH"),
        };
        let pe = &m.patterns[0];
        (pe.name.clone(), pe.chains[0].0.clone())
    }

    #[test]
    fn parse_var_length_bounds() {
        let b = |q: &str| first_rel(q).1.recursive.unwrap().bounds;
        assert_eq!(b("MATCH (a)-[e*]->(b) RETURN a"), (None, None));
        assert_eq!(b("MATCH (a)-[e*..]->(b) RETURN a"), (None, None));
        assert_eq!(b("MATCH (a)-[e*3]->(b) RETURN a"), (Some(3), Some(3)));
        assert_eq!(b("MATCH (a)-[e*2..]->(b) RETURN a"), (Some(2), None));
        assert_eq!(b("MATCH (a)-[e*..4]->(b) RETURN a"), (None, Some(4)));
        assert_eq!(b("MATCH (a)-[e*1..3]->(b) RETURN a"), (Some(1), Some(3)));
        assert_eq!(b("MATCH (a)-[e*0..0]->(b) RETURN a"), (Some(0), Some(0)));
        // A plain single hop has no recursive info.
        assert!(
            first_rel("MATCH (a)-[e]->(b) RETURN a")
                .1
                .recursive
                .is_none()
        );
    }

    #[test]
    fn parse_recursive_mode_and_semantic() {
        let r = |q: &str| first_rel(q).1.recursive.unwrap();
        assert_eq!(
            r("MATCH (a)-[e* SHORTEST 1..5]->(b) RETURN a").mode,
            RecursiveMode::Shortest
        );
        assert_eq!(
            r("MATCH (a)-[e* ALL SHORTEST 1..5]->(b) RETURN a").mode,
            RecursiveMode::AllShortest
        );
        assert_eq!(
            r("MATCH (a)-[e* TRAIL 1..3]->(b) RETURN a").semantic,
            PathSemantic::Trail
        );
        assert_eq!(
            r("MATCH (a)-[e* ACYCLIC 1..3]->(b) RETURN a").semantic,
            PathSemantic::Acyclic
        );
    }

    #[test]
    fn parse_named_path_and_lambda() {
        let (name, rel) = first_rel(
            "MATCH p = (a)-[e:knows*1..2 (r, n | WHERE n.ID < 3 | {r.date}, {n.fName})]->(b) RETURN p",
        );
        assert_eq!(name.as_deref(), Some("p"));
        let lambda = rel.recursive.unwrap().lambda.unwrap();
        assert_eq!(lambda.rel_var, "r");
        assert_eq!(lambda.node_var, "n");
        assert!(lambda.predicate.is_some());
        assert_eq!(lambda.rel_projection.unwrap().len(), 1);
        assert_eq!(lambda.node_projection.unwrap().len(), 1);

        // Filter-only and projection-only forms.
        let f = first_rel("MATCH (a)-[e:knows*1..2 (r, _ | WHERE r.x > 0)]->(b) RETURN a").1;
        let fl = f.recursive.unwrap().lambda.unwrap();
        assert!(fl.predicate.is_some());
        assert!(fl.rel_projection.is_none());
        let p = first_rel("MATCH (a)-[e:meets*1..2 (r, n | {r.times}, {})]->(b) RETURN a").1;
        let pl = p.recursive.unwrap().lambda.unwrap();
        assert!(pl.predicate.is_none());
        assert_eq!(pl.node_projection.unwrap().len(), 0);
    }

    #[test]
    fn parse_set_and_delete_clauses() {
        let q = single(parse_statement("MATCH (a:P) WHERE a.id=1 SET a.x = 5, a.y = 'z'").unwrap());
        match &q.updating[0] {
            UpdatingClause::Set(s) => {
                assert_eq!(s.items.len(), 2);
                assert!(
                    matches!(&s.items[0].target, SetTarget::Property { name, .. } if name == "x")
                );
            }
            _ => panic!("expected SET"),
        }
        // `SET a = e` (whole-value); the Neo4j-ism `+=` is a parse error (C++
        // has no `+=` — ledger "set-plus-equals").
        let q = single(parse_statement("MATCH (a:P) SET a = b").unwrap());
        assert!(matches!(&q.updating[0], UpdatingClause::Set(s)
            if matches!(s.items[0].target, SetTarget::Var(_))));
        assert!(parse_statement("MATCH (a:P) SET a += {x: 1}").is_err());

        // DELETE and DETACH DELETE (multiple targets); write-then-RETURN.
        let q = single(parse_statement("MATCH (a)-[e]->(b) DELETE e RETURN b.id").unwrap());
        assert!(
            matches!(&q.updating[0], UpdatingClause::Delete(d) if !d.detach && d.exprs.len() == 1)
        );
        assert!(q.ret.is_some());
        let q = single(parse_statement("MATCH (a) DETACH DELETE a, a").unwrap());
        assert!(
            matches!(&q.updating[0], UpdatingClause::Delete(d) if d.detach && d.exprs.len() == 2)
        );

        // Write before WITH: an updating clause in a non-terminal part.
        let q = single(
            parse_statement("MATCH (a) CREATE (:P {id: a.id}) WITH a MATCH (b) RETURN b").unwrap(),
        );
        assert_eq!(q.parts.len(), 1);
        assert!(matches!(&q.parts[0].updating[0], UpdatingClause::Create(_)));
    }

    #[test]
    fn parse_merge_clause() {
        // Single-node MERGE with ON CREATE / ON MATCH SET.
        let q = single(
            parse_statement(
                "MERGE (a:person {ID: 1}) ON CREATE SET a.age = 1 ON MATCH SET a.age = 2 RETURN a.ID",
            )
            .unwrap(),
        );
        match &q.updating[0] {
            UpdatingClause::Merge(m) => {
                assert_eq!(m.patterns.len(), 1);
                assert_eq!(m.patterns[0].head.var.as_deref(), Some("a"));
                assert_eq!(m.on_create.len(), 1);
                assert_eq!(m.on_match.len(), 1);
            }
            _ => panic!("expected MERGE"),
        }
        assert!(q.ret.is_some());

        // Rel MERGE (no SET); and chained MERGEs after a MATCH.
        let q = single(
            parse_statement(
                "MATCH (a),(b) MERGE (a)-[r:knows {since: 2020}]->(b) MERGE (a)-[:likes]->(b)",
            )
            .unwrap(),
        );
        assert_eq!(q.updating.len(), 2);
        assert!(matches!(&q.updating[0], UpdatingClause::Merge(m)
            if m.patterns[0].chains.len() == 1 && m.on_create.is_empty()));
    }

    #[test]
    fn parse_transaction_and_call() {
        use Statement::*;
        assert!(matches!(
            parse_statement("BEGIN TRANSACTION").unwrap(),
            Transaction(TxnOp::Begin { read_only: false })
        ));
        assert!(matches!(
            parse_statement("BEGIN TRANSACTION READ ONLY").unwrap(),
            Transaction(TxnOp::Begin { read_only: true })
        ));
        assert!(matches!(
            parse_statement("COMMIT").unwrap(),
            Transaction(TxnOp::Commit)
        ));
        assert!(matches!(
            parse_statement("ROLLBACK").unwrap(),
            Transaction(TxnOp::Rollback)
        ));
        assert!(matches!(
            parse_statement("CHECKPOINT").unwrap(),
            Transaction(TxnOp::Checkpoint)
        ));
        match parse_statement("CALL var_length_extend_max_depth=10").unwrap() {
            Call(CallStmt::SetConfig { key, value }) => {
                assert_eq!(key, "var_length_extend_max_depth");
                assert_eq!(value, Expr::Literal(Value::Int64(10)));
            }
            _ => panic!("expected CALL SetConfig"),
        }
        assert!(matches!(
            parse_statement("CALL auto_checkpoint=false").unwrap(),
            Call(CallStmt::SetConfig { .. })
        ));
        // `current_setting` is now a table function; a standalone `RETURN *`
        // form parses as a `CallStmt::TableFunc` with the setting key as its arg.
        match parse_statement("CALL current_setting('timeout') RETURN *").unwrap() {
            Call(CallStmt::TableFunc {
                arg, has_return, ..
            }) => {
                assert_eq!(arg.as_deref(), Some("timeout"));
                assert!(has_return);
            }
            _ => panic!("expected current_setting table function"),
        }
    }

    #[test]
    fn parse_create_pattern_and_aggregates() {
        let s = parse_statement("CREATE (:Person {name: 'Alice', age: 35})").unwrap();
        assert!(matches!(s, Statement::Query(_)));

        let s =
            parse_statement("MATCH (a:Person) RETURN count(*), sum(a.age), count(DISTINCT a.age)")
                .unwrap();
        let q = single(s);
        let items = q.ret.unwrap().items;
        assert_eq!(items.len(), 3);
        match &items[0] {
            ProjectionItem::Expr {
                expr: Expr::Function { name, args, .. },
                ..
            } => {
                assert_eq!(name, "count");
                assert_eq!(args[0], Expr::Star);
            }
            _ => panic!("expected count(*)"),
        }
    }

    #[test]
    fn parse_list_comprehension_and_parenthesized_membership() {
        let query = single(parse_statement("RETURN [x IN [1,2,3] WHERE x > 1 | x * 10]").unwrap());
        let ProjectionItem::Expr { expr, .. } = &query.ret.unwrap().items[0] else {
            panic!("expected expression projection");
        };
        let Expr::ListComprehension {
            var,
            predicate,
            projection,
            ..
        } = expr
        else {
            panic!("expected list comprehension");
        };
        assert_eq!(var, "x");
        assert!(predicate.is_some());
        assert!(projection.is_some());
        assert_eq!(
            super::expr_to_cypher(expr),
            "[x IN [1,2,3] WHERE x > 1 | x * 10]"
        );

        let query = single(parse_statement("RETURN [(x IN [1,2])]").unwrap());
        let ProjectionItem::Expr { expr, .. } = &query.ret.unwrap().items[0] else {
            panic!("expected expression projection");
        };
        assert!(matches!(expr, Expr::List(_)));
    }

    #[test]
    fn parse_undirected_and_reverse_rels() {
        let right = parse_statement("MATCH (a)-[r]->(b) RETURN a").unwrap();
        let left = parse_statement("MATCH (a)<-[r]-(b) RETURN a").unwrap();
        let both = parse_statement("MATCH (a)-[r]-(b) RETURN a").unwrap();
        for (s, dir) in [
            (right, Direction::Right),
            (left, Direction::Left),
            (both, Direction::Both),
        ] {
            let q = single(s);
            let ReadingClause::Match(m) = &q.reading[0] else {
                panic!("expected MATCH");
            };
            assert_eq!(m.patterns[0].chains[0].0.direction, dir);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_statement("MATCH (a:Person RETURN a").is_err());
        assert!(parse_statement("RETURN").is_err());
    }

    #[test]
    fn parse_with_splits_parts() {
        // Two WITH-terminated parts + a final RETURN part.
        let s = parse_statement(
            "MATCH (a:Person) WITH a.age AS age, count(*) AS c \
             WITH age AS age ORDER BY age SKIP 1 LIMIT 2 WHERE age > 30 \
             RETURN age",
        )
        .unwrap();
        let q = single(s);
        assert_eq!(q.parts.len(), 2);
        // Part 0: MATCH … WITH age, c.
        assert_eq!(q.parts[0].reading.len(), 1);
        assert_eq!(q.parts[0].with.projection.items.len(), 2);
        assert!(q.parts[0].with.where_clause.is_none());
        // Part 1: WITH … ORDER BY … SKIP/LIMIT … WHERE.
        assert!(q.parts[1].reading.is_empty());
        let w = &q.parts[1].with;
        assert_eq!(w.projection.order_by.len(), 1);
        assert_eq!(w.projection.skip, Some(Expr::Literal(Value::Int64(1))));
        assert_eq!(w.projection.limit, Some(Expr::Literal(Value::Int64(2))));
        assert!(w.where_clause.is_some());
        // Final part: RETURN age (no leading reading clauses).
        assert!(q.reading.is_empty());
        assert!(q.ret.is_some());
    }

    #[test]
    fn parse_query_starting_with_with() {
        // A query may start with WITH (no leading MATCH).
        let s = parse_statement("WITH [1,2,3] AS xs UNWIND xs AS x RETURN x").unwrap();
        let q = single(s);
        assert_eq!(q.parts.len(), 1);
        assert!(q.parts[0].reading.is_empty());
        // The UNWIND lands in the final part (after the WITH).
        assert_eq!(q.reading.len(), 1);
        assert!(matches!(q.reading[0], ReadingClause::Unwind(_)));
    }

    #[test]
    fn parse_union() {
        // A UNION ALL B UNION C — three operands, per-boundary flags.
        let s = parse_statement(
            "MATCH (a:Person) RETURN a.age \
             UNION ALL MATCH (b:Person) RETURN b.age \
             UNION MATCH (c:Person) RETURN c.age",
        )
        .unwrap();
        let rq = match s {
            Statement::Query(rq) => rq,
            _ => panic!("expected a query"),
        };
        assert_eq!(rq.singles.len(), 3);
        assert_eq!(rq.union_all, vec![true, false]); // UNION ALL, then UNION
        // Each operand is its own single query with a RETURN.
        assert!(rq.singles.iter().all(|s| s.ret.is_some()));

        // A plain single query: one operand, no boundaries.
        let s = parse_statement("MATCH (a:Person) RETURN a").unwrap();
        let rq = match s {
            Statement::Query(rq) => rq,
            _ => panic!(),
        };
        assert_eq!(rq.singles.len(), 1);
        assert!(rq.union_all.is_empty());
    }
}
