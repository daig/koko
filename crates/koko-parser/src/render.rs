//! Canonical Cypher rendering of an [`Expr`] AST back to source text.
//!
//! The implementation originated from the reference front-end's `rawName`
//! rendering rules. Two behaviors remain important:
//!
//! * **Operator expressions** (arithmetic, boolean, `IS NULL`, and peers) are
//!   rebuilt with single spaces around the operator and drop redundant
//!   parentheses because the parse tree has no parenthesis node.
//! * **Atoms** (variables, literals, `CASE`, function calls, and list literals)
//!   are rendered recursively with canonical single spacing.
//!
//! The `show_macros` surface has byte-exact regressions for variables, integer
//! literals, `+`/`-` arithmetic, and simple `CASE` with or without `ELSE`.
//! Other constructs use the same best-effort canonical style; this helper is
//! not a lossless source-code serializer.

use crate::ast::{ArithOp, CmpOp, Expr};
use koko_common::Value;

/// Render `e` back to canonical Cypher source text.
pub fn expr_to_cypher(e: &Expr) -> String {
    match e {
        Expr::OverflowInt(text) => text.clone(),
        Expr::Literal(v) => literal_to_cypher(v),
        Expr::Variable(s) => s.clone(),
        Expr::Property { base, name } => format!("{}.{}", expr_to_cypher(base), name),
        Expr::Parameter(s) => format!("${s}"),
        Expr::Function {
            name,
            distinct,
            args,
            arg_names,
        } => {
            let inner: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, a)| match arg_names.get(i).and_then(|o| o.as_deref()) {
                    Some(nm) => format!("{nm} := {}", expr_to_cypher(a)),
                    None => expr_to_cypher(a),
                })
                .collect();
            let prefix = if *distinct { "DISTINCT " } else { "" };
            format!("{name}({prefix}{})", inner.join(", "))
        }
        Expr::And(items) => join_binary(items, " AND "),
        Expr::Or(items) => join_binary(items, " OR "),
        Expr::Xor(a, b) => format!("{} XOR {}", expr_to_cypher(a), expr_to_cypher(b)),
        Expr::Not(a) => format!("NOT {}", expr_to_cypher(a)),
        Expr::Comparison { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                expr_to_cypher(lhs),
                cmp_op_str(*op),
                expr_to_cypher(rhs)
            )
        }
        Expr::Arithmetic { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                expr_to_cypher(lhs),
                arith_op_str(*op),
                expr_to_cypher(rhs)
            )
        }
        Expr::Negate(a) => format!("-{}", expr_to_cypher(a)),
        Expr::IsNull(a) => format!("{} IS NULL", expr_to_cypher(a)),
        Expr::IsNotNull(a) => format!("{} IS NOT NULL", expr_to_cypher(a)),
        Expr::List(items) => {
            let inner: Vec<String> = items.iter().map(expr_to_cypher).collect();
            format!("[{}]", inner.join(","))
        }
        Expr::Struct(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("{k}: {}", expr_to_cypher(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Expr::Lambda { params, body } => {
            let head = if params.len() == 1 {
                params[0].clone()
            } else {
                format!("({})", params.join(", "))
            };
            format!("{head} -> {}", expr_to_cypher(body))
        }
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            let mut s = format!("[{var} IN {}", expr_to_cypher(list));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE {}", expr_to_cypher(p)));
            }
            if let Some(p) = projection {
                s.push_str(&format!(" | {}", expr_to_cypher(p)));
            }
            s.push(']');
            s
        }
        Expr::PatternComprehension { projection, .. } => match projection {
            Some(p) => format!("[(…) | {}]", expr_to_cypher(p)),
            None => "[(…)]".to_string(),
        },
        Expr::Case {
            operand,
            when_thens,
            else_,
        } => {
            let mut s = String::from("CASE");
            if let Some(op) = operand {
                s.push(' ');
                s.push_str(&expr_to_cypher(op));
            }
            for (w, t) in when_thens {
                s.push_str(" WHEN ");
                s.push_str(&expr_to_cypher(w));
                s.push_str(" THEN ");
                s.push_str(&expr_to_cypher(t));
            }
            if let Some(e) = else_ {
                s.push_str(" ELSE ");
                s.push_str(&expr_to_cypher(e));
            }
            s.push_str(" END");
            s
        }
        Expr::Star => "*".to_string(),
        // Subqueries don't appear in macro bodies in the corpus; a minimal,
        // non-panicking rendering (the pattern detail isn't reconstructed).
        Expr::Subquery { kind, .. } => match kind {
            crate::ast::SubqueryKind::Exists => "EXISTS { ... }".to_string(),
            crate::ast::SubqueryKind::Count => "COUNT { ... }".to_string(),
        },
    }
}

fn join_binary(items: &[Expr], sep: &str) -> String {
    items
        .iter()
        .map(expr_to_cypher)
        .collect::<Vec<_>>()
        .join(sep)
}

/// Render a literal as it would appear in Cypher source. Integers (the only
/// `show_macros`-tested literal) render as their digits; bools/strings/null use
/// Cypher source syntax; other types fall back to the result formatting.
fn literal_to_cypher(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::String(s) => format!("'{s}'"),
        // Source-style float text (audit R5): TABLE_INFO shows `5.4`, not the
        // folded 6-decimal rendering `5.400000`.
        Value::Double(x) => format!("{x}"),
        Value::Float(x) => format!("{x}"),
        other => other.to_result_string(),
    }
}

fn arith_op_str(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
    }
}

fn cmp_op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}
