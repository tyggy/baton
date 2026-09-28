//! `markdown.inlinepatterns` (+ nl2br) and the `InlineProcessor`
//! treeprocessor that applies them, with the same stash/placeholder
//! mechanics (placeholders, re-scanning, text linking) as Python.

use super::block::build;
use super::entities::CODEPOINT2NAME;
use super::tree::{truthy, Id, Txt};
use super::{is_py_word, Fail, Md, ETX, R, STX};
use crate::pyfmt::strip;
use fancy_regex::{Captures, Regex};
use std::collections::VecDeque;
use std::sync::OnceLock;

pub enum Stashed {
    Node(Id),
    Str(String),
}

enum Found {
    Elem(Id),
    Str(String),
}

/// (node, start, end) — Python's handleMatch triple with non-None
/// start/end; `end` may be negative (a Python index from the end).
type Handled = Option<(Option<Found>, usize, isize)>;

const PREFIX: &str = "\u{2}klzzwxh:";
const N_PATTERNS: usize = 17;
const ESCAPED_CHARS: [char; 17] = [
    '\\', '`', '*', '_', '{', '}', '[', ']', '(', ')', '>', '#', '+', '-', '.', '!', '|',
];

struct Res {
    pats: Vec<Regex>,
    ast: Vec<Regex>,
    und: Vec<Regex>,
    link: Regex,
    ref_link: Regex,
}

const W: &str = r"[\p{L}\p{N}_]";

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| {
        let s = r"[\s\x1c-\x1f]";
        let pats = vec![
            build(r"(?:(?<!\\)((?:\\{2})+)(?=`+)|(?<!\\)`)"),
            build(r"(?s)\\(.)"),
            build(r"(?<!!)\["),
            build(r"(?<!!)\["),
            build(r"!\["),
            build(r"!\["),
            build(r"(?<!!)\["),
            build(r"!\["),
            build(r"<((?:[Ff]|[Hh][Tt])[Tt][Pp][Ss]?://[^<>]*)>"),
            build(r"<([^<> !]+@[^@<> ]+)>"),
            build(r"  \n"),
            build(
                r"(?s)(<(/?[a-zA-Z][^<>@ ]*( [^<>]*)?|!--(?:(?!<!--|-->).)*--|[?](?:(?!<[?]|[?]>).)*[?]|!\[CDATA\[(?:(?!<!\[CDATA\[|\]\]>).)*\]\])>)",
            ),
            build(r"(&(?:#[0-9]+|#x[0-9a-fA-F]+|[a-zA-Z0-9]+);)"),
            build(&format!(r"((^|(?<={s}))(\*{{1,3}}|_{{1,3}})(?={s}|\z))")),
            build(r"\*"),
            build(r"_"),
            build(r"\n"),
        ];
        let ast = vec![
            build(r"(?s)\A(\*)\*{2}(.+?)\*(.*?)\*{2}"),
            build(r"(?s)\A(\*)\*{2}(.+?)\*{2}(.*?)\*"),
            build(r"(?s)\A(\*)\*(?!\*)([^*]+?)\*(?!\*)(.+?)\*{3}"),
            build(r"(?s)\A(\*{2})(.+?)\*{2}"),
            build(r"(?s)\A(\*)([^\*]+)\*"),
        ];
        let und = vec![
            build(r"(?s)\A(_)_{2}(.+?)_(.*?)_{2}"),
            build(r"(?s)\A(_)_{2}(.+?)_{2}(.*?)_"),
            build(&format!(
                r"(?s)\A(_)_(?!_)(.+?)(?<!{W})_(?!_)(.+?)_{{3}}(?!{W})"
            )),
            build(&format!(r"(?s)\A(__)(?!_)(.+?)(?<!_)__(?!{W})")),
            build(&format!(r"(?s)\A(_)(?!_)(.+?)(?<!_)_(?!{W})")),
        ];
        Res {
            pats,
            ast,
            und,
            link: build(&format!(
                r#"(?s)\A\({s}*(?:(<[^<>]*>){s}*(?:('[^']*'|"[^"]*"){s}*)?\))?"#
            )),
            ref_link: build(&format!(r"(?s)\A{s}?\[([^\]]*)\]")),
        }
    })
}

fn code_escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn char_len(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Byte offset for a Python index (negative = from the end, in chars).
fn py_idx(s: &str, i: isize) -> usize {
    if i >= 0 {
        return (i as usize).min(s.len());
    }
    let n = (-i) as usize;
    let total = s.chars().count();
    if n >= total {
        return 0;
    }
    s.char_indices().nth(total - n).map(|(b, _)| b).unwrap_or(0)
}

fn py_slice(s: &str, a: isize, b: isize) -> &str {
    let a = py_idx(s, a);
    let b = py_idx(s, b);
    if a >= b {
        ""
    } else {
        &s[a..b]
    }
}

/// `\s+` → ' ' (or each `\s` → ' ' with `each`).
fn collapse_ws(s: &str, each: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if crate::pyfmt::is_space(c) {
            if each || !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn dequote(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        let mut cs = s.char_indices();
        cs.next();
        let start = cs.next().map(|(i, _)| i).unwrap_or(s.len());
        let end = s.char_indices().last().map(|(i, _)| i).unwrap_or(0);
        if start >= end {
            ""
        } else {
            &s[start..end]
        }
    } else {
        s
    }
}

/// `util.INLINE_PLACEHOLDER_RE.search(s, from)`: (start, digits, end).
fn find_placeholder(s: &str, from: usize) -> Option<(usize, &str, usize)> {
    let mut i = from;
    while let Some(off) = s.get(i..).and_then(|r| r.find(PREFIX)) {
        let p = i + off;
        let d0 = p + PREFIX.len();
        let d1 = d0 + s[d0..].bytes().take_while(u8::is_ascii_digit).count();
        if d1 > d0 && s.as_bytes().get(d1) == Some(&3) {
            return Some((p, &s[d0..d1], d1 + 1));
        }
        i = p + 1;
    }
    None
}

/// `BacktickInlineProcessor.find_code_spans`: (content start, content
/// end, match end) as byte offsets. Python counts in chars; the content
/// start can land past the end (then the code text is "").
fn find_code_spans(start: usize, data: &str) -> Option<(usize, usize, usize)> {
    let text = data.as_bytes();
    let last = text.len();
    let mut start = start;
    let mut max_ticks = 0;
    while start < last && text[start] == b'`' {
        max_ticks += 1;
        start += 1;
    }
    if max_ticks == 0 {
        return None;
    }
    let mut longest = 0;
    let mut end = 0;
    let mut i = start;
    while i < last {
        let mut span = 0;
        while i < last && text[i] == b'`' {
            span += 1;
            i += 1;
        }
        if span == 0 {
            i += 1;
            continue;
        }
        if max_ticks == span {
            return Some((start, i - span, i));
        }
        if span > longest {
            longest = span;
            end = i;
        }
    }
    if longest > 0 {
        let s = if longest <= max_ticks {
            start - (max_ticks - longest)
        } else {
            // advance (longest - max_ticks) chars
            data[start..]
                .char_indices()
                .nth(longest - max_ticks)
                .map(|(b, _)| start + b)
                .unwrap_or(last)
        };
        return Some((s, end - longest, end));
    }
    None
}

struct Groups {
    end: usize,
    g2: String,
    g3: Option<String>,
}

impl Md {
    fn stash_node(&mut self, node: Stashed) -> String {
        let id = format!("{:04}", self.stashed.len());
        self.stashed.push(node);
        format!("{PREFIX}{id}{ETX}")
    }

    fn stash_get(&self, id: &str) -> Option<&Stashed> {
        let idx: usize = id.parse().ok()?;
        if format!("{idx:04}") != id {
            return None;
        }
        self.stashed.get(idx)
    }

    /// `Pattern.unescape`.
    fn unescape_ph(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        let mut from = 0;
        while let Some((p, id, e)) = find_placeholder(text, from) {
            out.push_str(&text[last..p]);
            match self.stash_get(id) {
                Some(Stashed::Str(s)) => out.push_str(s),
                Some(Stashed::Node(n)) => out.push_str(&self.tree.itertext(*n)),
                None => {}
            }
            last = e;
            from = e;
        }
        out.push_str(&text[last..]);
        out
    }

    /// `HtmlInlineProcessor.unescape`.
    fn html_unescape(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        let mut from = 0;
        while let Some((p, id, e)) = find_placeholder(text, from) {
            out.push_str(&text[last..p]);
            match self.stash_get(id) {
                Some(Stashed::Str(s)) => {
                    out.push('\\');
                    out.push_str(s);
                }
                Some(Stashed::Node(n)) => {
                    let html = self.tree.to_html(*n);
                    out.push_str(&self.html_unescape(&html));
                }
                None => {}
            }
            last = e;
            from = e;
        }
        out.push_str(&text[last..]);
        out
    }

    fn em_match(&self, und: bool, idx: usize, data: &str, pos: usize) -> R<Option<Groups>> {
        if und && idx >= 2 {
            // leading (?<!\w)
            if data[..pos].chars().next_back().is_some_and(is_py_word) {
                return Ok(None);
            }
        }
        let re = if und {
            &res().und[idx]
        } else {
            &res().ast[idx]
        };
        let Some(c) = re.captures(&data[pos..]).map_err(|_| Fail)? else {
            return Ok(None);
        };
        Ok(Some(Groups {
            end: pos + c.get(0).expect("g0").end(),
            g2: c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
            g3: c.get(3).map(|m| m.as_str().to_string()),
        }))
    }

    fn build_em(&mut self, und: bool, idx: usize, g: &Groups) -> R<Id> {
        match idx {
            0 | 1 => {
                let (t1, t2) = if idx == 0 {
                    ("strong", "em")
                } else {
                    ("em", "strong")
                };
                let el1 = self.tree.element(t1);
                let el2 = self.tree.element(t2);
                self.parse_sub_patterns(und, &g.g2, el2, None, idx)?;
                self.tree.append(el1, el2);
                let g3 = g.g3.clone().unwrap_or_default();
                self.parse_sub_patterns(und, &g3, el1, Some(el2), idx)?;
                Ok(el1)
            }
            2 => {
                let el1 = self.tree.element("strong");
                let el2 = self.tree.element("em");
                self.parse_sub_patterns(und, &g.g2, el1, None, idx)?;
                self.tree.append(el1, el2);
                let g3 = g.g3.clone().unwrap_or_default();
                self.parse_sub_patterns(und, &g3, el2, None, idx)?;
                Ok(el1)
            }
            _ => {
                let el = self.tree.element(if idx == 3 { "strong" } else { "em" });
                self.parse_sub_patterns(und, &g.g2, el, None, idx)?;
                Ok(el)
            }
        }
    }

    fn set_text_or_tail(&mut self, parent: Id, last: Option<Id>, text: &str) {
        if text.is_empty() {
            return;
        }
        match last {
            Some(l) => self.tree.nodes[l].tail = Some(Txt::plain(text)),
            None => self.tree.nodes[parent].text = Some(Txt::plain(text)),
        }
    }

    /// `AsteriskProcessor.parse_sub_patterns`.
    fn parse_sub_patterns(
        &mut self,
        und: bool,
        data: &str,
        parent: Id,
        last: Option<Id>,
        idx: usize,
    ) -> R<()> {
        let ch = if und { b'_' } else { b'*' };
        let mut last = last;
        let mut offset = 0;
        let mut pos = 0;
        while pos < data.len() {
            if data.as_bytes()[pos] == ch {
                let mut matched = false;
                for index in 0..5 {
                    if index <= idx {
                        continue;
                    }
                    if let Some(g) = self.em_match(und, index, data, pos)? {
                        let text = &data[offset..pos];
                        self.set_text_or_tail(parent, last, text);
                        let el = self.build_em(und, index, &g)?;
                        self.tree.append(parent, el);
                        last = Some(el);
                        offset = g.end;
                        pos = g.end;
                        matched = true;
                    }
                }
                if !matched {
                    pos += char_len(data, pos);
                }
            } else {
                pos += char_len(data, pos);
            }
        }
        let text = &data[offset..];
        self.set_text_or_tail(parent, last, text);
        Ok(())
    }

    /// `LinkInlineProcessor.getText`.
    fn get_text(data: &str, index: usize) -> (String, usize, bool) {
        let mut bracket_count = 1;
        let mut text = String::new();
        let mut index = index;
        for c in data[index..].chars() {
            if c == ']' {
                bracket_count -= 1;
            } else if c == '[' {
                bracket_count += 1;
            }
            index += c.len_utf8();
            if bracket_count == 0 {
                break;
            }
            text.push(c);
        }
        (text, index, bracket_count == 0)
    }

    /// `LinkInlineProcessor.getLink`.
    fn get_link(&self, data: &str, index: usize) -> R<(String, Option<String>, isize, bool)> {
        let mut href = String::new();
        let mut title: Option<String> = None;
        let mut handled = false;
        let mut index = index as isize;
        let caps = res()
            .link
            .captures(&data[index as usize..])
            .map_err(|_| Fail)?;
        if let Some(m) = caps {
            let base = index as usize;
            if let Some(g1) = m.get(1).filter(|g| !g.as_str().is_empty()) {
                let g = g1.as_str();
                href = strip(&g[1..g.len() - 1]).to_string();
                if let Some(g2) = m.get(2).filter(|g| !g.as_str().is_empty()) {
                    let t = g2.as_str();
                    title = Some(t[1..t.len() - 1].to_string());
                }
                index = (base + m.get(0).expect("g0").end()) as isize;
                handled = true;
            } else {
                let start_index = base + m.get(0).expect("g0").end();
                let mut bracket_count: i64 = 1;
                let mut backtrack_count: i64 = 1;
                let mut idx = start_index as isize;
                let mut last_bracket: isize = -1;
                let mut quote: Option<char> = None;
                let mut start_quote: isize = -1;
                let mut exit_quote: isize = -1;
                let mut ignore_matches = false;
                let mut alt_quote: Option<char> = None;
                let mut start_alt_quote: isize = -1;
                let mut exit_alt_quote: isize = -1;
                let mut last: Option<char> = None;
                for c in data[start_index..].chars() {
                    if c == '(' {
                        if !ignore_matches {
                            bracket_count += 1;
                        } else if backtrack_count > 0 {
                            backtrack_count -= 1;
                        }
                    } else if c == ')' {
                        if (exit_quote != -1 && quote.is_some() && quote == last)
                            || (exit_alt_quote != -1 && alt_quote.is_some() && alt_quote == last)
                        {
                            bracket_count = 0;
                        } else if !ignore_matches {
                            bracket_count -= 1;
                        } else if backtrack_count > 0 {
                            backtrack_count -= 1;
                            if backtrack_count == 0 {
                                last_bracket = idx + 1;
                            }
                        }
                    } else if c == '\'' || c == '"' {
                        if quote.is_none() {
                            ignore_matches = true;
                            backtrack_count = bracket_count;
                            bracket_count = 1;
                            start_quote = idx + 1;
                            quote = Some(c);
                        } else if Some(c) != quote && alt_quote.is_none() {
                            start_alt_quote = idx + 1;
                            alt_quote = Some(c);
                        } else if Some(c) == quote {
                            exit_quote = idx + 1;
                        } else if alt_quote.is_some() && Some(c) == alt_quote {
                            exit_alt_quote = idx + 1;
                        }
                    }
                    idx += c.len_utf8() as isize;
                    if bracket_count == 0 {
                        let si = start_index as isize;
                        if exit_quote >= 0 && quote.is_some() && quote == last {
                            href = py_slice(data, si, start_quote - 1).to_string();
                            title = Some(py_slice(data, start_quote, exit_quote - 1).to_string());
                        } else if exit_alt_quote >= 0 && alt_quote.is_some() && alt_quote == last {
                            href = py_slice(data, si, start_alt_quote - 1).to_string();
                            title = Some(
                                py_slice(data, start_alt_quote, exit_alt_quote - 1).to_string(),
                            );
                        } else {
                            href = py_slice(data, si, idx - 1).to_string();
                        }
                        break;
                    }
                    if c != ' ' {
                        last = Some(c);
                    }
                }
                if bracket_count != 0 && backtrack_count == 0 {
                    href = py_slice(data, start_index as isize, last_bracket - 1).to_string();
                    idx = last_bracket;
                    bracket_count = 0;
                }
                index = idx;
                handled = bracket_count == 0;
            }
        }
        if let Some(t) = title {
            let t = self.unescape_ph(strip(&t));
            title = Some(collapse_ws(dequote(&t), true));
        }
        let href = strip(&self.unescape_ph(&href)).to_string();
        Ok((href, title, index, handled))
    }

    fn handle(&mut self, pi: usize, m: &Captures, data: &str) -> R<Handled> {
        let m0 = m.get(0).expect("g0");
        let (ms, me) = (m0.start(), m0.end() as isize);
        let g = |i: usize| m.get(i).map(|x| x.as_str()).unwrap_or("");
        Ok(match pi {
            0 => {
                if let Some(g1) = m.get(1).filter(|x| !x.as_str().is_empty()) {
                    let s = g1.as_str().replace("\\\\", "\u{2}92\u{3}");
                    return Ok(Some((Some(Found::Str(s)), ms, me)));
                }
                match find_code_spans(ms, data) {
                    Some((s, e, end)) => {
                        let el = self.tree.element("code");
                        let code = if s < e { strip(&data[s..e]) } else { "" };
                        self.tree.nodes[el].text = Some(Txt::atomic(code_escape(code)));
                        Some((Some(Found::Elem(el)), ms, end as isize))
                    }
                    None => None,
                }
            }
            1 => {
                let c = g(1).chars().next().expect("one char");
                if ESCAPED_CHARS.contains(&c) {
                    Some((Some(Found::Str(format!("{STX}{}{ETX}", c as u32))), ms, me))
                } else {
                    Some((None, ms, me))
                }
            }
            2 | 5 | 6 | 7 => {
                let (text, index, handled) = Self::get_text(data, m0.end());
                if !handled {
                    return Ok(None);
                }
                let (id, end) = if pi == 6 || pi == 7 {
                    (text.to_lowercase(), index)
                } else {
                    let Some(c) = res().ref_link.captures(&data[index..]).map_err(|_| Fail)? else {
                        return Ok(None);
                    };
                    let mut id = c.get(1).expect("g1").as_str().to_lowercase();
                    let end = index + c.get(0).expect("g0").end();
                    if id.is_empty() {
                        id = text.to_lowercase();
                    }
                    (id, end)
                };
                let id = collapse_ws(&id, false);
                let Some((href, title)) = self.references.get(&id).cloned() else {
                    return Ok(Some((None, ms, end as isize)));
                };
                let img = pi == 5 || pi == 7;
                let el = self.tree.element(if img { "img" } else { "a" });
                if img {
                    self.tree.set(el, "src", href);
                } else {
                    self.tree.set(el, "href", href);
                }
                if let Some(t) = title.filter(|t| !t.is_empty()) {
                    self.tree.set(el, "title", t);
                }
                if img {
                    let alt = self.unescape_ph(&text);
                    self.tree.set(el, "alt", alt);
                } else {
                    self.tree.nodes[el].text = Some(Txt::plain(text));
                }
                Some((Some(Found::Elem(el)), ms, end as isize))
            }
            3 | 4 => {
                let (text, index, handled) = Self::get_text(data, m0.end());
                if !handled {
                    return Ok(None);
                }
                let (href, title, index, handled) = self.get_link(data, index)?;
                if !handled {
                    return Ok(None);
                }
                let el;
                if pi == 3 {
                    el = self.tree.element("a");
                    self.tree.nodes[el].text = Some(Txt::plain(text));
                    self.tree.set(el, "href", href);
                    if let Some(t) = title {
                        self.tree.set(el, "title", t);
                    }
                } else {
                    el = self.tree.element("img");
                    self.tree.set(el, "src", href);
                    if let Some(t) = title {
                        self.tree.set(el, "title", t);
                    }
                    let alt = self.unescape_ph(&text);
                    self.tree.set(el, "alt", alt);
                }
                Some((Some(Found::Elem(el)), ms, index))
            }
            8 => {
                let el = self.tree.element("a");
                let href = self.unescape_ph(g(1));
                self.tree.set(el, "href", href);
                self.tree.nodes[el].text = Some(Txt::atomic(g(1)));
                Some((Some(Found::Elem(el)), ms, me))
            }
            9 => {
                let el = self.tree.element("a");
                let mut email = self.unescape_ph(g(1));
                if let Some(rest) = email.strip_prefix("mailto:") {
                    email = rest.to_string();
                }
                let amp = "\u{2}amp\u{3}";
                let letters: String = email
                    .chars()
                    .map(|c| {
                        let code = c as u32;
                        match CODEPOINT2NAME.binary_search_by_key(&code, |&(k, _)| k) {
                            Ok(i) => format!("{amp}{};", CODEPOINT2NAME[i].1),
                            Err(_) => format!("{amp}#{code};"),
                        }
                    })
                    .collect();
                self.tree.nodes[el].text = Some(Txt::atomic(letters));
                let mailto: String = format!("mailto:{email}")
                    .chars()
                    .map(|c| format!("{amp}#{};", c as u32))
                    .collect();
                self.tree.set(el, "href", mailto);
                Some((Some(Found::Elem(el)), ms, me))
            }
            10 | 16 => {
                let el = self.tree.element("br");
                Some((Some(Found::Elem(el)), ms, me))
            }
            11 | 12 => {
                let raw = super::unescape_stx(&self.html_unescape(g(1)));
                let ph = self.stash.store(raw);
                Some((Some(Found::Str(ph)), ms, me))
            }
            13 => Some((Some(Found::Str(g(1).to_string())), ms, me)),
            14 | 15 => {
                let und = pi == 15;
                let mut out = None;
                for idx in 0..5 {
                    if let Some(gr) = self.em_match(und, idx, data, ms)? {
                        let el = self.build_em(und, idx, &gr)?;
                        out = Some((Some(Found::Elem(el)), ms, gr.end as isize));
                        break;
                    }
                }
                out
            }
            _ => unreachable!("17 patterns"),
        })
    }

    /// `InlineProcessor.__applyPattern`.
    fn apply_pattern(
        &mut self,
        pi: usize,
        data: String,
        start_index: usize,
    ) -> R<(String, bool, usize)> {
        let re = &res().pats[pi];
        let mut pos = start_index.min(data.len());
        let found = loop {
            let caps = re.captures_from_pos(&data, pos).map_err(|_| Fail)?;
            let Some(caps) = caps else {
                return Ok((data, false, 0));
            };
            match self.handle(pi, &caps, &data)? {
                Some(x) => break x,
                None => {
                    let m0 = caps.get(0).expect("g0");
                    pos = if m0.end() > m0.start() {
                        m0.end()
                    } else {
                        m0.end() + char_len(&data, m0.end())
                    };
                    if pos > data.len() {
                        return Ok((data, false, 0));
                    }
                }
            }
        };
        let (node, start, end) = found;
        let Some(node) = node else {
            return Ok((data, true, end.max(0) as usize));
        };
        let stashed = match node {
            Found::Elem(id) => {
                if !self.tree.nodes[id].text.as_ref().is_some_and(|t| t.atomic) {
                    let mut kids = vec![id];
                    kids.extend(self.tree.nodes[id].children.iter().copied());
                    for child in kids {
                        if truthy(&self.tree.nodes[child].text) {
                            let t = self.tree.nodes[child].text.take().expect("text");
                            self.tree.nodes[child].text = Some(self.handle_inline(t, pi + 1)?);
                        }
                        if truthy(&self.tree.nodes[child].tail) {
                            let t = self.tree.nodes[child].tail.take().expect("tail");
                            self.tree.nodes[child].tail = Some(self.handle_inline(t, pi)?);
                        }
                    }
                }
                Stashed::Node(id)
            }
            Found::Str(s) => Stashed::Str(s),
        };
        let ph = self.stash_node(stashed);
        let e = py_idx(&data, end);
        let new = format!("{}{}{}", &data[..start], ph, &data[e..]);
        Ok((new, true, start + ph.len()))
    }

    /// `InlineProcessor.__handleInline`.
    fn handle_inline(&mut self, data: Txt, pattern_index: usize) -> R<Txt> {
        if data.atomic {
            return Ok(data);
        }
        let mut d = data.s;
        let mut start = 0;
        let mut pi = pattern_index;
        while pi < N_PATTERNS {
            let (nd, matched, ns) = self.apply_pattern(pi, d, start)?;
            d = nd;
            start = ns;
            if !matched {
                pi += 1;
            }
        }
        Ok(Txt::plain(d))
    }

    fn link_text(&mut self, result: &[Id], parent: Id, is_text: bool, text: Txt) {
        if text.s.is_empty() {
            return;
        }
        let slot = if let Some(&last) = result.last() {
            &mut self.tree.nodes[last].tail
        } else if !is_text {
            &mut self.tree.nodes[parent].tail
        } else {
            &mut self.tree.nodes[parent].text
        };
        *slot = Some(match slot.take() {
            Some(cur) if !cur.s.is_empty() => Txt::plain(cur.s + &text.s),
            _ => text,
        });
    }

    /// `InlineProcessor.__processPlaceholders`.
    fn process_placeholders(&mut self, data: Option<Txt>, parent: Id, is_text: bool) -> R<Vec<Id>> {
        let mut result: Vec<Id> = Vec::new();
        let Some(data) = data else {
            return Ok(result);
        };
        let s = data.s;
        let mut start = 0usize;
        while !s.is_empty() {
            match s[start.min(s.len())..].find(PREFIX).map(|i| i + start) {
                Some(index) => {
                    let (id, ph_end) = match find_placeholder(&s, index) {
                        Some((_, id, e)) => (Some(id.to_string()), e),
                        None => (None, index + 1),
                    };
                    let node = id.as_deref().and_then(|id| match self.stash_get(id) {
                        Some(Stashed::Node(n)) => Some(Found::Elem(*n)),
                        Some(Stashed::Str(x)) => Some(Found::Str(x.clone())),
                        None => None,
                    });
                    match node {
                        Some(node) => {
                            if index > 0 {
                                let text = s[start.min(index)..index].to_string();
                                self.link_text(&result, parent, is_text, Txt::plain(text));
                            }
                            match node {
                                Found::Elem(n) => {
                                    let mut kids = vec![n];
                                    kids.extend(self.tree.nodes[n].children.iter().copied());
                                    for child in kids {
                                        let tail_ok = self.tree.nodes[child]
                                            .tail
                                            .as_ref()
                                            .is_some_and(|t| !strip(&t.s).is_empty());
                                        if tail_ok {
                                            self.process_element_text(n, child, false)?;
                                        }
                                        let text_ok = self.tree.nodes[child]
                                            .text
                                            .as_ref()
                                            .is_some_and(|t| !strip(&t.s).is_empty());
                                        if text_ok {
                                            self.process_element_text(child, child, true)?;
                                        }
                                    }
                                    start = ph_end;
                                    result.push(n);
                                }
                                Found::Str(x) => {
                                    self.link_text(&result, parent, is_text, Txt::plain(x));
                                    start = ph_end;
                                }
                            }
                        }
                        None => {
                            let end = index + PREFIX.len();
                            let text = s[start.min(end)..end].to_string();
                            self.link_text(&result, parent, is_text, Txt::plain(text));
                            start = end;
                        }
                    }
                }
                None => {
                    let text = s[start.min(s.len())..].to_string();
                    self.link_text(
                        &result,
                        parent,
                        is_text,
                        Txt {
                            s: text,
                            atomic: data.atomic,
                        },
                    );
                    break;
                }
            }
        }
        Ok(result)
    }

    /// `InlineProcessor.__processElementText`.
    fn process_element_text(&mut self, node: Id, subnode: Id, is_text: bool) -> R<()> {
        let text = if is_text {
            self.tree.nodes[subnode].text.take()
        } else {
            self.tree.nodes[subnode].tail.take()
        };
        let mut child_result = self.process_placeholders(text, subnode, is_text)?;
        let pos = if !is_text && node != subnode {
            self.tree.nodes[node]
                .children
                .iter()
                .position(|&c| c == subnode)
                .ok_or(Fail)?
                + 1
        } else {
            0
        };
        child_result.reverse();
        for c in child_result {
            self.tree.insert(node, pos, c);
        }
        Ok(())
    }

    /// `InlineProcessor.run`.
    pub(super) fn inline_run(&mut self, root: Id) -> R<()> {
        self.stashed.clear();
        let mut stack: VecDeque<Id> = VecDeque::from([root]);
        while let Some(curr) = stack.pop_front() {
            let mut insert_queue: Vec<(Id, Vec<Id>)> = Vec::new();
            let mut i = 0;
            while i < self.tree.nodes[curr].children.len() {
                let child = self.tree.nodes[curr].children[i];
                let text_ok = self.tree.nodes[child]
                    .text
                    .as_ref()
                    .is_some_and(|t| !t.s.is_empty() && !t.atomic);
                if text_ok {
                    let text = self.tree.nodes[child].text.take().expect("text");
                    let handled = self.handle_inline(text, 0)?;
                    let lst = self.process_placeholders(Some(handled), child, true)?;
                    stack.extend(lst.iter().copied());
                    insert_queue.push((child, lst));
                }
                if truthy(&self.tree.nodes[child].tail) {
                    let tail = self.tree.nodes[child].tail.clone().expect("tail");
                    let tail = self.handle_inline(tail, 0)?;
                    let dumby = self.tree.element("d");
                    self.tree.nodes[child].tail = None;
                    let mut tail_result = self.process_placeholders(Some(tail), dumby, false)?;
                    if truthy(&self.tree.nodes[dumby].tail) {
                        self.tree.nodes[child].tail = self.tree.nodes[dumby].tail.take();
                    }
                    let pos = self.tree.nodes[curr]
                        .children
                        .iter()
                        .position(|&c| c == child)
                        .ok_or(Fail)?
                        + 1;
                    tail_result.reverse();
                    for nc in tail_result {
                        self.tree.insert(curr, pos, nc);
                    }
                }
                if !self.tree.nodes[child].children.is_empty() {
                    stack.push_back(child);
                }
                i += 1;
            }
            for (element, lst) in insert_queue {
                for (k, obj) in lst.into_iter().enumerate() {
                    self.tree.insert(element, k, obj);
                }
            }
        }
        Ok(())
    }
}
