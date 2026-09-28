//! `/api/messages` building blocks: `_normalize_blocks`, attachment
//! detection (`_extract_attachments_from_raw`, `_attachment_for`), the
//! Claude line parser and Codex's `_codex_line_to_message` /
//! `_codex_line_to_tail_record`.

use super::assets::AssetPolicy;
use super::paths::{basename, splitext_ext};
use super::{py_type_name, PyErr};
use crate::pyfmt::{head, len, strip, truthy};
use crate::pyjson::{dumps, dumps_unicode, py_str};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

const EXTS: [&str; 25] = [
    "md", "markdown", "txt", "png", "jpg", "jpeg", "gif", "webp", "heic", "pdf", "csv", "json",
    "yaml", "yml", "toml", "html", "htm", "swift", "py", "sh", "js", "ts", "tsx", "jsx", "css",
];

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// `[A-Za-z0-9_\-./]`
fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'/')
}

/// `[A-Za-z0-9_\-.]`
fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')
}

/// `\.(ext1|ext2|...)(?![A-Za-z0-9])` at `k` (which holds the dot):
/// the end of the extension, trying the alternatives in order.
fn ext_at(b: &[u8], k: usize) -> Option<usize> {
    if b.get(k) != Some(&b'.') {
        return None;
    }
    for e in EXTS {
        let e = e.as_bytes();
        let end = k + 1 + e.len();
        if b.len() >= end && &b[k + 1..end] == e && !b.get(end).is_some_and(|&c| is_alnum(c)) {
            return Some(end);
        }
    }
    None
}

/// Lazy `[A-Za-z0-9_\-./]+?\.ext(?!alnum)` from `s`: match end.
fn lazy_name_ext(b: &[u8], s: usize) -> Option<usize> {
    let mut k = s + 1;
    while k <= b.len() && is_path_char(b[k - 1]) {
        if let Some(e) = ext_at(b, k) {
            return Some(e);
        }
        if k == b.len() {
            break;
        }
        k += 1;
    }
    None
}

/// `_PATH_RE` matched at `p` (backtracking order of Python's `re`).
fn path_match_at(b: &[u8], p: usize) -> Option<usize> {
    if p > 0 && (is_alnum(b[p - 1]) || b[p - 1] == b'_') {
        return None;
    }
    // alt 1: ~/
    if b[p] == b'~' {
        if b.get(p + 1) == Some(&b'/') {
            return lazy_name_ext(b, p + 2);
        }
        return None;
    }
    if b[p] != b'/' {
        return None;
    }
    // alt 2: /[C]+/ (greedy, backtracking to earlier slashes)
    let mut run_end = p + 1;
    while run_end < b.len() && is_path_char(b[run_end]) {
        run_end += 1;
    }
    let mut j = run_end;
    while j > p + 2 {
        j -= 1;
        if b[j] == b'/' {
            if let Some(e) = lazy_name_ext(b, j + 1) {
                return Some(e);
            }
        }
    }
    // alt 3: /
    lazy_name_ext(b, p + 1)
}

/// `_PATH_RE.finditer(txt)`: matched strings.
pub fn path_matches(txt: &str) -> Vec<&str> {
    let b = txt.as_bytes();
    let mut out = Vec::new();
    let mut p = 0;
    while p < b.len() {
        if b[p] == b'/' || b[p] == b'~' {
            if let Some(e) = path_match_at(b, p) {
                out.push(&txt[p..e]);
                p = e;
                continue;
            }
        }
        p += 1;
    }
    out
}

/// `_REL_PATH_RE` matched at `p`: end of group 1.
fn rel_match_at(b: &[u8], p: usize) -> Option<usize> {
    if p > 0 && (is_name_char(b[p - 1]) || b[p - 1] == b'~' || b[p - 1] == b'/') {
        return None;
    }
    // components: [A-Za-z0-9_][A-Za-z0-9_\-.]*/
    let mut ends: Vec<usize> = Vec::new();
    let mut q = p;
    loop {
        if q >= b.len() || !(is_alnum(b[q]) || b[q] == b'_') {
            break;
        }
        let mut r = q + 1;
        while r < b.len() && is_name_char(b[r]) {
            r += 1;
        }
        if r < b.len() && b[r] == b'/' {
            ends.push(r + 1);
            q = r + 1;
        } else {
            break;
        }
    }
    for &start in ends.iter().rev() {
        // [A-Za-z0-9_\-.]+ greedy, then \.ext — backtrack to earlier dots
        let mut run = start;
        while run < b.len() && is_name_char(b[run]) {
            run += 1;
        }
        let mut k = run;
        while k > start + 1 {
            k -= 1;
            if let Some(e) = ext_at(b, k) {
                return Some(e);
            }
        }
    }
    None
}

/// `_REL_PATH_RE.finditer(txt)`: group 1 of each match.
pub fn rel_matches(txt: &str) -> Vec<&str> {
    let b = txt.as_bytes();
    let mut out = Vec::new();
    let mut p = 0;
    while p < b.len() {
        if is_alnum(b[p]) || b[p] == b'_' {
            if let Some(e) = rel_match_at(b, p) {
                out.push(&txt[p..e]);
                p = e;
                continue;
            }
        }
        p += 1;
    }
    out
}

/// `_ASSET_EXT_KIND`.
pub fn ext_kind(ext: &str) -> &'static str {
    match ext {
        ".md" | ".markdown" => "markdown",
        ".txt" => "text",
        ".png" | ".jpg" | ".jpeg" | ".gif" | ".webp" | ".heic" => "image",
        ".pdf" => "pdf",
        ".csv" => "csv",
        ".json" => "json",
        ".yaml" | ".yml" | ".toml" | ".html" | ".htm" | ".swift" | ".py" | ".sh" | ".js"
        | ".ts" | ".tsx" | ".jsx" | ".css" => "code",
        _ => "binary",
    }
}

/// Text-mode `for raw in f` with universal newlines: lines without their
/// terminator ("\n", "\r\n" or "\r").
pub fn universal_lines(s: &str) -> impl Iterator<Item = &str> {
    let b = s.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        if i >= b.len() {
            return None;
        }
        let start = i;
        while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
            i += 1;
        }
        let line = &s[start..i];
        if i < b.len() {
            if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
        }
        Some(line)
    })
}

/// `_attachment_for(path)`.
pub fn attachment_for(policy: &AssetPolicy, path: &str) -> Option<Value> {
    let safe = policy.path_under_allowed_root(path)?;
    let meta = std::fs::metadata(&safe).ok()?;
    if !meta.is_file() {
        return None;
    }
    let ext = splitext_ext(&safe).to_lowercase();
    let kind = ext_kind(&ext);
    let size = meta.len();
    let mut preview = String::new();
    if matches!(kind, "markdown" | "text" | "csv" | "json" | "code") && size <= 200_000 {
        if let Ok(bytes) = std::fs::read(&safe) {
            let text = String::from_utf8_lossy(&bytes);
            for raw in universal_lines(&text) {
                let s = strip(raw);
                if !s.is_empty() {
                    preview = head(s, 140).to_string();
                    break;
                }
            }
        }
    }
    let name = basename(&safe).to_string();
    Some(json!({
        "path": safe,
        "name": name,
        "kind": kind,
        "size": size,
        "preview_first_line": preview,
    }))
}

/// `os.path.join(cwd, rel)` for a relative `rel`.
fn path_join(a: &str, b: &str) -> String {
    if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

fn rstrip_punct(s: &str) -> &str {
    s.trim_end_matches([',', '.', ';', ':', ')', '"', '\''])
}

/// `_extract_attachments_from_raw(content, cwd)`.
pub fn extract_attachments(
    policy: &AssetPolicy,
    content: Option<&Value>,
    cwd: Option<&Value>,
) -> Vec<Value> {
    let cwd = cwd.filter(|c| truthy(c)).and_then(Value::as_str);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    let mut paths: HashSet<String> = HashSet::new();
    let mut consider = |raw: String, out: &mut Vec<Value>| {
        if !seen.insert(raw.clone()) {
            return;
        }
        let Some(att) = attachment_for(policy, &raw) else {
            return;
        };
        let p = att["path"].as_str().unwrap_or("").to_string();
        if paths.insert(p) {
            out.push(att);
        }
    };
    let mut scan = |txt: &str, out: &mut Vec<Value>| {
        if txt.is_empty() {
            return;
        }
        for m in path_matches(txt) {
            consider(rstrip_punct(m).to_string(), out);
        }
        if let Some(cwd) = cwd {
            for rel in rel_matches(txt) {
                consider(path_join(cwd, rstrip_punct(rel)), out);
            }
        }
    };
    match content {
        Some(Value::String(s)) => scan(s, &mut out),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                let Some(b) = b.as_object() else { continue };
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(Value::String(t)) = b.get("text") {
                            scan(t, &mut out);
                        }
                    }
                    Some("thinking") => {
                        if let Some(Value::String(t)) = b.get("thinking") {
                            scan(t, &mut out);
                        }
                    }
                    Some("tool_use") => match b.get("input") {
                        Some(Value::Object(inp)) => {
                            for v in inp.values() {
                                match v {
                                    Value::String(s) => scan(s, &mut out),
                                    Value::Array(xs) => {
                                        for x in xs {
                                            if let Value::String(s) = x {
                                                scan(s, &mut out);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Value::String(s)) => scan(s, &mut out),
                        _ => {}
                    },
                    Some("tool_result") => match b.get("content") {
                        Some(Value::String(s)) => scan(s, &mut out),
                        Some(Value::Array(subs)) => {
                            for sub in subs {
                                if let Some(sub) = sub.as_object() {
                                    if sub.get("type").and_then(Value::as_str) == Some("text") {
                                        if let Some(Value::String(t)) = sub.get("text") {
                                            scan(t, &mut out);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

fn attr_err(v: &Value, attr: &str) -> PyErr {
    PyErr(format!(
        "'{}' object has no attribute '{attr}'",
        py_type_name(v)
    ))
}

/// Python `(x or "")` for a value that must then be a str.
fn str_or_empty<'a>(v: Option<&'a Value>, attr: &str) -> Result<&'a str, PyErr> {
    match v {
        None => Ok(""),
        Some(v) if !truthy(v) => Ok(""),
        Some(Value::String(s)) => Ok(s),
        Some(v) => Err(attr_err(v, attr)),
    }
}

/// `_normalize_blocks(content)`.
pub fn normalize_blocks(content: Option<&Value>) -> Result<Vec<Value>, PyErr> {
    let mut out = Vec::new();
    let blocks = match content {
        Some(Value::String(s)) => {
            let s = strip(s);
            if !s.is_empty() && !s.starts_with("[Request interrupted") {
                out.push(json!({"type": "text", "text": s}));
            }
            return Ok(out);
        }
        Some(Value::Array(a)) => a,
        _ => return Ok(out),
    };
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        let bt = block.get("type").and_then(Value::as_str);
        match bt {
            Some("text") => {
                let t = str_or_empty(block.get("text"), "strip")?;
                if !strip(t).is_empty() {
                    out.push(json!({"type": "text", "text": t}));
                }
            }
            Some("thinking") => {
                let t = str_or_empty(block.get("thinking"), "strip")?;
                if !strip(t).is_empty() {
                    let mut cropped = head(t, 400).to_string();
                    if len(t) > 400 {
                        cropped.push('…');
                    }
                    out.push(json!({"type": "thinking", "text": cropped}));
                }
            }
            Some("tool_use") => {
                let name = match block.get("name") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => json!("?"),
                };
                let empty = Value::Object(Map::new());
                let inp = match block.get("input") {
                    Some(v) if truthy(v) => v,
                    _ => &empty,
                };
                let mut preview = String::new();
                if let Value::Object(o) = inp {
                    for k in [
                        "command",
                        "file_path",
                        "path",
                        "pattern",
                        "query",
                        "url",
                        "prompt",
                        "description",
                    ] {
                        if let Some(v) = o.get(k).filter(|v| truthy(v)) {
                            preview = strip(&py_str(v).replace('\n', " ")).to_string();
                            break;
                        }
                    }
                    if preview.is_empty() {
                        preview = head(&dumps_unicode(inp), 200).to_string();
                    }
                } else {
                    preview = head(&py_str(inp), 200).to_string();
                }
                let id = block.get("id").cloned().unwrap_or_else(|| json!(""));
                out.push(json!({
                    "type": "tool_use",
                    "name": name,
                    "preview": head(&preview, 300),
                    "id": id,
                }));
            }
            Some("tool_result") => {
                let text: String = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(subs)) => {
                        let mut parts: Vec<String> = Vec::new();
                        for sub in subs {
                            if let Some(sub) = sub.as_object() {
                                if sub.get("type").and_then(Value::as_str) == Some("text") {
                                    match sub.get("text") {
                                        Some(v) if !truthy(v) => parts.push(String::new()),
                                        None => parts.push(String::new()),
                                        Some(Value::String(s)) => parts.push(s.clone()),
                                        Some(v) => {
                                            return Err(PyErr(format!(
                                                "sequence item {}: expected str instance, {} found",
                                                parts.len(),
                                                py_type_name(v)
                                            )))
                                        }
                                    }
                                }
                            }
                        }
                        parts.join("\n")
                    }
                    _ => String::new(),
                };
                let mut text = strip(&text).to_string();
                if len(&text) > 800 {
                    text = format!("{}…", head(&text, 800));
                }
                if !text.is_empty() {
                    let tuid = block
                        .get("tool_use_id")
                        .cloned()
                        .unwrap_or_else(|| json!(""));
                    let is_error = block.get("is_error").is_some_and(truthy);
                    out.push(json!({
                        "type": "tool_result",
                        "text": text,
                        "tool_use_id": tuid,
                        "is_error": is_error,
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `bytes.strip()` (ASCII whitespace).
pub fn bstrip(b: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = b.iter().position(|c| !ws(c)).unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !ws(c))
        .map(|i| i + 1)
        .unwrap_or(start);
    &b[start..end.max(start)]
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `json.loads(bytes)`: UTF-8 (BOM allowed); `Err` with the message of
/// the UnicodeDecodeError Python would raise; `Ok(None)` for a
/// JSONDecodeError.
pub fn loads_bytes(b: &[u8]) -> Result<Option<Value>, PyErr> {
    let b = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(b);
    let s = match std::str::from_utf8(b) {
        Ok(s) => s,
        Err(e) => return Err(PyErr(utf8_error_message(b, &e))),
    };
    Ok(serde_json::from_str(s).ok())
}

/// CPython's `UnicodeDecodeError` text for UTF-8.
pub fn utf8_error_message(b: &[u8], e: &std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let first = b[pos];
    let (n, reason) = match e.error_len() {
        None => (b.len() - pos, "unexpected end of data"),
        Some(n) => {
            let invalid_start = matches!(first, 0x80..=0xc1 | 0xf5..=0xff);
            (
                n,
                if invalid_start {
                    "invalid start byte"
                } else {
                    "invalid continuation byte"
                },
            )
        }
    };
    if n <= 1 {
        format!("'utf-8' codec can't decode byte 0x{first:02x} in position {pos}: {reason}")
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {pos}-{}: {reason}",
            pos + n - 1
        )
    }
}

/// `parse_lines` inside `_send_session_messages`, on raw (bytes) lines.
pub fn parse_claude_lines<'a>(
    policy: &AssetPolicy,
    lines: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<Value>, PyErr> {
    let mut out = Vec::new();
    for raw in lines {
        let b = bstrip(raw);
        if b.is_empty() {
            continue;
        }
        if !contains(b, b"\"type\":\"user\"") && !contains(b, b"\"type\":\"assistant\"") {
            continue;
        }
        let Some(d) = loads_bytes(b)? else { continue };
        if let Some(m) = claude_message(policy, &d)? {
            out.push(m);
        }
    }
    Ok(out)
}

/// One parsed record → bubble message (or None).
pub fn claude_message(policy: &AssetPolicy, d: &Value) -> Result<Option<Value>, PyErr> {
    let Some(obj) = d.as_object() else {
        return Err(attr_err(d, "get"));
    };
    let line_type = obj.get("type");
    let lt = match line_type {
        Some(Value::String(s)) if s == "user" || s == "assistant" => s.as_str(),
        _ => return Ok(None),
    };
    let empty = Value::Object(Map::new());
    let msg = match obj.get("message") {
        Some(v) if truthy(v) => v,
        _ => &empty,
    };
    let Some(msg) = msg.as_object() else {
        return Err(attr_err(msg, "get"));
    };
    let content = msg.get("content");
    let blocks = normalize_blocks(content)?;
    if blocks.is_empty() {
        return Ok(None);
    }
    let role = match msg.get("role") {
        Some(v) if truthy(v) => match v {
            Value::String(s) => s.to_lowercase(),
            other => return Err(attr_err(other, "lower")),
        },
        _ => lt.to_string(),
    };
    let role = if role.is_empty() {
        lt.to_string()
    } else {
        role
    };
    let ts = match obj.get("timestamp") {
        Some(v) if truthy(v) => v.clone(),
        _ => json!(""),
    };
    let attachments = extract_attachments(policy, content, obj.get("cwd"));
    Ok(Some(json!({
        "role": role,
        "ts": ts,
        "blocks": blocks,
        "attachments": attachments,
    })))
}

/// `_codex_line_to_message(line)`.
pub fn codex_line_to_message(line: &[u8]) -> Result<Option<Value>, PyErr> {
    if !contains(line, b"\"response_item\"") {
        return Ok(None);
    }
    let Some(d) = loads_bytes(line)? else {
        return Ok(None);
    };
    let Some(obj) = d.as_object() else {
        return Err(attr_err(&d, "get"));
    };
    if obj.get("type").and_then(Value::as_str) != Some("response_item") {
        return Ok(None);
    }
    let empty = Value::Object(Map::new());
    let p = match obj.get("payload") {
        Some(v) if truthy(v) => v,
        _ => &empty,
    };
    let Some(p) = p.as_object() else {
        return Err(attr_err(p, "get"));
    };
    let ts = match obj.get("timestamp") {
        Some(v) if truthy(v) => v.clone(),
        _ => json!(""),
    };
    let pt = p.get("type").and_then(Value::as_str);
    let role = p.get("role").and_then(Value::as_str);
    if pt == Some("message") && matches!(role, Some("user" | "assistant")) {
        let mut parts: Vec<String> = Vec::new();
        let content = match p.get("content") {
            Some(v) if truthy(v) => v.clone(),
            _ => json!([]),
        };
        let items: Vec<Value> = match content {
            Value::Array(a) => a,
            Value::Object(o) => o.keys().map(|k| json!(k)).collect(),
            Value::String(s) => s.chars().map(|c| json!(c.to_string())).collect(),
            other => {
                return Err(PyErr(format!(
                    "'{}' object is not iterable",
                    py_type_name(&other)
                )))
            }
        };
        for b in &items {
            let Some(b) = b.as_object() else { continue };
            if matches!(
                b.get("type").and_then(Value::as_str),
                Some("input_text" | "output_text" | "text")
            ) {
                match b.get("text") {
                    None => parts.push(String::new()),
                    Some(Value::String(s)) => parts.push(s.clone()),
                    Some(v) => {
                        return Err(PyErr(format!(
                            "sequence item {}: expected str instance, {} found",
                            parts.len(),
                            py_type_name(v)
                        )))
                    }
                }
            }
        }
        let text = parts.join("\n");
        let text = strip(&text);
        if text.is_empty()
            || text.starts_with("<environment_context")
            || text.starts_with("<user_instructions")
            || text.starts_with("[Request interrupted")
        {
            return Ok(None);
        }
        return Ok(Some(json!({
            "role": role.expect("checked"),
            "ts": ts,
            "blocks": [{"type": "text", "text": text}],
            "attachments": [],
        })));
    }
    if matches!(pt, Some("function_call" | "custom_tool_call")) {
        let args = [p.get("arguments"), p.get("input")]
            .into_iter()
            .flatten()
            .find(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(""));
        let mut preview = args.clone();
        if let Value::String(a) = &args {
            if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(a) {
                // `parsed.get("command") or parsed.get("cmd")`
                let cmd = match parsed.get("command") {
                    Some(v) if truthy(v) => Some(v.clone()),
                    _ => parsed.get("cmd").cloned(),
                };
                preview = match cmd {
                    Some(Value::Array(items)) => {
                        let mut s = Vec::new();
                        for (i, it) in items.iter().enumerate() {
                            match it {
                                Value::String(x) => s.push(x.clone()),
                                other => {
                                    return Err(PyErr(format!(
                                        "sequence item {i}: expected str instance, {} found",
                                        py_type_name(other)
                                    )))
                                }
                            }
                        }
                        json!(s.join(" "))
                    }
                    Some(c) if truthy(&c) => c,
                    _ => args.clone(),
                };
            }
        }
        let name = match p.get("name") {
            Some(v) if truthy(v) => v.clone(),
            _ => json!("?"),
        };
        let preview = head(&py_str(&preview).replace('\n', " "), 300).to_string();
        let id = p.get("call_id").cloned().unwrap_or_else(|| json!(""));
        return Ok(Some(json!({
            "role": "assistant",
            "ts": ts,
            "attachments": [],
            "blocks": [{"type": "tool_use", "name": name, "preview": preview, "id": id}],
        })));
    }
    Ok(None)
}

/// `_codex_line_to_tail_record(line)`: a Claude-shaped record as
/// `json.dumps` bytes.
pub fn codex_line_to_tail_record(line: &[u8]) -> Result<Option<String>, PyErr> {
    let Some(m) = codex_line_to_message(line)? else {
        return Ok(None);
    };
    let mut content = Vec::new();
    for b in m["blocks"].as_array().expect("blocks") {
        if b["type"] == "tool_use" {
            let id = if truthy(&b["id"]) {
                b["id"].clone()
            } else {
                json!("")
            };
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": b["name"],
                "input": {"command": b.get("preview").cloned().unwrap_or_else(|| json!(""))},
            }));
        } else {
            content.push(json!({"type": "text", "text": b["text"]}));
        }
    }
    let rec = json!({
        "type": m["role"],
        "timestamp": m["ts"],
        "message": {"role": m["role"], "content": content},
    });
    Ok(Some(dumps(&rec)))
}
