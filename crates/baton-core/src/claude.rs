//! Claude Code transcript (`~/.claude/projects/<proj>/<session>.jsonl`)
//! parsing — a line-for-line port of `sessions_core._ParseState`.
//! Records are only ever appended, so the state can be fed incrementally.

use crate::pyfmt::{head, is_meaningful_message, len, py_int, round1, strip, truthy};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

const SEARCH_BUDGET: usize = 60_000;
const FILE_KEYS: [&str; 8] = [
    "file_path",
    "path",
    "notebook_path",
    "command",
    "pattern",
    "url",
    "old_string",
    "new_string",
];

/// A record Python's parser would have raised on. Python then returned
/// None for the whole file; `failed` is sticky for the same effect.
struct Fail;

#[derive(Default)]
pub struct ClaudeState {
    first_user_msg: Option<String>,
    last_user_msg: Option<String>,
    last_meaningful_msg: Option<String>,
    first_ts: Option<Value>,
    last_ts: Option<Value>,
    last_assistant_ts: Option<Value>,
    msg_count: u64,
    user_msg_count: u64,
    assistant_msg_count: u64,
    model: Option<Value>,
    last_usage: Option<Map<String, Value>>,
    first_user_msgs: Vec<String>,
    last_user_msgs: Vec<String>,
    n_user_msgs: u64,
    search_chunks: Vec<String>,
    search_len: usize,
    file_terms: BTreeSet<String>,
    failed: bool,
}

fn as_str(v: &Value) -> Result<&str, Fail> {
    v.as_str().ok_or(Fail)
}

impl ClaudeState {
    pub fn new() -> Self {
        Self::default()
    }

    fn add_search(&mut self, text: &Value) -> Result<(), Fail> {
        if !truthy(text) || self.search_len >= SEARCH_BUDGET {
            return Ok(());
        }
        let t = head(as_str(text)?, 1500).to_lowercase();
        self.search_len += len(&t);
        self.search_chunks.push(t);
        Ok(())
    }

    fn add_search_str(&mut self, text: &str) {
        if text.is_empty() || self.search_len >= SEARCH_BUDGET {
            return;
        }
        let t = head(text, 1500).to_lowercase();
        self.search_len += len(&t);
        self.search_chunks.push(t);
    }

    fn add_tool_terms(&mut self, inp: Option<&Value>) {
        let Some(Value::Object(inp)) = inp else {
            return;
        };
        for k in FILE_KEYS {
            if let Some(Value::String(v)) = inp.get(k) {
                if !v.is_empty() && self.file_terms.len() < 800 {
                    self.file_terms.insert(head(v, 200).to_lowercase());
                }
            }
        }
    }

    fn add_user_msg(&mut self, text: String) {
        self.n_user_msgs += 1;
        if self.first_user_msgs.len() < 5 {
            self.first_user_msgs.push(text.clone());
        }
        self.last_user_msgs.push(text);
        if self.last_user_msgs.len() > 3 {
            self.last_user_msgs.remove(0);
        }
    }

    /// Feed one line of the transcript (a trailing newline is fine).
    pub fn feed_line(&mut self, line: &str) {
        if self.failed {
            return;
        }
        let line = strip(line);
        if line.is_empty() {
            return;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if self.feed_record(&record).is_err() {
            self.failed = true;
        }
    }

    fn feed_record(&mut self, record: &Value) -> Result<(), Fail> {
        let record = record.as_object().ok_or(Fail)?; // record.get on a non-dict raised
        let ts = record.get("timestamp").cloned();
        if let Some(ts) = ts.as_ref().filter(|t| truthy(t)) {
            if self.first_ts.is_none() {
                self.first_ts = Some(ts.clone());
            }
            self.last_ts = Some(ts.clone());
        }
        let rtype = record.get("type");
        if rtype == Some(&json!("user")) {
            let msg = match record.get("message") {
                None => None,
                Some(Value::Object(m)) => Some(m),
                Some(_) => return Err(Fail), // msg.get on a non-dict
            };
            let raw = msg
                .and_then(|m| m.get("content"))
                .cloned()
                .unwrap_or_else(|| json!(""));
            let content: Value = if let Value::Array(blocks) = &raw {
                let mut parts: Vec<String> = Vec::new();
                for block in blocks {
                    match block {
                        Value::Object(b) if b.get("type") == Some(&json!("text")) => {
                            parts.push(as_str(b.get("text").unwrap_or(&json!("")))?.to_string());
                        }
                        Value::String(s) => parts.push(s.clone()),
                        Value::Object(b) if b.get("type") == Some(&json!("tool_result")) => {
                            let rc = b.get("content").cloned().unwrap_or_else(|| json!(""));
                            let rc = if let Value::Array(items) = &rc {
                                let mut out = Vec::new();
                                for it in items {
                                    if let Value::Object(o) = it {
                                        if o.get("type") == Some(&json!("text")) {
                                            out.push(
                                                as_str(o.get("text").unwrap_or(&json!("")))?
                                                    .to_string(),
                                            );
                                        }
                                    }
                                }
                                Value::String(out.join(" "))
                            } else {
                                rc
                            };
                            if let Value::String(s) = &rc {
                                self.add_search_str(head(s, 500));
                            }
                        }
                        _ => {}
                    }
                }
                Value::String(parts.join(" "))
            } else {
                raw
            };
            if truthy(&content) {
                let s = as_str(&content)?; // .startswith on a non-str raised
                if !s.starts_with("[Request interrupted") {
                    let s = s.to_string();
                    if self.first_user_msg.is_none() {
                        self.first_user_msg = Some(s.clone());
                    }
                    self.last_user_msg = Some(s.clone());
                    if is_meaningful_message(&s) {
                        self.last_meaningful_msg = Some(s.clone());
                    }
                    self.user_msg_count += 1;
                    self.add_user_msg(head(&s, 500).to_string());
                    self.add_search_str(&s);
                }
            }
            self.msg_count += 1;
        } else if rtype == Some(&json!("assistant")) {
            self.msg_count += 1;
            let msg_data = record.get("message");
            let msg_obj = msg_data.and_then(Value::as_object);
            let content = msg_obj.and_then(|m| m.get("content"));
            let mut meaningful = false;
            match content {
                Some(Value::Array(blocks)) if !blocks.is_empty() => {
                    if let Some(Value::Object(last)) = blocks.last() {
                        if last.get("type") == Some(&json!("text")) {
                            let t = last.get("text").cloned().unwrap_or(Value::Null);
                            if truthy(&t) && !strip(as_str(&t)?).is_empty() {
                                meaningful = true;
                            }
                        }
                    }
                    for block in blocks {
                        let Value::Object(b) = block else { continue };
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => self.add_search(b.get("text").unwrap_or(&json!("")))?,
                            Some("tool_use") => self.add_tool_terms(b.get("input")),
                            _ => {}
                        }
                    }
                }
                Some(Value::String(s)) if !strip(s).is_empty() => {
                    meaningful = true;
                    let s = s.clone();
                    self.add_search_str(&s);
                }
                _ => {}
            }
            if meaningful {
                self.assistant_msg_count += 1;
                if let Some(ts) = ts.filter(truthy) {
                    self.last_assistant_ts = Some(ts);
                }
            }
            if let Some(m) = msg_obj {
                if let Some(model) = m.get("model").filter(|v| truthy(v)) {
                    self.model = Some(model.clone());
                }
                if let Some(Value::Object(u)) = m.get("usage") {
                    self.last_usage = Some(u.clone());
                }
            }
        }
        Ok(())
    }

    /// The session dict, as `parse_session` returns it (None → `Value::Null`).
    pub fn result(&self, path: &Path) -> Value {
        if self.failed {
            return Value::Null;
        }
        self.try_result(path).unwrap_or(Value::Null)
    }

    fn try_result(&self, path: &Path) -> Result<Value, Fail> {
        let Some(first_ts) = &self.first_ts else {
            return Ok(Value::Null);
        };
        let mut sample: Vec<Value> = self.first_user_msgs.iter().map(|s| json!(s)).collect();
        if self.n_user_msgs > 5 && !self.last_user_msgs.is_empty() {
            sample.push(json!(format!(
                "... ({} messages later) ...",
                self.n_user_msgs - 5
            )));
            sample.extend(self.last_user_msgs.iter().map(|s| json!(s)));
        }
        let mut context_used: i64 = 0;
        let mut context_limit: i64 = 200_000;
        if let Some(u) = &self.last_usage {
            if !u.is_empty() {
                context_used = py_int(u.get("input_tokens")).map_err(|_| Fail)?
                    + py_int(u.get("cache_creation_input_tokens")).map_err(|_| Fail)?
                    + py_int(u.get("cache_read_input_tokens")).map_err(|_| Fail)?;
            }
        }
        if let Some(model) = &self.model {
            let m = as_str(model)?.to_lowercase();
            if m.contains("haiku") {
                context_limit = 200_000;
            } else if m.contains("opus")
                || m.contains("sonnet")
                || m.contains("[1m]")
                || m.contains("-1m")
            {
                context_limit = 1_000_000;
            }
        }
        if context_used > context_limit {
            context_limit = 1_000_000;
        }
        if context_used > context_limit {
            context_limit = context_used.max(1_000_000);
        }
        let context_pct = round1(context_used as f64 / context_limit as f64 * 100.0);

        let first = self
            .first_user_msg
            .clone()
            .unwrap_or_else(|| "(no message)".into());
        let last = self
            .last_meaningful_msg
            .clone()
            .or_else(|| self.last_user_msg.clone())
            .or_else(|| self.first_user_msg.clone())
            .unwrap_or_else(|| "(no message)".into());
        let terms: Vec<&str> = self.file_terms.iter().map(String::as_str).collect();
        let search = format!("{} {}", self.search_chunks.join(" "), terms.join(" "));
        Ok(json!({
            "id": path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
            "path": path.to_string_lossy(),
            "project": path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or(""),
            "origin": "claude",
            "first_msg": first,
            "last_msg": last,
            "first_ts": first_ts,
            "last_ts": self.last_ts,
            "last_assistant_ts": self.last_assistant_ts,
            "msg_count": self.msg_count,
            "user_msg_count": self.user_msg_count,
            "assistant_msg_count": self.assistant_msg_count,
            "model": self.model,
            "context_used": context_used,
            "context_limit": context_limit,
            "context_pct": context_pct,
            "sample_msgs": sample,
            "search_index": strip(&search),
        }))
    }
}
