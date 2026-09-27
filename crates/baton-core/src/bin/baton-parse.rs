//! Parity helper for bench/parity.py. Reads `claude|codex<TAB>path` lines on
//! stdin, prints one JSON result per line. With `--cuts N`, also checks that
//! incremental parsing equals a full parse at N random cut points of a copy.

use baton_core::{parse_file, Kind, Tracker};
use std::io::{BufRead, Write};

fn kind(s: &str) -> Kind {
    if s == "codex" {
        Kind::Codex
    } else {
        Kind::Claude
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cuts: usize = args
        .iter()
        .position(|a| a == "--cuts")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("baton-parse-{}", std::process::id()));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let Some((k, path)) = line.split_once('\t') else {
            continue;
        };
        let k = kind(k);
        let path = std::path::Path::new(path);
        let full = parse_file(path, k);
        let mut incr_ok = serde_json::Value::Null;
        if cuts > 0 {
            let data = std::fs::read(path).unwrap_or_default();
            let copy = tmp.with_file_name(format!(
                "{}.{}",
                tmp.file_name().unwrap().to_string_lossy(),
                path.file_name().unwrap().to_string_lossy()
            ));
            let mut points: Vec<usize> = (0..cuts)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    1 + (seed as usize) % data.len().max(1)
                })
                .collect();
            points.sort_unstable();
            points.push(data.len());
            let mut tr = Tracker::new();
            let mut ok = 0;
            for p in &points {
                std::fs::write(&copy, &data[..*p]).unwrap();
                if tr.parse(&copy, k) == parse_file(&copy, k) {
                    ok += 1;
                }
            }
            let _ = std::fs::remove_file(&copy);
            incr_ok = serde_json::json!([ok, points.len()]);
        }
        writeln!(
            out,
            "{}",
            serde_json::json!({"path": path, "result": full, "incr": incr_ok})
        )
        .unwrap();
    }
}
