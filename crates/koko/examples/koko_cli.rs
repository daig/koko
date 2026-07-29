//! Ad-hoc query CLI for differential testing against the C++ shell.
//!
//! Reads one Cypher statement per line from stdin; for each, prints the result
//! rows in the `.test` corpus format (`|`-joined cells), or `Error: <display>`,
//! followed by a `--KOKOSEP--` marker line. Set `KOKO_LOAD_DATASET=<dir>` to
//! load a CSV dataset (schema.cypher + copy.cypher) before reading stdin.

use koko::Database;
use koko::test_support::load_csv_dataset;
use std::io::{BufRead, Write};

fn main() {
    let db = Database::new();
    let conn = db.connect();
    if let Ok(dir) = std::env::var("KOKO_LOAD_DATASET") {
        if let Err(e) = load_csv_dataset(&conn, std::path::Path::new(&dir)) {
            eprintln!("dataset load failed: {e}");
            std::process::exit(2);
        }
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        // Keep the statement verbatim (trailing `;` included): decorated parser
        // errors quote the input, and the C++ shell's quoted text carries it.
        let stmt = line.trim();
        if stmt.is_empty() || stmt.starts_with("//") {
            continue;
        }
        match conn.execute(stmt) {
            Ok(result) => {
                let _ = write!(out, "{result}");
            }
            Err(e) => {
                let _ = writeln!(out, "Error: {e}");
            }
        }
        let _ = writeln!(out, "--KOKOSEP--");
    }
}
