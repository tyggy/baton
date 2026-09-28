//! Parity helper: reads JSON strings (one per line) on stdin and prints
//! `pymd::markdown` of each as a JSON string per line. With `--paths`,
//! prints `[absolute path matches, relative path matches]` instead (the
//! attachment scanners, checked against `_PATH_RE` / `_REL_PATH_RE`).

use std::io::{BufRead, Write};

fn main() {
    let paths = std::env::args().any(|a| a == "--paths");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        let src: String = serde_json::from_str(&line).expect("json string");
        let v = if paths {
            use baton_core::read::messages::{path_matches, rel_matches};
            serde_json::json!([path_matches(&src), rel_matches(&src)])
        } else {
            serde_json::Value::String(baton_core::pymd::markdown(&src))
        };
        writeln!(out, "{v}").expect("stdout");
    }
}
