//! Baton session engine. Parses Claude Code and Codex transcripts exactly
//! like the Python reference (`sessions_core.py`) and keeps per-file parse
//! state so an active transcript is re-parsed only from where it left off.

pub mod claude;
pub mod codex;
pub mod pyfmt;
pub mod sessions;

use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

enum State {
    Claude(claude::ClaudeState),
    Codex(codex::CodexState),
}

impl State {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Claude => State::Claude(claude::ClaudeState::new()),
            Kind::Codex => State::Codex(codex::CodexState::new()),
        }
    }
    fn feed_line(&mut self, line: &str) {
        match self {
            State::Claude(s) => s.feed_line(line),
            State::Codex(s) => s.feed_line(line),
        }
    }
    fn result(&self, path: &Path) -> Value {
        match self {
            State::Claude(s) => s.result(path),
            State::Codex(s) => s.result(path),
        }
    }
}

const CHUNK: usize = 4 << 20;

/// Feed complete lines from `offset` to EOF in bounded chunks. Returns the
/// bytes consumed and whatever partial line remains; `None` when the bytes
/// aren't UTF-8 (Python's text-mode read raised → the file parsed to None).
fn feed_from(
    state: &mut State,
    path: &Path,
    offset: u64,
) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut consumed = 0u64;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let Some(cut) = pending.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&pending[..cut]) else {
            return Ok(None);
        };
        for line in text.split('\n') {
            state.feed_line(line);
        }
        consumed += cut as u64;
        pending.drain(..cut);
    }
    Ok(Some((consumed, pending)))
}

/// Full parse, as `parse_session` / `parse_codex_session` do (the final
/// line counts even without a trailing newline).
pub fn parse_file(path: &Path, kind: Kind) -> Value {
    if kind == Kind::Claude
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("agent-"))
    {
        return Value::Null;
    }
    let mut state = State::new(kind);
    match feed_from(&mut state, path, 0) {
        Ok(Some((_, rest))) => {
            if !rest.is_empty() {
                match std::str::from_utf8(&rest) {
                    Ok(t) => state.feed_line(t),
                    Err(_) => return Value::Null,
                }
            }
            state.result(path)
        }
        _ => Value::Null,
    }
}

struct Entry {
    ino: u64,
    offset: u64,
    state: State,
}

/// Incremental parser: keeps each file's state and byte offset; a call
/// parses only the complete lines appended since the last one. Starts over
/// when the file was replaced or truncated.
#[derive(Default)]
pub struct Tracker {
    files: HashMap<PathBuf, Entry>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(&mut self, path: &Path, kind: Kind) -> Value {
        let Ok(meta) = std::fs::metadata(path) else {
            return Value::Null;
        };
        let fresh = match self.files.get(path) {
            Some(e) => e.ino != meta.ino() || meta.len() < e.offset,
            None => true,
        };
        if fresh {
            self.files.insert(
                path.to_path_buf(),
                Entry {
                    ino: meta.ino(),
                    offset: 0,
                    state: State::new(kind),
                },
            );
        }
        let e = self.files.get_mut(path).expect("inserted above");
        match feed_from(&mut e.state, path, e.offset) {
            Ok(Some((consumed, _))) => {
                e.offset += consumed;
                e.state.result(path)
            }
            _ => {
                self.files.remove(path);
                parse_file(path, kind)
            }
        }
    }
}
