//! Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) parsing —
//! a line-for-line port of `sessions_core._CodexParseState`.

use crate::pyfmt::{head, is_meaningful_message, split_ws, strip, truthy};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

struct Fail;

#[derive(Default)]
pub struct CodexState {
    first_user_msg: Option<String>,
    last_user_msg: Option<String>,
    last_meaningful_msg: Option<String>,
    first_ts: Option<Value>,
    last_ts: Option<Value>,
    last_assistant_ts: Option<Value>,
    msg_count: u64,
    user_msg_count: u64,
    assistant_msg_count: u64,
    session_id: Option<Value>,
    parent_thread_id: Option<Value>,
    is_subagent: bool,
    cwd: Option<Value>,
    model: Option<Value>,
    first_user_msgs: Vec<String>,
    last_user_msgs: Vec<String>,
    n_user_msgs: u64,
    search_chunks: Vec<String>,
    file_terms: BTreeSet<String>,
    failed: bool,
}

/// `a or b` on optional JSON values.
fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    a.filter(|v| truthy(v)).or(b.filter(|v| truthy(v)))
}

impl CodexState {
    pub fn new() -> Self {
        Self::default()
    }

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
        let record = record.as_object().ok_or(Fail)?;
        let ts = record.get("timestamp").cloned();
        if let Some(t) = ts.as_ref().filter(|t| truthy(t)) {
            if self.first_ts.is_none() {
                self.first_ts = Some(t.clone());
            }
            self.last_ts = Some(t.clone());
        }
        let rtype = record.get("type").and_then(Value::as_str);
        if !matches!(
            rtype,
            Some("session_meta" | "turn_context" | "response_item")
        ) {
            return Ok(()); // Python never touches payload for other record types
        }
        // payload = record.get("payload") or {}  (.get on a truthy non-dict raised)
        let empty = serde_json::Map::new();
        let payload = match record.get("payload") {
            Some(v) if truthy(v) => v.as_object().ok_or(Fail)?,
            _ => &empty,
        };
        match rtype {
            Some("session_meta") => {
                if let Some(v) = or(payload.get("id"), payload.get("session_id")) {
                    self.session_id = Some(v.clone());
                }
                if let Some(v) = payload.get("parent_thread_id").filter(|v| truthy(v)) {
                    self.parent_thread_id = Some(v.clone());
                }
                if let Some(Value::Object(src)) = payload.get("source") {
                    if src.contains_key("subagent") {
                        self.is_subagent = true;
                    }
                }
                if let Some(v) = payload.get("cwd").filter(|v| truthy(v)) {
                    self.cwd = Some(v.clone());
                }
            }
            Some("turn_context") => {
                if let Some(m) = or(payload.get("model"), payload.get("model_name")) {
                    if !self.model.as_ref().is_some_and(truthy) {
                        self.model = Some(m.clone());
                    }
                }
            }
            Some("response_item") => {
                let pt = payload.get("type").and_then(Value::as_str);
                match pt {
                    Some("message") => {
                        let role = payload.get("role").and_then(Value::as_str);
                        let mut parts: Vec<String> = Vec::new();
                        // `for block in payload.get("content") or []`
                        match payload.get("content") {
                            Some(Value::Array(blocks)) => {
                                for b in blocks {
                                    if let Value::Object(o) = b {
                                        if let Some(t) = o.get("text") {
                                            if truthy(t) {
                                                parts.push(t.as_str().ok_or(Fail)?.to_string());
                                            } else {
                                                parts.push(String::new());
                                            }
                                        }
                                    }
                                }
                            }
                            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => return Err(Fail), // iterating an int
                            Some(Value::Bool(true)) => return Err(Fail),
                            _ => {} // falsy → [], dict/str iterate keys/chars → no dict blocks
                        }
                        let content = strip(&parts.join(" ")).to_string();
                        if content.is_empty() {
                            return Ok(());
                        }
                        self.msg_count += 1;
                        if role == Some("user") && !content.starts_with("[Request interrupted") {
                            if self.first_user_msg.is_none() {
                                self.first_user_msg = Some(content.clone());
                            }
                            self.last_user_msg = Some(content.clone());
                            self.user_msg_count += 1;
                            self.n_user_msgs += 1;
                            if self.first_user_msgs.len() < 5 {
                                self.first_user_msgs.push(head(&content, 500).to_string());
                            }
                            self.last_user_msgs.push(head(&content, 500).to_string());
                            if self.last_user_msgs.len() > 3 {
                                self.last_user_msgs.remove(0);
                            }
                            if is_meaningful_message(&content) {
                                self.last_meaningful_msg = Some(content.clone());
                            }
                            self.search_chunks.push(head(&content, 200).to_lowercase());
                        } else if role == Some("assistant") {
                            self.assistant_msg_count += 1;
                            self.last_assistant_ts = ts;
                            self.search_chunks.push(head(&content, 200).to_lowercase());
                        }
                    }
                    Some("custom_tool_call") | Some("function_call") => {
                        let args = or(payload.get("arguments"), payload.get("input"))
                            .cloned()
                            .unwrap_or(json!(""));
                        match &args {
                            Value::Object(a) => {
                                for k in ["path", "file_path", "command"] {
                                    if let Some(Value::String(v)) = a.get(k) {
                                        if v.chars().count() < 200 {
                                            self.file_terms.insert(v.clone());
                                        }
                                    }
                                }
                            }
                            Value::String(s) => {
                                for tok in split_ws(s) {
                                    if tok.contains('/') && tok.chars().count() < 200 {
                                        self.file_terms.insert(
                                            tok.trim_matches(|c| "',\";".contains(c)).to_string(),
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn result(&self, path: &Path) -> Value {
        if self.failed || self.session_id.is_none() || self.first_ts.is_none() || self.is_subagent {
            return Value::Null;
        }
        let mut sample: Vec<Value> = self.first_user_msgs.iter().map(|s| json!(s)).collect();
        if self.n_user_msgs > 5 && !self.last_user_msgs.is_empty() {
            sample.push(json!(format!(
                "... ({} messages later) ...",
                self.n_user_msgs - 5
            )));
            sample.extend(self.last_user_msgs.iter().map(|s| json!(s)));
        }
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
        json!({
            "id": self.session_id,
            "path": path.to_string_lossy(),
            "project": path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or(""),
            "origin": "codex",
            "cwd": self.cwd,
            "first_msg": first,
            "last_msg": last,
            "first_ts": self.first_ts,
            "last_ts": self.last_ts,
            "last_assistant_ts": self.last_assistant_ts,
            "msg_count": self.msg_count,
            "user_msg_count": self.user_msg_count,
            "assistant_msg_count": self.assistant_msg_count,
            "model": self.model,
            "context_used": 0,
            "context_limit": 200_000,
            "context_pct": 0,
            "sample_msgs": sample,
            "search_index": strip(&search),
        })
    }
}
