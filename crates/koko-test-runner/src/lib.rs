//! `koko-test-runner` — a parser and runner for the Koko `.test` corpus
//! format, used as the differential oracle.
//!
//! P0 supports the subset of the format the bespoke `CREATE`-based fixtures
//! need: a `-DATASET CSV empty` header, `-CASE`, `-LOG`, `-STATEMENT` (with
//! continuation lines), the `---- N` / `---- ok` / `---- error` result blocks,
//! and the `-CHECK_ORDER` / `-CHECK_PRECISION` flags. Default comparison sorts
//! both sides lexicographically (matching the C++ runner); `-CHECK_ORDER`
//! compares in order. Files whose dataset is not `empty` (i.e. those needing the
//! P1 CSV loader) are reported as skipped rather than failed.

mod md5;

use koko::{Connection, Database, DatabaseConfig, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

/// What a statement's result block asserts.
#[derive(Debug, Clone, PartialEq)]
pub enum Expected {
    /// `---- ok`: the statement must succeed (output ignored).
    Ok,
    /// `---- N`: the statement must return exactly these rendered rows.
    Rows(Vec<String>),
    /// `---- N` followed by `<FILE>:name` — the expected rows live in the answer
    /// file `name` (resolved against the runner's answers dir at compare time;
    /// used by the corpus for large results).
    RowsFile(String),
    /// `---- error`: the statement must fail with this message.
    Error(String),
    /// `---- error(regex)`: the statement must fail with a message fully matching
    /// this regex (C++ `std::regex_match` — anchored).
    ErrorRegex(String),
    /// `---- hash` + `N tuples hashed to <md5>`: the statement must return `count`
    /// rows whose MD5 (each row + `\n`, sorted unless `-CHECK_ORDER`) is `md5`.
    Hash { count: usize, md5: String },
    /// A `;`-packed statement beyond its block count: it runs, but its outcome is
    /// not compared (the C++ runner clamps to `min(statements, blocks)`).
    Unchecked,
}

/// A `-MULTI_COPY_RANDOM <splits> <table> [SEED s0 s1] "<source>"` action: the
/// C++ runner splits the source CSV's rows into `splits` random slices and runs
/// one `COPY <table> FROM <slice>` per slice. The split points only exercise
/// storage batch boundaries — the resulting logical DB state is identical for
/// any split — so this runner uses even slices and ignores the seed.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCopySpec {
    pub splits: usize,
    pub table: String,
    /// Source path; may contain `${KOKO_ROOT_DIRECTORY}` (expanded at run time).
    pub source: String,
}

/// Harness-side dataset setup that intentionally runs inside the current
/// connection/transaction rather than using the normal bulk bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatasetAction {
    CreateSchema(String),
    InsertByRow(String),
}

/// A single `-STATEMENT` and its expectation.
#[derive(Debug, Clone)]
pub struct TestStatement {
    pub query: String,
    pub check_order: bool,
    pub check_precision: bool,
    pub check_column_names: bool,
    /// Cypher `$name` parameter values in scope (from preceding `-PARAMETER`).
    pub params: Vec<(String, Value)>,
    pub expected: Expected,
    /// The named connection this statement routes to (from a `[conn1] …` prefix),
    /// or `None` for the case's default connection.
    pub conn_name: Option<String>,
    /// The preceding `-LOG <label>` (e.g. `q1`), used to label per-statement timing
    /// in the perf gate (`KOKO_TIMING=1`); `None` if no `-LOG`.
    pub label: Option<String>,
    /// The source `-STATEMENT` line's index when that line held MULTIPLE
    /// `;`-split statements — an engine error in the group aborts the rest of
    /// it (the C++ multi-statement chain breaks at the first failure).
    pub line_group: Option<usize>,
    /// Set for a synthetic `-MULTI_COPY_RANDOM` action (query is empty).
    pub multi_copy: Option<MultiCopySpec>,
    /// A `-BATCH_STATEMENTS` file under `test/statements`, executed one line at a time.
    pub batch_file: Option<String>,
    /// Manual schema or row-wise dataset setup directive.
    pub dataset_action: Option<DatasetAction>,
    /// Statements in one `BEGIN/END_CONCURRENT_EXECUTION` block share this id.
    pub concurrent_group: Option<usize>,
    /// Synthetic `-IMPORT_DATABASE <path>` harness action: replace the current
    /// in-memory database before the following `IMPORT DATABASE` statement.
    pub reset_database: bool,
    /// Synthetic `-REMOVE_FILE <path>` harness action.
    pub remove_file: Option<String>,
}

/// Parse a `-PARAMETER name=value` value: a quoted string, `true`/`false`, an
/// integer, a float, else a bare string.
fn parse_param_value(s: &str) -> Value {
    let t = s.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        return Value::String(t[1..t.len() - 1].to_string());
    }
    if t.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int64(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Double(f);
    }
    Value::String(t.to_string())
}

/// A `-CASE` block.
#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub skip: bool,
    /// Why the case is skipped (shown in the runner's `SKIP` line), e.g.
    /// `-SKIP_IN_MEM` or `uses -LOOP`.
    pub skip_reason: Option<String>,
    pub statements: Vec<TestStatement>,
}

/// A parsed `.test` file.
#[derive(Debug, Clone)]
pub struct TestFile {
    pub dataset: String,
    /// A `-SKIP` (or `-WASM_ONLY`, in a non-WASM build) in the *header* disables
    /// the whole file, like C++'s group-level `DISABLED_` prefix.
    pub header_skip: bool,
    /// A header `-BUFFER_POOL_SIZE N` — the memory limit `bm_info` reports.
    pub buffer_pool_size: Option<i64>,
    pub cases: Vec<TestCase>,
}

const DIRECTIVE_PREFIXES: &[&str] = &[
    "-CASE",
    "-STATEMENT",
    "-LOG",
    "-CHECK_ORDER",
    "-CHECK_PRECISION",
    "-CHECK_COLUMN_NAMES",
    "-SKIP",
    "-DATASET",
    "-PARAMETER",
    "-DEFINE",
    "-INSERT",
    "-LOOP",
    "-ENDLOOP",
    "-PARALLELISM",
    "-RELOADDB",
    "-IMPORT_DATABASE",
    "-REMOVE_FILE",
    "-BUFFER_POOL_SIZE",
    "-CREATE_CONNECTION",
    "-CREATE_DATASET_SCHEMA",
    "-BEGIN_CONCURRENT_EXECUTION",
    "-END_CONCURRENT_EXECUTION",
    "-BATCH_STATEMENTS",
    "-CHECKPOINT_WAIT_TIMEOUT",
    "-TEST_FWD_ONLY_REL",
    "-SET",
    "-MULTI_COPY_RANDOM",
    "-WASM_ONLY",
];

/// Substitute `-SET`-defined `${var}`s (parse-time, like C++ `replaceVariables`).
/// Applied to statement text, expected rows, and expected error/regex text.
fn substitute_vars(s: &str, vars: &HashMap<String, String>) -> String {
    if !s.contains("${") {
        return s.to_string();
    }
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    // Built-in corpus variables: the engine version db_version() reports,
    // and the repo root (expected rows echo COPY file paths through it).
    out = out.replace("${KOKO_VERSION}", "0.17.0");
    if out.contains("${KOKO_ROOT_DIRECTORY}") {
        if let Ok(root) = std::env::var("KOKO_ROOT_DIRECTORY") {
            out = out.replace("${KOKO_ROOT_DIRECTORY}", &root);
        }
    }
    // Export/import fixtures use caller-provided temporary directories such as
    // `${KOKO_EXPORT_DB_DIRECTORY}`. Expand any remaining environment-backed
    // corpus variable while preserving unknown placeholders verbatim.
    let mut cursor = 0;
    while let Some(relative_start) = out[cursor..].find("${") {
        let start = cursor + relative_start;
        let Some(relative_end) = out[start + 2..].find('}') else {
            break;
        };
        let end = start + 2 + relative_end;
        let name = &out[start + 2..end];
        let Ok(value) = std::env::var(name) else {
            cursor = end + 1;
            continue;
        };
        out.replace_range(start..=end, &value);
        cursor = start + value.len();
    }
    out
}

/// Evaluate the `-SET <name> <expr>` forms implemented by the C++ corpus
/// harness: `REPEAT`, `ARANGE`, `current_timestamp()`, PCG32 seed/random calls,
/// quoted strings, and bare integers.
#[derive(Debug, Clone, Copy, Default)]
struct OraclePcg32 {
    state: u64,
}

impl OraclePcg32 {
    const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
    const INCREMENT: u64 = 1_442_695_040_888_963_407;

    fn seed(&mut self, seed: u64) {
        self.state = seed
            .wrapping_add(Self::INCREMENT)
            .wrapping_mul(Self::MULTIPLIER)
            .wrapping_add(Self::INCREMENT);
    }

    fn next(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(Self::MULTIPLIER)
            .wrapping_add(Self::INCREMENT);
        let shifted = (((old ^ (old >> 18)) >> 27) & u32::MAX as u64) as u32;
        shifted.rotate_right((old >> 59) as u32)
    }

    fn bounded(&mut self, upper: u32) -> u32 {
        let threshold = upper.wrapping_neg() % upper;
        loop {
            let value = self.next();
            if value >= threshold {
                return value % upper;
            }
        }
    }
}

fn eval_set_expr(
    expr: &str,
    vars: &HashMap<String, String>,
    random: &mut OraclePcg32,
) -> Result<String, String> {
    let expanded = substitute_vars(expr.trim(), vars);
    let e = expanded.trim();
    let toks: Vec<&str> = e.split_whitespace().collect();
    let first = toks.first().copied().unwrap_or_default();
    if first.eq_ignore_ascii_case("REPEAT") {
        let times: u64 = toks
            .get(1)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("invalid REPEAT expression `{expr}`"))?;
        let start = e
            .find('"')
            .ok_or_else(|| format!("invalid REPEAT expression `{expr}`"))?;
        let end = e
            .rfind('"')
            .filter(|end| *end > start)
            .ok_or_else(|| format!("invalid REPEAT expression `{expr}`"))?;
        let template = &e[start + 1..end];
        let mut output = String::new();
        for index in 1..=times {
            output.push_str(&template.replace("${count}", &index.to_string()));
        }
        return Ok(output);
    }
    if first == "ARANGE" {
        let start: i64 = toks
            .get(1)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("invalid ARANGE expression `{expr}`"))?;
        let end: i64 = toks
            .get(2)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("invalid ARANGE expression `{expr}`"))?;
        return Ok(format!(
            "[{}]",
            (start..=end)
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if e.eq_ignore_ascii_case("current_timestamp()") {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("current_timestamp() failed: {error}"))?
            .as_micros()
            .min(u64::MAX as u128) as u64;
        return Ok(micros.to_string());
    }
    if let Some(seed) = e
        .strip_prefix("random.set_seed(")
        .and_then(|value| value.strip_suffix(')'))
    {
        let seed = seed
            .parse::<u64>()
            .map_err(|_| format!("invalid random seed `{seed}`"))?;
        random.seed(seed);
        return Ok("0".to_string());
    }
    if let Some(upper) = e
        .strip_prefix("random.randInt32(")
        .and_then(|value| value.strip_suffix(')'))
    {
        let upper = upper
            .parse::<u32>()
            .map_err(|_| format!("invalid random bound `{upper}`"))?;
        if upper == 0 {
            return Err("random bound must be positive".to_string());
        }
        return Ok(random.bounded(upper).to_string());
    }
    if e.len() >= 2 && e.starts_with('"') && e.ends_with('"') {
        return Ok(e[1..e.len() - 1].to_string());
    }
    e.parse::<i64>()
        .map(|value| value.to_string())
        .map_err(|_| format!("invalid SET expression `{expr}`"))
}

fn loop_values(
    spec: &str,
    vars: &HashMap<String, String>,
) -> Result<(String, Vec<String>), String> {
    let expanded = substitute_vars(spec, vars);
    let tokens: Vec<&str> = expanded.split_whitespace().collect();
    let variable = tokens
        .first()
        .ok_or_else(|| format!("invalid LOOP syntax `{spec}`"))?
        .to_string();
    let values = if tokens.len() == 2 && tokens[1].starts_with('[') && tokens[1].ends_with(']') {
        tokens[1][1..tokens[1].len() - 1]
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    } else if tokens.len() >= 3 {
        let start: i64 = tokens[1]
            .parse()
            .map_err(|_| format!("invalid LOOP start `{}`", tokens[1]))?;
        let end: i64 = tokens[2]
            .parse()
            .map_err(|_| format!("invalid LOOP end `{}`", tokens[2]))?;
        let step: usize = tokens
            .get(3)
            .map_or(Ok(1), |value| value.parse())
            .map_err(|_| format!("invalid LOOP step in `{spec}`"))?;
        if step == 0 {
            return Err(format!("LOOP step must be positive in `{spec}`"));
        }
        (start..=end)
            .step_by(step)
            .map(|value| value.to_string())
            .collect()
    } else {
        return Err(format!("invalid LOOP syntax `{spec}`"));
    };
    Ok((variable, values))
}

fn preprocess_segment(
    lines: &[&str],
    vars: &mut HashMap<String, String>,
    random: &mut OraclePcg32,
    output: &mut Vec<String>,
) -> Result<(), String> {
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if let Some(spec) = trimmed.strip_prefix("-LOOP ") {
            let mut depth = 1usize;
            let mut end = index + 1;
            while end < lines.len() && depth != 0 {
                let candidate = lines[end].trim_start();
                if candidate.starts_with("-LOOP ") {
                    depth += 1;
                } else if candidate == "-ENDLOOP" {
                    depth -= 1;
                }
                end += 1;
            }
            if depth != 0 {
                return Err(format!("unterminated LOOP `{}`", lines[index].trim()));
            }
            let body_end = end - 1;
            let (variable, values) = loop_values(spec, vars)?;
            for value in values {
                vars.insert(variable.clone(), value);
                preprocess_segment(&lines[index + 1..body_end], vars, random, output)?;
            }
            index = end;
            continue;
        }
        if trimmed == "-ENDLOOP" {
            return Err("unexpected -ENDLOOP".to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("-SET ") {
            let (name, expr) = rest
                .trim()
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("invalid SET directive `{trimmed}`"))?;
            let value = eval_set_expr(expr, vars, random)?;
            vars.insert(name.to_string(), value);
            index += 1;
            continue;
        }
        output.push(substitute_vars(lines[index], vars));
        index += 1;
    }
    Ok(())
}

fn preprocess_control_directives(content: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut vars = HashMap::new();
    let mut random = OraclePcg32::default();
    let mut output = Vec::with_capacity(lines.len());
    preprocess_segment(&lines, &mut vars, &mut random, &mut output)?;
    Ok(output.join("\n"))
}

/// Whether `t` (already trim_start'ed) is a well-formed result marker. Like
/// the C++ tokenizer, only the exact token `----` counts — `----1` or `-----`
/// is an unknown line that glues to the statement, which is then silently
/// DISCARDED when no real result block follows.
fn is_result_marker(t: &str) -> bool {
    t == "----" || t.starts_with("---- ")
}

fn is_directive(line: &str) -> bool {
    let t = line.trim_start();
    is_result_marker(t) || t == "--" || DIRECTIVE_PREFIXES.iter().any(|p| t.starts_with(p))
}

/// Strip Cypher comments from one statement line: `//` to end-of-line and inline
/// `/* … */`, both only *outside* string literals (a `://` in a quoted path is
/// data). Operates per line, so a trailing `//` comment cannot swallow the
/// statement's next continuation line.
fn strip_cypher_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == '\\' {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '/' && chars.peek() == Some(&'/') => break,
            None if c == '/' && chars.peek() == Some(&'*') => {
                chars.next();
                // Skip to `*/` (or end of line for an unterminated block).
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            None => {
                out.push(c);
                if c == '\'' || c == '"' {
                    quote = Some(c);
                }
            }
        }
    }
    out.trim_end().to_string()
}

/// Whether a trimmed line is a bare `-SKIP` directive — possibly with a trailing
/// comment (`-SKIP  # reason`) — as opposed to the conditional `-SKIP_*` family.
fn is_skip_directive(t: &str) -> bool {
    t == "-SKIP"
        || t.strip_prefix("-SKIP")
            .is_some_and(|r| r.starts_with(' ') || r.starts_with('\t') || r.starts_with('#'))
}

/// Split a `-STATEMENT` body into its `;`-separated statements, ignoring `;` inside
/// string literals (single or double quoted, with backslash escapes) and dropping
/// empty segments (so a trailing `;` yields no extra statement). A statement with
/// no `;` returns a single element.
fn split_statements(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = query.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == '\\' {
                    // An escaped character is part of the string verbatim.
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None if c == ';' => {
                // Keep the terminator: the engine's decorated parser errors
                // quote the statement as received, `;` included (like the C++
                // connection, which gets each statement with its terminator).
                cur.push(c);
                let t = cur.trim();
                if t != ";" {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            None => {
                cur.push(c);
                if c == '\'' || c == '"' {
                    quote = Some(c);
                }
            }
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// Split a leading `[connName]` routing prefix off a `-STATEMENT` body. Only a
/// bracketed *identifier* (`[A-Za-z_][A-Za-z0-9_]*`) is treated as a connection
/// name, so a statement that genuinely starts with a list literal (`[1, 2]`) is
/// left untouched. Returns `(Some(name), rest)` or `(None, body)`.
fn extract_conn_prefix(body: &str) -> (Option<String>, String) {
    let t = body.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let name = &rest[..close];
            if !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return (
                    Some(name.to_string()),
                    rest[close + 1..].trim_start().to_string(),
                );
            }
        }
    }
    (None, body.to_string())
}

/// Parse one `-STATEMENT` starting at `lines[i]` (its body + continuation lines +
/// one-or-more consecutive `----` result blocks). A body may pack several
/// `;`-separated statements, each zipped to its own `----` block; a leading
/// `[conn]` prefix routes all of them to that connection. Returns the parsed
/// statements and the index just past the result block(s). Shared by top-level
/// case parsing and `-DEFINE_STATEMENT_BLOCK` capture.
/// A transaction-control statement (silent in a C++ multi-statement chain).
fn is_txn_statement(q: &str) -> bool {
    let t = q.trim_start().to_ascii_uppercase();
    t.starts_with("BEGIN") || t.starts_with("COMMIT") || t.starts_with("ROLLBACK")
}

#[allow(clippy::too_many_arguments)]
fn parse_one_statement(
    lines: &[&str],
    mut i: usize,
    mut check_order: bool,
    mut check_precision: bool,
    mut check_column_names: bool,
    params: &[(String, Value)],
    vars: &HashMap<String, String>,
    body_override: Option<&str>,
) -> Result<(Vec<TestStatement>, usize), String> {
    let start_line = i;
    let body = match body_override {
        Some(body) => body,
        None => lines[i]
            .trim_start()
            .strip_prefix("-STATEMENT ")
            .expect("caller verified the -STATEMENT prefix")
            .trim(),
    };
    let mut query = strip_cypher_comments(body);
    i += 1;
    // Continuation lines until a directive (typically the `----` block); a
    // `#`-comment line inside a statement is not part of the query. Cypher
    // `//`-to-EOL and inline `/* */` comments are stripped per line (before
    // joining — a trailing `//` must not swallow the next continuation line).
    let mut cont_count = 0usize;
    while i < lines.len() && !is_directive(lines[i]) {
        let cont = lines[i].trim_end();
        if !cont.trim_start().starts_with('#') {
            // C++ `extractTextBeforeNextStatement(ignoreLineBreak=true)` joins
            // continuation lines with a SINGLE-SPACE delimiter BETWEEN them
            // (the first is appended straight to the -STATEMENT text), on top
            // of each line's own indentation — so a decorated parser error's
            // quoted window has 12 spaces before the first continuation but 13
            // (delimiter + indent) before the second (tck match4.Scenario9&10).
            if cont_count > 0 {
                query.push(' ');
            }
            query.push_str(&strip_cypher_comments(cont));
            cont_count += 1;
        }
        i += 1;
    }
    // Flags may also appear between the statement and its result block.
    while i < lines.len() {
        let ft = lines[i].trim();
        if ft == "-CHECK_ORDER" {
            check_order = true;
            i += 1;
        } else if ft == "-CHECK_PRECISION" {
            check_precision = true;
            i += 1;
        } else if ft == "-CHECK_COLUMN_NAMES" {
            check_column_names = true;
            i += 1;
        } else if ft.starts_with("-LOG") {
            i += 1;
        } else {
            break;
        }
    }
    if i >= lines.len() || !is_result_marker(lines[i].trim_start()) {
        // No (well-formed) result block: C++ absorbs the statement into the
        // next one's parse and never runs it — discard silently.
        return Ok((Vec::new(), i));
    }
    // One or more consecutive `---- ` result blocks.
    let mut expecteds = Vec::new();
    while i < lines.len() && is_result_marker(lines[i].trim_start()) {
        let spec = lines[i].trim_start().trim_start_matches('-').trim();
        i += 1;
        let expected = if spec == "ok" {
            Expected::Ok
        } else if spec == "error(regex)" {
            // Like `error`, but the collected text is an anchored regex over the
            // (right-trimmed) actual error. `${var}`s are substituted at parse
            // time (C++ replaceVariables on the ERROR_REGEX text).
            let start = i;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.is_empty() || t.starts_with('#') || is_directive(lines[i]) {
                    break;
                }
                i += 1;
            }
            let pat = lines[start..i]
                .iter()
                .map(|l| l.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            Expected::ErrorRegex(substitute_vars(&pat, vars))
        } else if spec == "hash" {
            // The next line reads `N tuples hashed to <md5>`: the leading token is
            // the row count, the trailing token the digest (middle words ignored,
            // like the C++ parser).
            let line = lines.get(i).map(|l| l.trim()).unwrap_or_default();
            let toks: Vec<&str> = line.split_whitespace().collect();
            let count: usize = toks
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("invalid hash expectation line `{line}`"))?;
            let md5 = toks.last().copied().unwrap_or_default().to_string();
            i += 1;
            Expected::Hash { count, md5 }
        } else if spec == "error" {
            // The expected error spans every line up to the next directive (C++
            // `extractTextBeforeNextStatement`), so a multi-line message — e.g. a
            // function-signature mismatch's `Actual:`/`Expected:` lines — is kept
            // whole rather than truncated to its first line. Joined with `\n` and
            // right-trimmed (the blank line before the next `-STATEMENT` falls away),
            // matching the C++ runner's `rtrim`.
            // The error runs over consecutive non-blank lines (a real multi-line
            // message — e.g. a signature mismatch's contiguous `Actual:`/`Expected:`
            // lines — has no internal blank); it stops at the first blank line,
            // `#`-comment, directive, or EOF that separates it from the next case.
            let start = i;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.is_empty() || t.starts_with('#') || is_directive(lines[i]) {
                    break;
                }
                i += 1;
            }
            let msg = lines[start..i]
                .iter()
                .map(|l| l.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            Expected::Error(substitute_vars(&msg, vars))
        } else {
            let n: usize = spec
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("invalid result count `{spec}`"))?;
            // A `<FILE>:name` line means the N expected rows are stored in an
            // answer file (resolved at run time), not inline.
            if let Some(name) = lines
                .get(i)
                .map(|l| l.trim())
                .and_then(|l| l.strip_prefix("<FILE>:"))
            {
                let file = name.trim().to_string();
                i += 1;
                Expected::RowsFile(file)
            } else {
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    // Verbatim (no trim_end): a result cell can be — or end in —
                    // whitespace (e.g. ORDER BY over `['', ' ', …]`), so trimming the
                    // expected line would conflate `' '` with `''` and falsely fail.
                    // `lines()` already drops the `\n`/`\r\n` terminator.
                    let row = lines.get(i).map(|l| l.to_string()).unwrap_or_default();
                    rows.push(substitute_vars(&row, vars));
                    i += 1;
                }
                Expected::Rows(rows)
            }
        };
        expecteds.push(expected);
    }
    // A leading `[conn]` prefix applies to every `;`-split sub-statement.
    let (conn_name, clean) = extract_conn_prefix(&substitute_vars(&query, vars));
    let stmts = split_statements(&clean);
    // Result blocks pair with statements in order, except transaction-control
    // statements inside a MULTI-statement line, which produce no result in the
    // C++ chain (BEGIN/COMMIT are silent there); surplus statements run
    // unchecked, surplus blocks are ignored.
    let multi = stmts.len() > 1;
    let group = if multi { Some(start_line) } else { None };
    let mut blocks = expecteds.into_iter();
    let out = stmts
        .into_iter()
        .map(|q| {
            let expected = if multi && is_txn_statement(&q) {
                Expected::Unchecked
            } else {
                blocks.next().unwrap_or(Expected::Unchecked)
            };
            TestStatement {
                query: q,
                check_order,
                check_precision,
                check_column_names,
                params: params.to_vec(),
                expected,
                conn_name: conn_name.clone(),
                label: None,
                line_group: group,
                multi_copy: None,
                batch_file: None,
                dataset_action: None,
                concurrent_group: None,
                reset_database: false,
                remove_file: None,
            }
        })
        .collect();
    Ok((out, i))
}

/// Parse `.test` file content. Returns a human-readable error on malformed input.
pub fn parse_test_file(content: &str) -> Result<TestFile, String> {
    let expanded = preprocess_control_directives(content)?;
    let lines: Vec<&str> = expanded.lines().collect();
    let mut i = 0;

    // Header: up to a line that trims to "--".
    let mut dataset = String::from("empty");
    let mut buffer_pool_size: Option<i64> = None;
    let mut header_skip = false;
    let mut saw_separator = false;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim() == "--" {
            saw_separator = true;
            i += 1;
            break;
        }
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("-DATASET") {
            // `-DATASET <TYPE> <name> [...]`
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                dataset = parts[1].to_string();
            }
        }
        if let Some(rest) = t.strip_prefix("-BUFFER_POOL_SIZE") {
            buffer_pool_size = rest.trim().parse::<i64>().ok();
        }
        // A header-level `-SKIP` disables the whole group (C++ parser.cpp:190-204);
        // `-WASM_ONLY` skips everywhere except a WASM build; `-SKIP_IN_MEM` skips
        // here too (this engine is in-memory, like the C++ in-mem mode). Other
        // `-SKIP_*` conditionals do not skip a standard build.
        if is_skip_directive(t) || t.starts_with("-WASM_ONLY") || t.starts_with("-SKIP_IN_MEM") {
            header_skip = true;
        }
        i += 1;
    }

    let mut cases: Vec<TestCase> = Vec::new();
    let mut check_order = false;
    let mut check_precision = false;
    let mut check_column_names = false;
    let mut pending_log: Option<String> = None;
    let mut params: Vec<(String, Value)> = Vec::new();
    // Dynamic `-SET` and `-LOOP` substitutions have already been expanded.
    let vars: HashMap<String, String> = HashMap::new();
    // `-DEFINE_STATEMENT_BLOCK NAME [ … ]` macros, expanded by `-INSERT_STATEMENT_BLOCK`.
    let mut blocks: HashMap<String, Vec<TestStatement>> = HashMap::new();
    let mut current_concurrent_group = None;
    let mut next_concurrent_group = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_end();
        let t = line.trim_start();

        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        // A skipped case's body is opaque (C++ never *runs* a DISABLED_ case, so
        // an exotic directive/result form inside one must not parse-fail the whole
        // file). Skim everything until the next `-CASE`; `-DEFINE_STATEMENT_BLOCK`
        // is exempt so later cases can still expand blocks defined in between.
        if cases.last().is_some_and(|c| c.skip)
            && !t.starts_with("-CASE ")
            && !t.starts_with("-DEFINE_STATEMENT_BLOCK ")
        {
            i += 1;
            continue;
        }
        if let Some(name) = t.strip_prefix("-CASE ") {
            if current_concurrent_group.is_some() {
                return Err("found -CASE before -END_CONCURRENT_EXECUTION".to_string());
            }
            cases.push(TestCase {
                name: name.trim().to_string(),
                skip: header_skip,
                skip_reason: header_skip.then(|| "-SKIP".to_string()),
                statements: Vec::new(),
            });
            check_order = false;
            check_precision = false;
            params.clear();
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("-PARAMETER ") {
            if let Some((name, val)) = rest.split_once('=') {
                let name = name.trim().to_string();
                params.retain(|(n, _)| n != &name);
                params.push((name, parse_param_value(val)));
            }
            i += 1;
            continue;
        }
        // The C++ harness reopens a fresh database at this path before the
        // following `IMPORT DATABASE` query. The Rust product is intentionally
        // in-memory, so preserve the observable empty-database boundary without
        // introducing a native database path.
        if t.starts_with("-IMPORT_DATABASE ") {
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -IMPORT_DATABASE before any -CASE".to_string())?;
            case.statements.push(TestStatement {
                query: String::new(),
                check_order: false,
                check_precision: false,
                check_column_names: false,
                params: Vec::new(),
                expected: Expected::Unchecked,
                conn_name: None,
                label: None,
                line_group: None,
                multi_copy: None,
                batch_file: None,
                dataset_action: None,
                concurrent_group: None,
                reset_database: true,
                remove_file: None,
            });
            i += 1;
            continue;
        }
        if let Some(path) = t.strip_prefix("-REMOVE_FILE ") {
            let path = substitute_vars(path.trim(), &vars);
            let path = path
                .strip_prefix('"')
                .and_then(|path| path.strip_suffix('"'))
                .unwrap_or(&path)
                .to_string();
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -REMOVE_FILE before any -CASE".to_string())?;
            case.statements.push(TestStatement {
                query: String::new(),
                check_order: false,
                check_precision: false,
                check_column_names: false,
                params: Vec::new(),
                expected: Expected::Unchecked,
                conn_name: None,
                label: None,
                line_group: None,
                multi_copy: None,
                batch_file: None,
                dataset_action: None,
                concurrent_group: None,
                reset_database: false,
                remove_file: Some(path),
            });
            i += 1;
            continue;
        }
        // `-MULTI_COPY_RANDOM <splits> <table> [SEED s0 s1] "<source>"`: run the
        // source CSV as <splits> sequential COPYs (synthetic statement; the seed
        // only affects storage batch boundaries, never the logical result).
        if let Some(rest) = t.strip_prefix("-MULTI_COPY_RANDOM ") {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            let splits: usize = toks
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("invalid -MULTI_COPY_RANDOM line `{t}`"))?;
            let table = toks
                .get(1)
                .ok_or_else(|| format!("invalid -MULTI_COPY_RANDOM line `{t}`"))?
                .to_string();
            let source = rest
                .find('"')
                .and_then(|a| rest.rfind('"').filter(|b| *b > a).map(|b| &rest[a + 1..b]))
                .ok_or_else(|| format!("invalid -MULTI_COPY_RANDOM source `{t}`"))?;
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -MULTI_COPY_RANDOM before any -CASE".to_string())?;
            case.statements.push(TestStatement {
                query: String::new(),
                check_order: false,
                check_precision: false,
                check_column_names: false,
                params: Vec::new(),
                expected: Expected::Ok,
                conn_name: None,
                label: None,
                line_group: None,
                multi_copy: Some(MultiCopySpec {
                    splits,
                    table,
                    source: substitute_vars(source, &vars),
                }),
                batch_file: None,
                dataset_action: None,
                concurrent_group: current_concurrent_group,
                reset_database: false,
                remove_file: None,
            });
            i += 1;
            continue;
        }
        if is_skip_directive(t) {
            if let Some(c) = cases.last_mut() {
                c.skip = true;
                c.skip_reason = Some("-SKIP".to_string());
            }
            i += 1;
            continue;
        }
        // `-SKIP_IN_MEM`: skip the whole case (we always run in-memory).
        if t == "-SKIP_IN_MEM" {
            if let Some(c) = cases.last_mut() {
                c.skip = true;
                c.skip_reason = Some("-SKIP_IN_MEM".to_string());
            }
            i += 1;
            continue;
        }
        // `-WASM_ONLY`: the case runs only under a WASM build, which this never is
        // (C++ parser.cpp:115-119 skips it in a standard build).
        if t.starts_with("-WASM_ONLY") {
            if let Some(c) = cases.last_mut() {
                c.skip = true;
                c.skip_reason = Some("-WASM_ONLY".to_string());
            }
            i += 1;
            continue;
        }
        // `-RELOADDB`: a no-op in-memory (the data never left RAM), so drop the
        // directive and let the case continue — matching the C++ runner's
        // `if (!inMemMode)` reload guard.
        if t == "-RELOADDB" {
            i += 1;
            continue;
        }
        // `-CREATE_CONNECTION name`: named connections are created lazily on first
        // `[name]` use by the runner, so this only needs recognizing here.
        if t.starts_with("-CREATE_CONNECTION") {
            i += 1;
            continue;
        }
        if let Some(dataset) = t.strip_prefix("-CREATE_DATASET_SCHEMA ") {
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -CREATE_DATASET_SCHEMA before any -CASE".to_string())?;
            case.statements.push(TestStatement {
                query: String::new(),
                check_order: false,
                check_precision: false,
                check_column_names: false,
                params: Vec::new(),
                expected: Expected::Ok,
                conn_name: None,
                label: None,
                line_group: None,
                multi_copy: None,
                batch_file: None,
                dataset_action: Some(DatasetAction::CreateSchema(dataset.trim().to_string())),
                concurrent_group: None,
                reset_database: false,
                remove_file: None,
            });
            i += 1;
            continue;
        }
        if let Some(dataset) = t.strip_prefix("-INSERT_DATASET_BY_ROW ") {
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -INSERT_DATASET_BY_ROW before any -CASE".to_string())?;
            case.statements.push(TestStatement {
                query: String::new(),
                check_order: false,
                check_precision: false,
                check_column_names: false,
                params: Vec::new(),
                expected: Expected::Ok,
                conn_name: None,
                label: None,
                line_group: None,
                multi_copy: None,
                batch_file: None,
                dataset_action: Some(DatasetAction::InsertByRow(dataset.trim().to_string())),
                concurrent_group: None,
                reset_database: false,
                remove_file: None,
            });
            i += 1;
            continue;
        }
        if t == "-BEGIN_CONCURRENT_EXECUTION" {
            if current_concurrent_group.is_some() {
                return Err("nested -BEGIN_CONCURRENT_EXECUTION".to_string());
            }
            current_concurrent_group = Some(next_concurrent_group);
            next_concurrent_group += 1;
            i += 1;
            continue;
        }
        if t == "-END_CONCURRENT_EXECUTION" {
            if current_concurrent_group.take().is_none() {
                return Err("unmatched -END_CONCURRENT_EXECUTION".to_string());
            }
            i += 1;
            continue;
        }
        // `-DEFINE_STATEMENT_BLOCK NAME [ … ]`: capture the enclosed statements
        // (each a normal `-STATEMENT` + `----`) until a line that is just `]`.
        if let Some(rest) = t.strip_prefix("-DEFINE_STATEMENT_BLOCK ") {
            let name = rest.trim().trim_end_matches('[').trim().to_string();
            i += 1;
            let mut block: Vec<TestStatement> = Vec::new();
            while i < lines.len() {
                let bt = lines[i].trim();
                if bt == "]" {
                    i += 1;
                    break;
                }
                if bt.is_empty() || bt == "[" || bt.starts_with('#') {
                    i += 1;
                    continue;
                }
                if bt.starts_with("-STATEMENT ") {
                    let (stmts, ni) =
                        parse_one_statement(&lines, i, false, false, false, &[], &vars, None)?;
                    block.extend(stmts);
                    i = ni;
                } else {
                    i += 1;
                }
            }
            blocks.insert(name, block);
            continue;
        }
        // `-INSERT_STATEMENT_BLOCK NAME`: expand a captured block inline into the case.
        if let Some(rest) = t.strip_prefix("-INSERT_STATEMENT_BLOCK ") {
            let name = rest.trim().to_string();
            let mut block = blocks
                .get(&name)
                .ok_or_else(|| format!("statement block `{name}` is not defined"))?
                .clone();
            for statement in &mut block {
                statement.concurrent_group = current_concurrent_group;
            }
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -INSERT_STATEMENT_BLOCK before any -CASE".to_string())?;
            case.statements.extend(block);
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("-LOG") {
            // Remember the label (e.g. `q1`) for the next statement's perf timing.
            pending_log = Some(rest.trim().to_string());
            i += 1;
            continue;
        }
        if t == "-CHECK_ORDER" {
            check_order = true;
            i += 1;
            continue;
        }
        if t == "-CHECK_PRECISION" {
            check_precision = true;
            i += 1;
            continue;
        }
        if t == "-CHECK_COLUMN_NAMES" {
            check_column_names = true;
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("-BATCH_STATEMENTS ") {
            let (conn_name, source) = extract_conn_prefix(rest.trim());
            let file = source
                .strip_prefix("<FILE:>")
                .or_else(|| source.strip_prefix("<FILE>:"))
                .ok_or_else(|| format!("invalid -BATCH_STATEMENTS source `{source}`"))?
                .trim()
                .to_string();
            let (mut statements, next) = parse_one_statement(
                &lines,
                i,
                check_order,
                check_precision,
                check_column_names,
                &params,
                &vars,
                Some("RETURN 0"),
            )?;
            if statements.len() != 1 {
                return Err(format!(
                    "-BATCH_STATEMENTS `{file}` did not produce one expectation"
                ));
            }
            let statement = &mut statements[0];
            statement.query.clear();
            statement.conn_name = conn_name;
            statement.batch_file = Some(file);
            statement.concurrent_group = current_concurrent_group;
            if pending_log.is_some() {
                statement.label = pending_log.clone();
            }
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -BATCH_STATEMENTS before any -CASE".to_string())?;
            case.statements.extend(statements);
            i = next;
            check_order = false;
            check_precision = false;
            check_column_names = false;
            pending_log = None;
            continue;
        }
        if t.starts_with("-STATEMENT ") {
            let (mut stmts, ni) = parse_one_statement(
                &lines,
                i,
                check_order,
                check_precision,
                check_column_names,
                &params,
                &vars,
                None,
            )?;
            i = ni;
            for statement in &mut stmts {
                statement.concurrent_group = current_concurrent_group;
            }
            // Attach the pending `-LOG` label to the statement(s) it precedes.
            if pending_log.is_some() {
                for s in &mut stmts {
                    s.label = pending_log.clone();
                }
            }
            let case = cases
                .last_mut()
                .ok_or_else(|| "found -STATEMENT before any -CASE".to_string())?;
            case.statements.extend(stmts);
            check_order = false;
            check_precision = false;
            check_column_names = false;
            pending_log = None;
            continue;
        }
        // Unknown directive — skip (forward-compatibility).
        i += 1;
    }

    if current_concurrent_group.is_some() {
        return Err("unterminated -BEGIN_CONCURRENT_EXECUTION".to_string());
    }
    // Reject malformed files that would otherwise vacuously "pass" by running
    // zero assertions (a missing `--` separator swallows the whole body).
    if !saw_separator {
        return Err("missing `--` header/body separator".to_string());
    }
    if cases.is_empty() {
        return Err("no -CASE blocks found".to_string());
    }

    Ok(TestFile {
        dataset,
        header_skip,
        buffer_pool_size,
        cases,
    })
}

/// The outcome of running one case.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass,
    Skip(String),
    Fail(String),
}

/// Result of running a single case.
#[derive(Debug, Clone)]
pub struct CaseResult {
    pub name: String,
    pub outcome: Outcome,
}

/// Filesystem context for resolving a `.test` file's references against the real
/// corpus checkout. Both are `None` for the bespoke p0 fixtures (which use neither).
#[derive(Clone, Copy, Default)]
pub struct CorpusEnv<'a> {
    /// Directory holding `<FILE>:name` answer files (the corpus's `test/answers/`).
    pub answers_dir: Option<&'a Path>,
    /// `${KOKO_ROOT_DIRECTORY}` — the repo root, substituted into statement text so
    /// explicit `COPY … FROM "${KOKO_ROOT_DIRECTORY}/dataset/…"` paths resolve.
    pub root: Option<&'a Path>,
}

/// Expand corpus path variables in a statement (`${KOKO_ROOT_DIRECTORY}` → the
/// repo root). Borrows the input unchanged when there's nothing to expand.
fn expand_corpus_vars<'a>(query: &'a str, env: &CorpusEnv) -> Cow<'a, str> {
    match env.root {
        Some(root) if query.contains("${KOKO_ROOT_DIRECTORY}") => {
            Cow::Owned(query.replace("${KOKO_ROOT_DIRECTORY}", &root.to_string_lossy()))
        }
        _ => Cow::Borrowed(query),
    }
}

/// Unknown `${NAME}` placeholders are literal text in corpus error-regex
/// expectations. Escape only those still present after environment expansion;
/// braces used by the regex itself remain untouched.
fn escape_unexpanded_regex_vars(pattern: &str) -> Cow<'_, str> {
    let mut cursor = 0usize;
    let mut output = None::<String>;
    while let Some(relative_start) = pattern[cursor..].find("${") {
        let start = cursor + relative_start;
        let Some(relative_end) = pattern[start + 2..].find('}') else {
            break;
        };
        let end = start + 2 + relative_end;
        let buffer = output.get_or_insert_with(|| String::with_capacity(pattern.len() + 4));
        buffer.push_str(&pattern[cursor..start]);
        buffer.push_str(r"\$\{");
        buffer.push_str(&pattern[start + 2..end]);
        buffer.push_str(r"\}");
        cursor = end + 1;
    }
    match output {
        Some(mut output) => {
            output.push_str(&pattern[cursor..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(pattern),
    }
}

/// Run every case in a parsed file against a fresh in-memory database each.
///
/// `dataset_root`, if given, is the directory under which `-DATASET CSV <name>`
/// datasets live (each a subdirectory with `schema.cypher` + `copy.cypher`).
/// A case whose dataset is neither `empty` nor available under `dataset_root`
/// is skipped (not failed), so the suite stays green without the submodule.
pub fn run_test_file(file: &TestFile, dataset_root: Option<&Path>) -> Vec<CaseResult> {
    run_test_file_with(file, dataset_root, CorpusEnv::default())
}

/// Like [`run_test_file`], with a [`CorpusEnv`] for resolving `<FILE>:` answer
/// files and `${KOKO_ROOT_DIRECTORY}` paths against the real corpus checkout.
pub fn run_test_file_with(
    file: &TestFile,
    dataset_root: Option<&Path>,
    env: CorpusEnv,
) -> Vec<CaseResult> {
    // Case-insensitive: C++ lowercases the dataset path, so `-DATASET CSV EMPTY`
    // runs as an empty database there (19 corpus cases were hidden behind this).
    let is_empty = file.dataset.is_empty()
        || file.dataset.eq_ignore_ascii_case("empty")
        || file.dataset.eq_ignore_ascii_case("none");
    // A dataset is *bootstrapped* (schema.cypher + copy.cypher auto-loaded) only
    // when it ships a schema. Some corpus datasets (e.g. csv-dialect-detection,
    // csv-sniffing-test) are bare CSV directories that tests read via explicit
    // `${KOKO_ROOT_DIRECTORY}` paths with nothing to auto-load — those are still
    // "available" (run, don't skip) as long as the directory is present.
    let dataset_dir = if is_empty {
        None
    } else {
        dataset_root
            .map(|root| root.join(&file.dataset))
            .filter(|d| d.join("schema.cypher").exists())
    };
    let available = is_empty
        || dataset_dir.is_some()
        || dataset_root.is_some_and(|root| root.join(&file.dataset).is_dir());

    file.cases
        .iter()
        .map(|case| {
            let outcome = if case.skip {
                Outcome::Skip(
                    case.skip_reason
                        .clone()
                        .unwrap_or_else(|| "-SKIP".to_string()),
                )
            } else if !available {
                Outcome::Skip(format!("dataset `{}` is not available", file.dataset))
            } else {
                run_case(
                    case,
                    dataset_dir.as_deref(),
                    dataset_root,
                    env,
                    file.buffer_pool_size,
                )
            };
            CaseResult {
                name: case.name.clone(),
                outcome,
            }
        })
        .collect()
}

/// Whether the perf gate's per-statement timing is on (`KOKO_TIMING=1`). Reported to
/// stderr so it never perturbs the normal PASS/FAIL stdout (keeps gates byte-identical).
fn timing_on() -> bool {
    std::env::var("KOKO_TIMING").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// Render a caught panic payload as its message.
fn panic_msg(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn configured_connection(db: &Database) -> Connection {
    let connection = db.connect();
    connection
        .set_max_num_threads(2)
        .expect("test runner thread count is valid");
    connection
}

fn execute_checked_statement(
    connection: &Connection,
    statement: &TestStatement,
    env: CorpusEnv,
    index: usize,
    timing: bool,
) -> Result<bool, String> {
    let started = std::time::Instant::now();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        run_statement(connection, statement, env)
    }))
    .unwrap_or_else(|payload| Err(format!("PANICKED: {}", panic_msg(&*payload))));
    if timing {
        let label = statement
            .label
            .clone()
            .unwrap_or_else(|| format!("#{}", index + 1));
        let status = if result.is_ok() { "ok " } else { "ERR" };
        eprintln!(
            "[timing] {status} {label:>26} {:>9.3}s",
            started.elapsed().as_secs_f64()
        );
    }
    result.map_err(|message| {
        format!(
            "statement #{} `{}`{}: {message}",
            index + 1,
            truncate(&statement.query, 80),
            statement
                .conn_name
                .as_deref()
                .map(|name| format!(" [{name}]"))
                .unwrap_or_default(),
        )
    })
}

fn run_concurrent_group(
    db: &Database,
    connections: &mut HashMap<String, Connection>,
    statements: &[(usize, &TestStatement)],
    env: CorpusEnv,
    timing: bool,
) -> Result<(), String> {
    let mut grouped: Vec<(String, Vec<(usize, &TestStatement)>)> = Vec::new();
    for &(index, statement) in statements {
        let key = statement.conn_name.clone().unwrap_or_default();
        if let Some((_, queue)) = grouped.iter_mut().find(|(name, _)| name == &key) {
            queue.push((index, statement));
        } else {
            grouped.push((key, vec![(index, statement)]));
        }
    }
    let work: Vec<_> = grouped
        .into_iter()
        .map(|(key, queue)| {
            let connection = connections
                .remove(&key)
                .unwrap_or_else(|| configured_connection(db));
            (key, connection, queue)
        })
        .collect();
    let barrier = Arc::new(Barrier::new(work.len() + 1));
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = work
            .into_iter()
            .map(|(key, connection, queue)| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let result = panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut aborted_group = None;
                        for (index, statement) in queue {
                            if statement.line_group.is_some()
                                && statement.line_group == aborted_group
                            {
                                continue;
                            }
                            if execute_checked_statement(
                                &connection,
                                statement,
                                env,
                                index,
                                timing,
                            )? {
                                aborted_group = statement.line_group;
                            }
                        }
                        Ok(())
                    }))
                    .unwrap_or_else(|payload| {
                        Err(format!(
                            "concurrent worker PANICKED: {}",
                            panic_msg(&*payload)
                        ))
                    });
                    (key, connection, result)
                })
            })
            .collect();
        barrier.wait();
        handles
            .into_iter()
            .map(|handle| handle.join())
            .collect::<Vec<_>>()
    });
    let mut first_failure = None;
    for outcome in outcomes {
        match outcome {
            Ok((key, connection, result)) => {
                connections.insert(key, connection);
                if first_failure.is_none() {
                    first_failure = result.err();
                }
            }
            Err(payload) if first_failure.is_none() => {
                first_failure = Some(format!(
                    "concurrent worker PANICKED: {}",
                    panic_msg(&*payload)
                ));
            }
            Err(_) => {}
        }
    }
    match first_failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn resolve_manual_dataset(
    dataset: &str,
    dataset_root: Option<&Path>,
    env: CorpusEnv,
) -> Result<PathBuf, String> {
    if let Some(root) = dataset_root {
        let path = root.join(dataset);
        if path.is_dir() {
            return Ok(path);
        }
    }
    if let Some(root) = env.root {
        let path = root.join("dataset").join(dataset);
        if path.is_dir() {
            return Ok(path);
        }
    }
    Err(format!("manual dataset `{dataset}` is not available"))
}

fn script_lines(script: &str) -> impl Iterator<Item = &str> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with('#'))
        .map(|line| line.trim_end_matches(';').trim())
        .filter(|line| !line.is_empty())
}

fn execute_schema(connection: &Connection, directory: &Path) -> Result<(), String> {
    let path = directory.join("schema.cypher");
    let script = std::fs::read_to_string(&path)
        .map_err(|error| format!("reading dataset schema {}: {error}", path.display()))?;
    let storage_root = directory.to_string_lossy().replace('\'', "''");
    for statement in script_lines(&script) {
        let statement = statement
            .replace("storage = '.'", &format!("storage = '{storage_root}'"))
            .replace("storage='.'", &format!("storage='{storage_root}'"));
        connection
            .query(&statement)
            .map_err(|error| format!("executing dataset schema `{statement}`: {error}"))?;
    }
    Ok(())
}

fn query_first_string(connection: &Connection, query: &str) -> Result<String, String> {
    let result = connection
        .query(query)
        .map_err(|error| format!("executing dataset metadata query `{query}`: {error}"))?;
    result
        .rows()
        .next()
        .ok_or_else(|| format!("dataset metadata query returned no rows: `{query}`"))?
        .get::<String>(0)
        .map_err(|error| format!("reading dataset metadata query `{query}`: {error}"))
}

fn parse_copy_source(line: &str) -> Result<(&str, &str), String> {
    let mut tokens = line.split_whitespace();
    if !tokens
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case("COPY"))
    {
        return Err(format!("invalid dataset COPY statement `{line}`"));
    }
    let table = tokens
        .next()
        .ok_or_else(|| format!("missing table in dataset COPY statement `{line}`"))?;
    let start = line
        .find('"')
        .ok_or_else(|| format!("missing file in dataset COPY statement `{line}`"))?;
    let end = line[start + 1..]
        .find('"')
        .map(|offset| start + 1 + offset)
        .ok_or_else(|| format!("unterminated file in dataset COPY statement `{line}`"))?;
    Ok((table, &line[start + 1..end]))
}

fn execute_dataset_by_row(connection: &Connection, directory: &Path) -> Result<(), String> {
    let copy_path = directory.join("copy.cypher");
    let copy = std::fs::read_to_string(&copy_path).map_err(|error| {
        format!(
            "reading dataset copy script {}: {error}",
            copy_path.display()
        )
    })?;
    for copy_statement in script_lines(&copy) {
        let (table, source) = parse_copy_source(copy_statement)?;
        let table_type = query_first_string(
            connection,
            &format!("CALL show_tables() WHERE name='{table}' RETURN type"),
        )?;
        let properties_result = connection
            .query(&format!(
                "CALL table_info('{table}') RETURN name, type ORDER BY `property id`"
            ))
            .map_err(|error| format!("reading properties for `{table}`: {error}"))?;
        let mut properties = Vec::new();
        for row in properties_result.rows() {
            let name = row
                .get::<String>(0)
                .map_err(|error| format!("reading property name for `{table}`: {error}"))?;
            let data_type = row
                .get::<String>(1)
                .map_err(|error| format!("reading property type for `{table}`: {error}"))?;
            properties.push((name, data_type));
        }
        let property_header = properties
            .iter()
            .map(|(name, data_type)| format!("{name} {data_type}"))
            .collect::<Vec<_>>()
            .join(",");
        let property_body = properties
            .iter()
            .map(|(name, _)| format!("{name}:{name}"))
            .collect::<Vec<_>>()
            .join(",");
        let source_path = directory.join(source);
        let source_path = source_path
            .to_str()
            .ok_or_else(|| format!("dataset path is not UTF-8: {}", source_path.display()))?
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let query = if table_type.eq_ignore_ascii_case("NODE") {
            format!(
                "LOAD WITH HEADERS ({property_header}) FROM \"{source_path}\" \
                 CREATE (:{table} {{{property_body}}})"
            )
        } else if table_type.eq_ignore_ascii_case("REL") {
            let connection_result = connection
                .query(&format!("CALL show_connection('{table}') RETURN *"))
                .map_err(|error| {
                    format!("reading relationship endpoints for `{table}`: {error}")
                })?;
            let row = connection_result
                .rows()
                .next()
                .ok_or_else(|| format!("relationship `{table}` has no endpoint metadata"))?;
            let source_table = row
                .get::<String>(0)
                .map_err(|error| format!("reading source table for `{table}`: {error}"))?;
            let destination_table = row
                .get::<String>(1)
                .map_err(|error| format!("reading destination table for `{table}`: {error}"))?;
            let source_key = row
                .get::<String>(2)
                .map_err(|error| format!("reading source key for `{table}`: {error}"))?;
            let destination_key = row
                .get::<String>(3)
                .map_err(|error| format!("reading destination key for `{table}`: {error}"))?;
            let source_type = query_first_string(
                connection,
                &format!("CALL table_info('{source_table}') WHERE name='{source_key}' RETURN type"),
            )?;
            let destination_type = query_first_string(
                connection,
                &format!(
                    "CALL table_info('{destination_table}') WHERE name='{destination_key}' RETURN type"
                ),
            )?;
            let header = if property_header.is_empty() {
                format!("aid_ {source_type},bid_ {destination_type}")
            } else {
                format!("aid_ {source_type},bid_ {destination_type},{property_header}")
            };
            format!(
                "LOAD WITH HEADERS ({header}) FROM \"{source_path}\" \
                 MATCH (a:{source_table}), (b:{destination_table}) \
                 WHERE a.{source_key} = aid_ AND b.{destination_key} = bid_ \
                 CREATE (a)-[:{table} {{{property_body}}}]->(b)"
            )
        } else {
            return Err(format!(
                "unsupported table type `{table_type}` for dataset table `{table}`"
            ));
        };
        connection
            .query(&query)
            .map_err(|error| format!("row-wise load for `{table}` failed: {error}"))?;
    }
    Ok(())
}

fn execute_dataset_action(
    connection: &Connection,
    action: &DatasetAction,
    dataset_root: Option<&Path>,
    env: CorpusEnv,
) -> Result<(), String> {
    let dataset = match action {
        DatasetAction::CreateSchema(dataset) | DatasetAction::InsertByRow(dataset) => dataset,
    };
    let directory = resolve_manual_dataset(dataset, dataset_root, env)?;
    match action {
        DatasetAction::CreateSchema(_) => execute_schema(connection, &directory),
        DatasetAction::InsertByRow(_) => execute_dataset_by_row(connection, &directory),
    }
}

fn run_case(
    case: &TestCase,
    dataset_dir: Option<&Path>,
    dataset_root: Option<&Path>,
    env: CorpusEnv,
    buffer_pool_size: Option<i64>,
) -> Outcome {
    let timing = timing_on();
    let config = match buffer_pool_size.filter(|&bytes| bytes > 0) {
        Some(bytes) => DatabaseConfig::new()
            .with_memory_limit(bytes as u64)
            .expect("positive corpus buffer-pool limit"),
        None => DatabaseConfig::default(),
    };
    let mut db =
        Database::in_memory_with_config(config.clone()).expect("validated corpus database config");
    let mut connections = HashMap::new();
    connections.insert(String::new(), configured_connection(&db));
    if let Some(directory) = dataset_dir {
        let started = std::time::Instant::now();
        match panic::catch_unwind(AssertUnwindSafe(|| {
            connections[""].load_csv_dataset(directory)
        })) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Outcome::Fail(format!("dataset load failed: {error}")),
            Err(payload) => {
                return Outcome::Fail(format!("dataset load PANICKED: {}", panic_msg(&*payload)));
            }
        }
        if timing {
            eprintln!(
                "[timing] load {:>26} {:>9.3}s",
                case.name,
                started.elapsed().as_secs_f64()
            );
        }
    }
    let mut aborted_group = None;
    let mut index = 0;
    while index < case.statements.len() {
        let statement = &case.statements[index];
        if let Some(group) = statement.concurrent_group {
            let end = case.statements[index..]
                .iter()
                .position(|candidate| candidate.concurrent_group != Some(group))
                .map_or(case.statements.len(), |offset| index + offset);
            let group_statements: Vec<_> = case.statements[index..end]
                .iter()
                .enumerate()
                .map(|(offset, statement)| (index + offset, statement))
                .collect();
            if let Err(error) =
                run_concurrent_group(&db, &mut connections, &group_statements, env, timing)
            {
                return Outcome::Fail(error);
            }
            index = end;
            continue;
        }
        if statement.line_group.is_some() && statement.line_group == aborted_group {
            index += 1;
            continue;
        }
        if let Some(path) = &statement.remove_file {
            if let Err(error) = std::fs::remove_file(path) {
                return Outcome::Fail(format!(
                    "statement #{} `-REMOVE_FILE {path}`: {error}",
                    index + 1
                ));
            }
            index += 1;
            continue;
        }
        if statement.reset_database {
            db = Database::in_memory_with_config(config.clone())
                .expect("validated corpus database config");
            connections.clear();
            connections.insert(String::new(), configured_connection(&db));
            index += 1;
            continue;
        }
        if let Some(action) = &statement.dataset_action {
            let connection = connections
                .get("")
                .expect("default corpus connection is always present");
            if let Err(error) = execute_dataset_action(connection, action, dataset_root, env) {
                return Outcome::Fail(format!(
                    "statement #{} dataset action `{action:?}`: {error}",
                    index + 1
                ));
            }
            index += 1;
            continue;
        }
        let key = statement.conn_name.clone().unwrap_or_default();
        let connection = connections
            .entry(key)
            .or_insert_with(|| configured_connection(&db));
        match execute_checked_statement(connection, statement, env, index, timing) {
            Ok(true) => aborted_group = statement.line_group,
            Ok(false) => {}
            Err(error) => return Outcome::Fail(error),
        }
        index += 1;
    }
    Outcome::Pass
}

/// Execute a `-MULTI_COPY_RANDOM` action: split the source CSV's rows into
/// `splits` slices (even slices — split points never change the logical result)
/// and `COPY` each slice into the table sequentially.
fn run_multi_copy(
    conn: &koko::Connection,
    spec: &MultiCopySpec,
    env: CorpusEnv,
) -> Result<(), String> {
    let source = expand_corpus_vars(&spec.source, &env);
    let content = std::fs::read_to_string(source.as_ref())
        .map_err(|e| format!("reading multi-copy source {source}: {e}"))?;
    let rows: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let dir = std::env::temp_dir();
    let chunk = rows.len().div_ceil(spec.splits.max(1));
    for (k, slice) in rows.chunks(chunk.max(1)).enumerate() {
        let path = dir.join(format!(
            "koko_multicopy_{}_{}_{k}.csv",
            std::process::id(),
            spec.table
        ));
        std::fs::write(&path, slice.join("\n"))
            .map_err(|e| format!("writing multi-copy slice: {e}"))?;
        let res = conn.query(&format!("COPY {} FROM \"{}\"", spec.table, path.display()));
        let _ = std::fs::remove_file(&path);
        res.map_err(|e| format!("multi-copy slice #{k} failed: {e}"))?;
    }
    Ok(())
}

/// Run one statement and check its expectation. `Ok(true)` = the expectation
/// held but the ENGINE returned an error (an expected error) — a multi-
/// statement line stops after it, like the C++ result chain.
fn run_statement(
    conn: &koko::Connection,
    stmt: &TestStatement,
    env: CorpusEnv,
) -> Result<bool, String> {
    if let Some(file) = &stmt.batch_file {
        let root = env
            .root
            .ok_or_else(|| format!("batch statement file `{file}` requires KOKO_ROOT_DIRECTORY"))?;
        let path = root.join("test/statements").join(file);
        let content = std::fs::read_to_string(&path)
            .map_err(|error| format!("reading batch statement file {}: {error}", path.display()))?;
        let mut engine_errored = false;
        for (index, query) in content.lines().enumerate() {
            let mut expanded = stmt.clone();
            expanded.query = query.to_string();
            expanded.batch_file = None;
            engine_errored |= run_statement(conn, &expanded, env)
                .map_err(|error| format!("batch line {}: {error}", index + 1))?;
        }
        return Ok(engine_errored);
    }
    if let Some(spec) = &stmt.multi_copy {
        return run_multi_copy(conn, spec, env).map(|()| false);
    }
    // Resolve a `<FILE>:name` expected result into inline rows from the answer file.
    let resolved: Expected;
    let expected: &Expected = match &stmt.expected {
        Expected::RowsFile(name) => {
            let dir = env.answers_dir.ok_or_else(|| {
                format!("answer file `{name}` referenced but no answers dir is configured")
            })?;
            let path = dir.join(name);
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading answer file {}: {e}", path.display()))?;
            resolved = Expected::Rows(content.lines().map(|l| l.trim_end().to_string()).collect());
            &resolved
        }
        other => other,
    };
    let params: Vec<(&str, Value)> = stmt
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    // Substitute `${KOKO_ROOT_DIRECTORY}` so explicit COPY/LOAD paths resolve.
    let query = expand_corpus_vars(&stmt.query, &env);
    let result = conn.query_with_params(&query, &params);
    let engine_errored = result.is_err();
    let outcome: Result<(), String> = match (expected, result) {
        (Expected::Ok, Ok(_)) => Ok(()),
        (Expected::Ok, Err(e)) => Err(format!("expected success, got error: {e}")),
        (Expected::Error(expected), Err(e)) => {
            let expected = expand_corpus_vars(expected, &env);
            let actual = e.to_string();
            // Both sides right-trimmed, matching the C++ runner (`StringUtils::rtrim`).
            if actual.trim_end() == expected.as_ref().trim_end() {
                Ok(())
            } else {
                Err(format!("expected error `{expected}`, got `{actual}`"))
            }
        }
        (Expected::Error(expected), Ok(_)) => Err(format!(
            "expected error `{}`, but statement succeeded",
            expand_corpus_vars(expected, &env)
        )),
        (Expected::ErrorRegex(pat), Err(e)) => {
            let expanded = expand_corpus_vars(pat, &env);
            let pat = escape_unexpanded_regex_vars(&expanded);
            let actual = e.to_string();
            let actual = actual.trim_end();
            // C++ `std::regex_match` is anchored — the whole error must match.
            let re = regex::Regex::new(&format!(r"\A(?:{pat})\z"))
                .map_err(|err| format!("invalid error regex `{pat}`: {err}"))?;
            if re.is_match(actual) {
                Ok(())
            } else {
                Err(format!("error `{actual}` does not match regex `{pat}`"))
            }
        }
        (Expected::ErrorRegex(pat), Ok(_)) => {
            let expanded = expand_corpus_vars(pat, &env);
            let pat = escape_unexpanded_regex_vars(&expanded);
            Err(format!(
                "expected error matching regex `{pat}`, but statement succeeded"
            ))
        }
        (Expected::Rows(expected), Ok(result)) => compare_rows(expected, &result, stmt),
        (Expected::Rows(_), Err(e)) => Err(format!("expected rows, got error: {e}")),
        (Expected::Hash { count, md5 }, Ok(result)) => {
            let mut rows = actual_rows(&result, stmt);
            // The hashed bytes are the rows in compare order: sorted unless
            // `-CHECK_ORDER` (the C++ runner hashes post-sort).
            if !stmt.check_order {
                rows.sort();
            }
            if rows.len() != *count {
                return Err(format!(
                    "expected {count} hashed row(s), got {}",
                    rows.len()
                ));
            }
            let mut hasher = md5::Md5::new();
            for row in &rows {
                hasher.update(row.as_bytes());
                hasher.update(b"\n");
            }
            let digest = hasher.hex_digest();
            if digest == *md5 {
                Ok(())
            } else {
                Err(format!("result hash `{digest}` != expected `{md5}`"))
            }
        }
        (Expected::Hash { .. }, Err(e)) => Err(format!("expected rows, got error: {e}")),
        // A statement beyond its block count runs unchecked (C++ clamp).
        (Expected::Unchecked, _) => Ok(()),
        // RowsFile was resolved to Rows above.
        (Expected::RowsFile(_), _) => unreachable!("RowsFile resolved to Rows above"),
    };
    outcome.map(|()| engine_errored)
}

/// The actual result rows in corpus format: `|`-joined cells, with the column-name
/// header row prepended under `-CHECK_COLUMN_NAMES` (it then participates in
/// count/sort/compare like a data row, exactly like the C++ runner).
fn actual_rows(result: &koko::QueryResult, stmt: &TestStatement) -> Vec<String> {
    let mut rows = result.to_result_strings();
    if stmt.check_column_names {
        rows.insert(0, result.column_names().join("|"));
    }
    rows
}

fn compare_rows(
    expected: &[String],
    result: &koko::QueryResult,
    stmt: &TestStatement,
) -> Result<(), String> {
    if stmt.check_precision {
        return compare_rows_precision(expected, result, stmt);
    }
    let mut actual = actual_rows(result, stmt);
    let mut expected = expected.to_vec();
    if actual.len() != expected.len() {
        return Err(format!(
            "expected {} row(s), got {}: {:?}",
            expected.len(),
            actual.len(),
            actual
        ));
    }
    if !stmt.check_order {
        actual.sort();
        expected.sort();
    }
    for (a, e) in actual.iter().zip(expected.iter()) {
        if a != e {
            return Err(format!("row mismatch: expected `{e}`, got `{a}`"));
        }
    }
    Ok(())
}

/// `-CHECK_PRECISION` comparison, mirroring the C++ runner's `checkResultNumeric`:
/// requires `-CHECK_ORDER` (hard error in C++); FLOAT/DOUBLE cells compare with a
/// type-specific 1-ULP tolerance against the *raw* value; every other cell must
/// match exactly as a string.
fn compare_rows_precision(
    expected: &[String],
    result: &koko::QueryResult,
    stmt: &TestStatement,
) -> Result<(), String> {
    if !stmt.check_order {
        return Err("CHECK_ORDER MUST BE ENABLED FOR CHECK_PRECISION".to_string());
    }
    let nrows = result.num_rows();
    let ncols = result.num_columns();
    let header = usize::from(stmt.check_column_names);
    if expected.len() != nrows + header {
        return Err(format!(
            "expected {} row(s), got {}",
            expected.len(),
            nrows + header
        ));
    }
    if stmt.check_column_names {
        let names = result.column_names().join("|");
        if expected[0] != names {
            return Err(format!(
                "column-name row mismatch: expected `{}`, got `{names}`",
                expected[0]
            ));
        }
    }
    for r in 0..nrows {
        let exp_cells: Vec<&str> = expected[r + header].split('|').collect();
        if exp_cells.len() != ncols {
            return Err(format!(
                "row {} has {} field(s), expected {}",
                r + 1,
                ncols,
                exp_cells.len()
            ));
        }
        for (c, expected_cell) in exp_cells.iter().enumerate() {
            let cell = result
                .value(r, c)
                .map_err(|error| format!("cannot access result cell ({r}, {c}): {error}"))?;
            let ok = match &cell {
                Value::Float(x) => expected_cell
                    .parse::<f32>()
                    .is_ok_and(|y| precision_equal_f32(*x, y)),
                Value::Double(x) => expected_cell
                    .parse::<f64>()
                    .is_ok_and(|y| precision_equal_f64(*x, y)),
                value => value.to_result_string() == *expected_cell,
            };
            if !ok {
                return Err(format!(
                    "row {} field {}: expected `{}`, got `{}`",
                    r + 1,
                    c + 1,
                    expected_cell,
                    cell.to_result_string()
                ));
            }
        }
    }
    Ok(())
}

/// C++ `precisionEqual<T>`: `|x−y| ≤ ldexp(ε, ilogb(min(|x|,|y|)))` — a 1-ULP
/// tolerance at the smaller operand's binade (subnormals clamp to the smallest
/// normal exponent − 1).
fn precision_equal_f64(x: f64, y: f64) -> bool {
    let m = x.abs().min(y.abs());
    let exp = if m < f64::MIN_POSITIVE {
        f64::MIN_EXP - 1
    } else {
        ((m.to_bits() >> 52) & 0x7ff) as i32 - 1023
    };
    (x - y).abs() <= f64::EPSILON * (exp as f64).exp2()
}

fn precision_equal_f32(x: f32, y: f32) -> bool {
    let m = x.abs().min(y.abs());
    let exp = if m < f32::MIN_POSITIVE {
        f32::MIN_EXP - 1
    } else {
        ((m.to_bits() >> 23) & 0xff) as i32 - 127
    };
    (x - y).abs() <= f32::EPSILON * (exp as f32).exp2()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Walk back to a UTF-8 char boundary so byte-slicing a statement that
        // contains multibyte characters (e.g. the emoji/accented strings in
        // `ddl_empty.test`) never panics.
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_run_inline() {
        let content = "\
-DATASET CSV empty
--
-CASE Basic
-STATEMENT CREATE NODE TABLE P(name STRING, age INT64, PRIMARY KEY(name))
---- ok
-STATEMENT CREATE (:P {name: 'Alice', age: 35})
---- ok
-STATEMENT MATCH (p:P) RETURN p.name, p.age
---- 1
Alice|35
-STATEMENT MATCH (p:P) WHERE p.age < 0 RETURN p.name
---- 0
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.dataset, "empty");
        assert_eq!(file.cases.len(), 1);
        let results = run_test_file(&file, None);
        assert_eq!(results[0].outcome, Outcome::Pass, "{:?}", results[0]);
    }

    #[test]
    fn detects_mismatch_and_error() {
        let content = "\
-DATASET CSV empty
--
-CASE Err
-STATEMENT MATCH (p:Missing) RETURN p
---- error
Binder exception: Table Missing does not exist.
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn split_statements_quotes_and_trailing() {
        // Statements keep their `;` terminator (the engine quotes them verbatim
        // in decorated parser errors, like the C++ connection).
        assert_eq!(split_statements("RETURN 1"), vec!["RETURN 1"]);
        assert_eq!(split_statements("RETURN 1;"), vec!["RETURN 1;"]);
        assert_eq!(split_statements("A; B; C"), vec!["A;", "B;", "C"]);
        // A `;` inside a string literal is not a separator.
        assert_eq!(split_statements("RETURN 'a;b'"), vec!["RETURN 'a;b'"]);
        assert_eq!(
            split_statements("RETURN 'a;b'; RETURN 2"),
            vec!["RETURN 'a;b';", "RETURN 2"]
        );
        // An escaped quote keeps the string open.
        assert_eq!(split_statements(r"RETURN 'a\';b'"), vec![r"RETURN 'a\';b'"]);
    }

    #[test]
    fn multi_statement_block_runs() {
        // One `-STATEMENT` packing three `;`-separated statements + three blocks.
        let content = "\
-DATASET CSV empty
--
-CASE Multi
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id));
           CREATE (:P {id: 1});
           MATCH (p:P) RETURN p.id
---- ok
---- ok
---- 1
1
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.cases[0].statements.len(), 3);
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn conn_prefix_extraction() {
        assert_eq!(
            extract_conn_prefix("[conn1] RETURN 1"),
            (Some("conn1".to_string()), "RETURN 1".to_string())
        );
        // A statement that genuinely starts with a list literal is left intact
        // (the bracket content is not an identifier).
        assert_eq!(
            extract_conn_prefix("[1, 2] AS xs"),
            (None, "[1, 2] AS xs".to_string())
        );
        assert_eq!(
            extract_conn_prefix("RETURN [1, 2, 3]"),
            (None, "RETURN [1, 2, 3]".to_string())
        );
    }

    #[test]
    fn multi_connection_isolation() {
        // `[conn1]`/`[conn2]` route to independent connections over one database:
        // conn2 sees conn1's write only after COMMIT (snapshot isolation, asserted
        // through the runner). The unprefixed setup runs on the default connection.
        let content = "\
-DATASET CSV empty
--
-CASE MultiConn
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-CREATE_CONNECTION conn1
-CREATE_CONNECTION conn2
-STATEMENT [conn1] BEGIN
---- ok
-STATEMENT [conn1] CREATE (:P {id: 1})
---- ok
-STATEMENT [conn2] MATCH (p:P) RETURN count(*)
---- 1
0
-STATEMENT [conn1] COMMIT
---- ok
-STATEMENT [conn2] MATCH (p:P) RETURN count(*)
---- 1
1
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(
            file.cases[0].statements[1].conn_name.as_deref(),
            Some("conn1")
        );
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn statement_block_expand() {
        let content = "\
-DATASET CSV empty
--
-DEFINE_STATEMENT_BLOCK SETUP [
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-STATEMENT CREATE (:P {id: 1})
---- ok
]
-CASE UsesBlock
-INSERT_STATEMENT_BLOCK SETUP
-STATEMENT MATCH (p:P) RETURN count(*)
---- 1
1
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.cases[0].statements.len(), 3); // 2 from the block + 1
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn reloaddb_is_a_noop_in_memory() {
        // `-RELOADDB` is dropped (the case continues; data is still in RAM).
        let content = "\
-DATASET CSV empty
--
-CASE Reload
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-STATEMENT CREATE (:P {id: 1})
---- ok
-RELOADDB
-STATEMENT MATCH (p:P) RETURN count(*)
---- 1
1
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.cases[0].statements.len(), 3);
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn unresolved_corpus_variables_are_literal_in_error_regexes() {
        assert_eq!(
            escape_unexpanded_regex_vars(
                r"^File ${KOKO_EXPORT_DB_DIRECTORY}[/\\]schema\.cypher does not exist\.$"
            ),
            r"^File \$\{KOKO_EXPORT_DB_DIRECTORY\}[/\\]schema\.cypher does not exist\.$"
        );
    }

    #[test]
    fn import_database_directive_starts_from_an_empty_database() {
        let content = "\
-DATASET CSV empty
--
-CASE ImportReset
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-IMPORT_DATABASE ignored-native-path
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.cases[0].statements.len(), 3);
        assert!(file.cases[0].statements[1].reset_database);
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }

    #[test]
    fn skip_in_mem_skips_but_loops_execute() {
        let content = "\
-DATASET CSV empty
--
-CASE InMem
-SKIP_IN_MEM
-STATEMENT RETURN 1
---- 1
1
-CASE Loopy
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-LOOP i 1 3
-STATEMENT CREATE (:P {id: ${i}})
---- ok
-ENDLOOP
-STATEMENT MATCH (p:P) RETURN count(*)
---- 1
3
";
        let file = parse_test_file(content).unwrap();
        let results = run_test_file(&file, None);
        assert_eq!(
            results[0].outcome,
            Outcome::Skip("-SKIP_IN_MEM".to_string())
        );
        assert_eq!(results[1].outcome, Outcome::Pass);
        assert_eq!(file.cases[1].statements.len(), 5);
    }

    #[test]
    fn answer_file_reference_parses() {
        // `---- N` followed by `<FILE>:name` stores the answer filename, consuming
        // only that one line (the N rows live in the file, resolved at run time).
        let content = "\
-DATASET CSV empty
--
-CASE FileRef
-STATEMENT MATCH (n) RETURN n
---- 3
<FILE>:expected.txt
-STATEMENT RETURN 1
---- 1
1
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(
            file.cases[0].statements[0].expected,
            Expected::RowsFile("expected.txt".to_string())
        );
        // The `<FILE>:` line was consumed; the next statement parses normally.
        assert_eq!(file.cases[0].statements.len(), 2);
    }

    #[test]
    fn expands_environment_backed_corpus_variables() {
        let path = std::env::var("PATH").expect("test process has PATH");
        assert_eq!(
            substitute_vars("${PATH}/fixture", &HashMap::new()),
            format!("{path}/fixture")
        );
        assert_eq!(
            substitute_vars("${KOKO_UNKNOWN_TEST_VARIABLE}/fixture", &HashMap::new()),
            "${KOKO_UNKNOWN_TEST_VARIABLE}/fixture"
        );
    }

    #[test]
    fn remove_file_directive_runs_before_the_next_statement() {
        let path = std::env::temp_dir().join(format!(
            "koko-runner-remove-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"remove me").unwrap();
        let content = format!(
            "-DATASET CSV empty\n--\n-CASE Remove\n-REMOVE_FILE \"{}\"\n-STATEMENT RETURN 1\n---- 1\n1\n",
            path.display()
        );
        let file = parse_test_file(&content).unwrap();
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
        assert!(!path.exists());
    }

    #[test]
    fn expands_root_directory_var() {
        let env = CorpusEnv {
            answers_dir: None,
            root: Some(Path::new("/repo")),
        };
        assert_eq!(
            expand_corpus_vars("COPY t FROM \"${KOKO_ROOT_DIRECTORY}/dataset/x.csv\"", &env),
            "COPY t FROM \"/repo/dataset/x.csv\""
        );
        // Nothing to expand → the input is borrowed unchanged.
        assert!(matches!(
            expand_corpus_vars("RETURN 1", &env),
            Cow::Borrowed(_)
        ));
        // No root configured → unchanged even with the variable present.
        assert!(matches!(
            expand_corpus_vars("FROM \"${KOKO_ROOT_DIRECTORY}/x\"", &CorpusEnv::default()),
            Cow::Borrowed(_)
        ));
    }
    #[test]
    fn oracle_pcg32_matches_cpp_fixed_seed() {
        let mut random = OraclePcg32::default();
        random.seed(1_234_567_890);
        let values: Vec<u32> = (0..1_000).map(|_| random.bounded(1_000_000)).collect();
        assert_eq!(&values[..5], &[48_140, 278_343, 960_365, 204_732, 723_011]);
        assert_eq!(
            values.iter().map(|value| u64::from(*value)).sum::<u64>(),
            502_790_265
        );
    }

    #[test]
    fn batch_statement_file_executes_each_line() {
        let root = std::env::temp_dir().join(format!(
            "koko-runner-batch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let statements = root.join("test/statements");
        std::fs::create_dir_all(&statements).unwrap();
        std::fs::write(
            statements.join("batch.cypher"),
            "CREATE (:P {id: 1})\nCREATE (:P {id: 2})\n",
        )
        .unwrap();
        let content = "\
-DATASET CSV empty
--
-CASE Batch
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-BATCH_STATEMENTS <FILE:>batch.cypher
---- ok
-STATEMENT MATCH (p:P) RETURN count(*)
---- 1
2
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(
            file.cases[0].statements[1].batch_file.as_deref(),
            Some("batch.cypher")
        );
        let results = run_test_file_with(
            &file,
            None,
            CorpusEnv {
                answers_dir: None,
                root: Some(&root),
            },
        );
        assert_eq!(results[0].outcome, Outcome::Pass);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_dataset_schema_and_row_loading_execute() {
        let root = std::env::temp_dir().join(format!(
            "koko-runner-dataset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dataset = root.join("mini");
        std::fs::create_dir_all(&dataset).unwrap();
        std::fs::write(
            dataset.join("schema.cypher"),
            "CREATE NODE TABLE P(id INT64, name STRING, PRIMARY KEY(id));\n",
        )
        .unwrap();
        std::fs::write(dataset.join("copy.cypher"), "COPY P FROM \"P.csv\";\n").unwrap();
        std::fs::write(dataset.join("P.csv"), "1,Alice\n2,Bob\n").unwrap();
        let content = "\
-DATASET CSV empty
--
-CASE ManualDataset
-CREATE_DATASET_SCHEMA mini
-INSERT_DATASET_BY_ROW mini
-STATEMENT MATCH (p:P) RETURN p.id, p.name ORDER BY p.id
---- 2
1|Alice
2|Bob
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(
            file.cases[0].statements[0].dataset_action,
            Some(DatasetAction::CreateSchema("mini".to_string()))
        );
        assert_eq!(run_test_file(&file, Some(&root))[0].outcome, Outcome::Pass);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_execution_block_runs_each_connection_queue() {
        let content = "\
-DATASET CSV empty
--
-CASE Concurrent
-STATEMENT CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))
---- ok
-STATEMENT CALL debug_enable_multi_writes=true
---- ok
-CREATE_CONNECTION conn2
-BEGIN_CONCURRENT_EXECUTION
-STATEMENT CREATE (:P {id: 1})
---- ok
-STATEMENT [conn2] CREATE (:P {id: 2})
---- ok
-END_CONCURRENT_EXECUTION
-STATEMENT MATCH (p:P) RETURN count(*)
---- 1
2
";
        let file = parse_test_file(content).unwrap();
        assert_eq!(file.cases[0].statements[2].concurrent_group, Some(0));
        assert_eq!(file.cases[0].statements[3].concurrent_group, Some(0));
        assert_eq!(run_test_file(&file, None)[0].outcome, Outcome::Pass);
    }
}
