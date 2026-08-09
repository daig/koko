//! Scalar macros: the registry type and the pre-binding AST→AST expansion.
//!
//! A macro is a parameterized expression template. At a call site (which parses to
//! [`Expr::Function`], indistinguishable from a real function call) the call's
//! arguments are substituted for the macro's parameters in a copy of its body, and
//! the call is replaced by that body. This mirrors the C++ `bindMacroExpression` +
//! `MacroParameterReplacer`, but done **before binding** so the binder and catalog
//! never see macros — only fully-expanded ASTs.
//!
//! The registry lives in the `koko` layer (not the catalog) because the body is
//! an `ast::Expr`, a `koko-parser` type the catalog crate may not depend on. It is
//! part of the transaction snapshot, so create/drop roll back with the txn.

use crate::{Error, Result};
use koko_parser::ast::{
    Expr, MatchClause, NodePattern, PatternElement, ProjectionItem, ReadingClause, RegularQuery,
    RelPattern, ReturnClause, Statement, UpdatingClause, WithClause,
};
use std::collections::HashMap;

/// A registered scalar macro (its parameters + body), keyed in the registry by the
/// **uppercased** name (macro/function names are case-insensitive).
#[derive(Clone)]
pub struct MacroDef {
    pub positional: Vec<String>,
    pub defaults: Vec<(String, Expr)>,
    pub body: Expr,
}

/// The macro registry: uppercased name → definition.
pub type MacroRegistry = HashMap<String, MacroDef>;

/// A self-referential macro would expand forever; cap the chain and error instead
/// of overflowing the stack. Real macro nesting in the corpus is shallow.
const MAX_MACRO_DEPTH: u32 = 64;

/// Return a copy of `stmt` with every macro call in it expanded. Only statements
/// that carry expressions which can call a macro are walked (queries, the inner
/// query of `CREATE … AS`, and the source query of `COPY (…) TO`); everything
/// else is returned unchanged.
pub fn expand_statement(stmt: &Statement, macros: &MacroRegistry) -> Result<Statement> {
    let mut stmt = stmt.clone();
    match &mut stmt {
        Statement::Query(rq) => expand_query(rq, macros, 0)?,
        Statement::CreateTableAs(c) => expand_query(&mut c.query, macros, 0)?,
        Statement::CopyTo(c) => expand_query(&mut c.query, macros, 0)?,
        _ => {}
    }
    Ok(stmt)
}

fn expand_query(rq: &mut RegularQuery, macros: &MacroRegistry, depth: u32) -> Result<()> {
    for sq in &mut rq.singles {
        for part in &mut sq.parts {
            expand_reading(&mut part.reading, macros, depth)?;
            expand_updating(&mut part.updating, macros, depth)?;
            expand_with(&mut part.with, macros, depth)?;
        }
        expand_reading(&mut sq.reading, macros, depth)?;
        expand_updating(&mut sq.updating, macros, depth)?;
        if let Some(ret) = &mut sq.ret {
            expand_return(ret, macros, depth)?;
        }
    }
    Ok(())
}

fn expand_reading(clauses: &mut [ReadingClause], macros: &MacroRegistry, depth: u32) -> Result<()> {
    for c in clauses {
        match c {
            ReadingClause::Match(MatchClause {
                patterns,
                where_clause,
                ..
            }) => {
                for p in patterns {
                    expand_pattern(p, macros, depth)?;
                }
                if let Some(w) = where_clause {
                    expand_expr(w, macros, depth)?;
                }
            }
            ReadingClause::Unwind(u) => expand_expr(&mut u.expr, macros, depth)?,
            ReadingClause::Call(call) => {
                for argument in &mut call.args {
                    expand_expr(argument, macros, depth)?;
                }
                if let Some(where_clause) = &mut call.where_clause {
                    expand_expr(where_clause, macros, depth)?;
                }
            }
            // `LOAD FROM`'s only macro-expandable expression is a trailing `WHERE`
            // (the path/options are literals).
            ReadingClause::LoadFrom(l) => {
                if let Some(w) = &mut l.where_clause {
                    expand_expr(w, macros, depth)?;
                }
            }
        }
    }
    Ok(())
}

fn expand_updating(
    clauses: &mut [UpdatingClause],
    macros: &MacroRegistry,
    depth: u32,
) -> Result<()> {
    for c in clauses {
        match c {
            UpdatingClause::Create(cc) => {
                for p in &mut cc.patterns {
                    expand_pattern(p, macros, depth)?;
                }
            }
            UpdatingClause::Set(sc) => {
                for it in &mut sc.items {
                    expand_expr(&mut it.value, macros, depth)?;
                }
            }
            UpdatingClause::Delete(dc) => {
                for e in &mut dc.exprs {
                    expand_expr(e, macros, depth)?;
                }
            }
            UpdatingClause::Merge(mc) => {
                for p in &mut mc.patterns {
                    expand_pattern(p, macros, depth)?;
                }
                for it in mc.on_create.iter_mut().chain(mc.on_match.iter_mut()) {
                    expand_expr(&mut it.value, macros, depth)?;
                }
            }
        }
    }
    Ok(())
}

fn expand_with(w: &mut WithClause, macros: &MacroRegistry, depth: u32) -> Result<()> {
    expand_return(&mut w.projection, macros, depth)?;
    if let Some(wc) = &mut w.where_clause {
        expand_expr(wc, macros, depth)?;
    }
    Ok(())
}

fn expand_return(r: &mut ReturnClause, macros: &MacroRegistry, depth: u32) -> Result<()> {
    for item in &mut r.items {
        if let ProjectionItem::Expr { expr, .. } = item {
            expand_expr(expr, macros, depth)?;
        }
    }
    for (e, _) in &mut r.order_by {
        expand_expr(e, macros, depth)?;
    }
    for e in r.skip.iter_mut().chain(r.limit.iter_mut()) {
        expand_expr(e, macros, depth)?;
    }
    Ok(())
}

fn expand_pattern(p: &mut PatternElement, macros: &MacroRegistry, depth: u32) -> Result<()> {
    expand_node(&mut p.head, macros, depth)?;
    for (rel, node) in &mut p.chains {
        expand_rel(rel, macros, depth)?;
        expand_node(node, macros, depth)?;
    }
    Ok(())
}

fn expand_node(n: &mut NodePattern, macros: &MacroRegistry, depth: u32) -> Result<()> {
    for (_, e) in &mut n.properties {
        expand_expr(e, macros, depth)?;
    }
    Ok(())
}

fn expand_rel(r: &mut RelPattern, macros: &MacroRegistry, depth: u32) -> Result<()> {
    for (_, e) in &mut r.properties {
        expand_expr(e, macros, depth)?;
    }
    if let Some(lam) = r.recursive.as_mut().and_then(|rec| rec.lambda.as_mut()) {
        if let Some(p) = &mut lam.predicate {
            expand_expr(p, macros, depth)?;
        }
        for proj in lam
            .rel_projection
            .iter_mut()
            .chain(lam.node_projection.iter_mut())
        {
            for e in proj {
                expand_expr(e, macros, depth)?;
            }
        }
    }
    Ok(())
}

/// Expand every macro call reachable from `e` (in place). Children are expanded
/// first (bottom-up), so a macro's arguments are fully expanded before they are
/// substituted into its body; then, if `e` itself is a macro call, it is replaced
/// by the substituted-and-recursively-expanded body.
fn expand_expr(e: &mut Expr, macros: &MacroRegistry, depth: u32) -> Result<()> {
    expand_children(e, macros, depth)?;
    if let Expr::Function { name, args, .. } = e {
        let upper = name.to_uppercase();
        if let Some(def) = macros.get(&upper) {
            if depth >= MAX_MACRO_DEPTH {
                return Err(Error::binder(format!(
                    "Macro {upper} expansion exceeded the maximum depth (recursive macro?)."
                )));
            }
            let mut body = build_macro_body(def, &upper, args)?;
            expand_expr(&mut body, macros, depth + 1)?;
            *e = body;
        }
    }
    Ok(())
}

/// Recurse `expand_expr` into every sub-expression of `e` *without* treating `e`
/// itself as a macro call. Exhaustive over `Expr` (no catch-all that would silently
/// drop children and leave a macro un-expanded).
fn expand_children(e: &mut Expr, macros: &MacroRegistry, depth: u32) -> Result<()> {
    match e {
        Expr::Function { args, .. } => {
            for a in args {
                expand_expr(a, macros, depth)?;
            }
        }
        Expr::Arithmetic { lhs, rhs, .. } | Expr::Comparison { lhs, rhs, .. } => {
            expand_expr(lhs, macros, depth)?;
            expand_expr(rhs, macros, depth)?;
        }
        Expr::And(v) | Expr::Or(v) | Expr::List(v) => {
            for x in v {
                expand_expr(x, macros, depth)?;
            }
        }
        Expr::Xor(a, b) => {
            expand_expr(a, macros, depth)?;
            expand_expr(b, macros, depth)?;
        }
        Expr::Not(a) | Expr::Negate(a) | Expr::IsNull(a) | Expr::IsNotNull(a) => {
            expand_expr(a, macros, depth)?;
        }
        Expr::Struct(fields) => {
            for (_, x) in fields {
                expand_expr(x, macros, depth)?;
            }
        }
        Expr::Lambda { body, .. } => expand_expr(body, macros, depth)?,
        Expr::PatternComprehension { projection, .. } => {
            if let Some(p) = projection {
                expand_expr(p, macros, depth)?;
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expand_expr(list, macros, depth)?;
            if let Some(p) = predicate {
                expand_expr(p, macros, depth)?;
            }
            if let Some(p) = projection {
                expand_expr(p, macros, depth)?;
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_,
        } => {
            if let Some(op) = operand {
                expand_expr(op, macros, depth)?;
            }
            for (w, t) in when_thens {
                expand_expr(w, macros, depth)?;
                expand_expr(t, macros, depth)?;
            }
            if let Some(el) = else_ {
                expand_expr(el, macros, depth)?;
            }
        }
        Expr::Subquery {
            patterns,
            where_clause,
            ..
        } => {
            for p in patterns {
                expand_pattern(p, macros, depth)?;
            }
            if let Some(w) = where_clause {
                expand_expr(w, macros, depth)?;
            }
        }
        Expr::Property { base, .. } => {
            expand_expr(base, macros, depth)?;
        }
        Expr::Literal(_)
        | Expr::OverflowInt(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::Star => {}
    }
    Ok(())
}

/// Build the substituted body for a macro call: validate arity, map each parameter
/// to its argument (positional first; then defaults, taking a provided argument if
/// present else the default expression), and replace the parameters in a body copy.
fn build_macro_body(def: &MacroDef, upper_name: &str, args: &[Expr]) -> Result<Expr> {
    let n_pos = def.positional.len();
    let n_total = n_pos + def.defaults.len();
    if args.len() < n_pos || args.len() > n_total {
        return Err(Error::binder(format!(
            "Invalid number of arguments for macro {upper_name}."
        )));
    }
    let mut map: HashMap<&str, &Expr> = HashMap::with_capacity(n_total);
    for (i, p) in def.positional.iter().enumerate() {
        map.insert(p.as_str(), &args[i]);
    }
    for (j, (pname, default_expr)) in def.defaults.iter().enumerate() {
        let idx = n_pos + j;
        let val = if idx < args.len() {
            &args[idx]
        } else {
            default_expr
        };
        map.insert(pname.as_str(), val);
    }
    let mut body = def.body.clone();
    substitute(&mut body, &map);
    Ok(body)
}

/// Replace each `Expr::Variable(param)` in `e` with the corresponding argument,
/// including variables nested inside a property base expression.
fn substitute(e: &mut Expr, map: &HashMap<&str, &Expr>) {
    match e {
        Expr::Variable(s) => {
            if let Some(rep) = map.get(s.as_str()) {
                *e = (*rep).clone();
            }
        }
        Expr::Property { base, .. } => substitute(base, map),
        Expr::Function { args, .. } => {
            for a in args {
                substitute(a, map);
            }
        }
        Expr::Arithmetic { lhs, rhs, .. } | Expr::Comparison { lhs, rhs, .. } => {
            substitute(lhs, map);
            substitute(rhs, map);
        }
        Expr::And(v) | Expr::Or(v) | Expr::List(v) => {
            for x in v {
                substitute(x, map);
            }
        }
        Expr::Xor(a, b) => {
            substitute(a, map);
            substitute(b, map);
        }
        Expr::Not(a) | Expr::Negate(a) | Expr::IsNull(a) | Expr::IsNotNull(a) => substitute(a, map),
        Expr::Struct(fields) => {
            for (_, x) in fields {
                substitute(x, map);
            }
        }
        Expr::Lambda { body, .. } => substitute(body, map),
        Expr::PatternComprehension { projection, .. } => {
            if let Some(p) = projection {
                substitute(p, map);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            substitute(list, map);
            if let Some(p) = predicate {
                substitute(p, map);
            }
            if let Some(p) = projection {
                substitute(p, map);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_,
        } => {
            if let Some(op) = operand {
                substitute(op, map);
            }
            for (w, t) in when_thens {
                substitute(w, map);
                substitute(t, map);
            }
            if let Some(el) = else_ {
                substitute(el, map);
            }
        }
        Expr::Subquery { where_clause, .. } => {
            // Pattern sub-expressions of a macro-body subquery aren't substituted
            // (extraordinarily niche; untested). The WHERE predicate is.
            if let Some(w) = where_clause {
                substitute(w, map);
            }
        }
        Expr::Literal(_) | Expr::OverflowInt(_) | Expr::Parameter(_) | Expr::Star => {}
    }
}

/// Build an owned parameter array from `name => value` pairs.
///
/// Values convert through [`crate::Value`]'s `From` implementations, so bare
/// Rust scalars work:
///
/// ```
/// use koko::{Database, params};
/// # fn main() -> koko::Result<()> {
/// let database = Database::new();
/// let connection = database.connect();
/// connection.execute("CREATE NODE TABLE P(name STRING, age INT64, PRIMARY KEY(name))")?;
/// let mut statement =
///     connection.prepare("MATCH (p:P) WHERE p.age >= $min RETURN p.name")?;
/// let result = statement.execute_with(params! { "min" => 30 })?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[macro_export]
macro_rules! params {
    () => {
        [] as [$crate::Parameter; 0]
    };
    ($($name:expr => $value:expr),+ $(,)?) => {
        [$($crate::Parameter::new($name, $value)),+]
    };
}
