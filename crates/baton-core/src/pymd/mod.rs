//! A port of Python-Markdown 3.x as the reference server calls it:
//! `markdown.markdown(text, extensions=["tables", "fenced_code", "nl2br"],
//! output_format="html5")`. The session preview's `html` field must be
//! byte-identical, quirks included (the preprocessors, the patched
//! `html.parser` state machine, block processors, inline patterns and the
//! ElementTree serializer are each ported line by line).

mod block;
mod entities;
mod html_extract;
mod inline;
mod tree;

use std::collections::HashMap;
use tree::{Id, Tree, Txt};

/// An exception the Python code would have raised (IndexError & co.) —
/// the server then renders `html = ""`.
#[derive(Debug)]
pub struct Fail;

pub type R<T> = Result<T, Fail>;

pub const STX: char = '\u{2}';
pub const ETX: char = '\u{3}';

/// `util.BLOCK_LEVEL_ELEMENTS`.
const BLOCK_LEVEL_ELEMENTS: [&str; 59] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
    "canvas",
    "colgroup",
    "dd",
    "body",
    "dt",
    "group",
    "html",
    "iframe",
    "li",
    "legend",
    "math",
    "map",
    "noscript",
    "output",
    "object",
    "option",
    "progress",
    "script",
    "style",
    "summary",
    "tbody",
    "td",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "tr",
    "video",
    "center",
];

/// `Markdown.is_block_level`.
pub fn is_block_level(tag: &str) -> bool {
    let t = tag.to_lowercase();
    let t = t.trim_end_matches('/');
    BLOCK_LEVEL_ELEMENTS.contains(&t)
}

/// `util.HtmlStash` (raw HTML blocks).
#[derive(Default)]
pub struct Stash {
    pub blocks: Vec<String>,
}

impl Stash {
    pub fn store(&mut self, html: String) -> String {
        self.blocks.push(html);
        format!("{STX}wzxhzdk:{}{ETX}", self.blocks.len() - 1)
    }
}

pub(crate) struct Md {
    pub tree: Tree,
    pub stash: Stash,
    pub references: HashMap<String, (String, Option<String>)>,
    pub state: Vec<&'static str>,
    pub hr_match: (usize, usize),
    pub table_border: u8,
    pub table_sep: Vec<String>,
    pub stashed: Vec<inline::Stashed>,
}

/// `markdown.markdown(source, extensions=["tables", "fenced_code",
/// "nl2br"], output_format="html5")`; `""` where Python raised.
pub fn markdown(source: &str) -> String {
    convert(source).unwrap_or_default()
}

fn convert(source: &str) -> R<String> {
    if crate::pyfmt::strip(source).is_empty() {
        return Ok(String::new());
    }
    let mut md = Md {
        tree: Tree::default(),
        stash: Stash::default(),
        references: HashMap::new(),
        state: Vec::new(),
        hr_match: (0, 0),
        table_border: 0,
        table_sep: Vec::new(),
        stashed: Vec::new(),
    };
    // Preprocessors: normalize_whitespace (30), fenced_code_block (25),
    // html_block (20). Each works on '\n'.join(lines).
    let text = normalize_whitespace(source);
    let text = fenced_blocks(&mut md.stash, text);
    let text = {
        let mut ex = html_extract::Extractor::new(&mut md.stash);
        ex.feed(&text);
        ex.close();
        ex.cleandoc.concat()
    };
    let root = md.tree.element("div");
    md.parse_chunk(root, &text)?;
    md.inline_run(root)?;
    prettify(&mut md.tree, root);
    unescape_tree(&mut md.tree, root);
    let output = md.tree.to_html(root);
    let start = output.find("<div>").ok_or(Fail)? + 5;
    let end = output.rfind("</div>").ok_or(Fail)?;
    let output = if start <= end {
        crate::pyfmt::strip(&output[start..end]).to_string()
    } else {
        String::new()
    };
    let output = raw_html_post(&md.stash, &output);
    let output = output.replace("\u{2}amp\u{3}", "&");
    Ok(crate::pyfmt::strip(&output).to_string())
}

/// `NormalizeWhitespace.run`.
fn normalize_whitespace(source: &str) -> String {
    let s: String = source.chars().filter(|&c| c != STX && c != ETX).collect();
    let s = s.replace("\r\n", "\n").replace('\r', "\n") + "\n\n";
    // expandtabs(4)
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            '\t' => {
                let n = 4 - (col % 4);
                for _ in 0..n {
                    out.push(' ');
                }
                col += n;
            }
            '\n' | '\r' => {
                out.push(c);
                col = 0;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    // re.sub(r'(?<=\n) +\n', '\n', source): space-only lines (not the
    // first) become empty.
    let lines: Vec<&str> = out.split('\n').collect();
    let last = lines.len() - 1;
    let mut res = String::with_capacity(out.len());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            res.push('\n');
        }
        // preceded by '\n' (i > 0) and followed by one (i < last)
        if i > 0 && i < last && !line.is_empty() && line.bytes().all(|b| b == b' ') {
            continue;
        }
        res.push_str(line);
    }
    res
}

pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(crate::pyfmt::is_space)
}

pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(crate::pyfmt::is_space)
}

fn escape_fenced(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Does `FENCED_BLOCK_RE` match at line start `p`? Returns
/// (end, attrs group span, lang, code).
fn fence_at(text: &str, p: usize) -> Option<FenceMatch> {
    let b = text.as_bytes();
    let fc = *b.get(p)?;
    if fc != b'`' && fc != b'~' {
        return None;
    }
    let mut q = p;
    while q < b.len() && b[q] == fc {
        q += 1;
    }
    if q - p < 3 {
        return None;
    }
    let fence = &text[p..q];
    let mut r = q;
    while r < b.len() && b[r] == b' ' {
        r += 1;
    }
    let line_end = text[r..].find('\n').map(|i| r + i)?;
    let info = &text[r..line_end];
    // Alternative 1: {attrs} filling the rest of the line.
    if info.len() >= 2 && info.starts_with('{') && info.ends_with('}') {
        let body_start = line_end + 1;
        if let Some((code_end, end)) = find_close(text, body_start, fence) {
            return Some(FenceMatch {
                start: p,
                end,
                attrs: Some((r + 1, line_end - 1)),
                lang: None,
                code: (body_start, code_end),
            });
        }
        return None;
    }
    // Alternative 2: (\.?(?P<lang>[\w#.+-]*)[ ]*)?(hl_lines=(q)(.*?)(q)[ ]*)?\n
    let ib = info.as_bytes();
    let mut i = 0;
    if ib.first() == Some(&b'.') {
        i = 1;
    }
    let lang_start = i;
    let mut lang_end = i;
    for (j, c) in info[i..].char_indices() {
        if is_py_word(c) || matches!(c, '#' | '.' | '+' | '-') {
            lang_end = i + j + c.len_utf8();
        } else {
            break;
        }
    }
    // Greedy lang, then shorter ones (backtracking) so a following
    // hl_lines=... can match.
    let mut cut = lang_end;
    loop {
        let lang = &info[lang_start..cut];
        let mut k = cut;
        while k < ib.len() && ib[k] == b' ' {
            k += 1;
        }
        if k == ib.len() {
            let body_start = line_end + 1;
            if let Some((code_end, end)) = find_close(text, body_start, fence) {
                return Some(FenceMatch {
                    start: p,
                    end,
                    attrs: None,
                    lang: Some(lang.to_string()),
                    code: (body_start, code_end),
                });
            }
            return None;
        }
        if info[k..].starts_with("hl_lines=") {
            if let Some(m) = hl_lines_then_code(text, r + k + 9, fence) {
                return Some(FenceMatch {
                    start: p,
                    end: m.1,
                    attrs: None,
                    lang: Some(lang.to_string()),
                    code: (m.0, m.2),
                });
            }
        }
        if cut == lang_start {
            break;
        }
        // step back one char
        let prev = info[lang_start..cut]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        cut -= prev;
    }
    None
}

struct FenceMatch {
    start: usize,
    end: usize,
    attrs: Option<(usize, usize)>,
    lang: Option<String>,
    code: (usize, usize),
}

/// `hl_lines=(["'])(.*?)\1[ ]*\n` (DOTALL) followed by the code body and
/// closing fence; `q` is the index of the quote. Returns (code start,
/// match end, code end).
fn hl_lines_then_code(text: &str, q: usize, fence: &str) -> Option<(usize, usize, usize)> {
    let b = text.as_bytes();
    let quote = *b.get(q)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let mut pos = q + 1;
    while let Some(off) = text[pos..].find(quote as char) {
        let close = pos + off;
        let mut k = close + 1;
        while k < b.len() && b[k] == b' ' {
            k += 1;
        }
        if k < b.len() && b[k] == b'\n' {
            let body = k + 1;
            if let Some((code_end, end)) = find_close(text, body, fence) {
                return Some((body, end, code_end));
            }
        }
        pos = close + 1;
    }
    None
}

/// `(?P<code>.*?)(?<=\n)(?P=fence)[ ]*$` from `body` (which follows a
/// '\n'): (code end, match end).
fn find_close(text: &str, body: usize, fence: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut q = body;
    loop {
        if text[q..].starts_with(fence) {
            let mut e = q + fence.len();
            while e < b.len() && b[e] == b' ' {
                e += 1;
            }
            if e == b.len() || b[e] == b'\n' {
                return Some((q, e));
            }
        }
        let nl = text[q..].find('\n')?;
        q = q + nl + 1;
    }
}

/// `markdown.extensions.attr_list.get_attrs_and_remainder`.
fn attrs_and_remainder(s: &str) -> (Vec<(String, String)>, String) {
    let mut attrs = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    let word_end = |from: usize| {
        let mut j = from;
        while j < b.len() && !matches!(b[j], b' ' | b'=' | b'}') {
            j += 1;
        }
        j
    };
    while i < b.len() {
        let we = word_end(i);
        if we > i && we < b.len() && b[we] == b'=' {
            let v = we + 1;
            // [^ =}]+=".*?"  /  [^ =}]+='.*?'
            if v < b.len() && (b[v] == b'"' || b[v] == b'\'') {
                if let Some(off) = s[v + 1..].find(b[v] as char) {
                    let t = &s[i..v + 1 + off + 1];
                    let (k, val) = t.split_once('=').expect("has =");
                    let q = b[v] as char;
                    attrs.push((k.to_string(), val.trim_matches(q).to_string()));
                    i = v + 1 + off + 1;
                    continue;
                }
            }
            // [^ =}]+=[^ =}]+
            let ve = word_end(v);
            if ve > v {
                let (k, val) = s[i..ve].split_once('=').expect("has =");
                attrs.push((k.to_string(), val.to_string()));
                i = ve;
                continue;
            }
        }
        if we > i {
            let t = &s[i..we];
            if let Some(rest) = t.strip_prefix('.') {
                attrs.push((".".into(), rest.into()));
            } else if let Some(rest) = t.strip_prefix('#') {
                attrs.push(("id".into(), rest.into()));
            } else {
                attrs.push((t.into(), t.into()));
            }
            i = we;
            continue;
        }
        if b[i] == b' ' {
            i += 1;
            continue;
        }
        break;
    }
    let rem = &s[i..];
    let rem = match rem.find('}') {
        Some(ix) => rem[ix..].to_string(),
        None => String::new(),
    };
    (attrs, rem)
}

/// `FencedBlockPreprocessor.run` (no codehilite, no attr_list).
fn fenced_blocks(stash: &mut Stash, mut text: String) -> String {
    let mut index = 0usize;
    'outer: loop {
        // search from `index`: candidate starts are line starts >= index
        let mut p = index;
        let m = loop {
            if p > text.len() {
                break None;
            }
            let at_line_start = p == 0 || text.as_bytes()[p - 1] == b'\n';
            if at_line_start {
                if let Some(m) = fence_at(&text, p) {
                    break Some(m);
                }
            }
            match text[p..].find('\n') {
                Some(off) => p = p + off + 1,
                None => break None,
            }
        };
        let Some(m) = m else { break };
        let mut lang: Option<String> = None;
        let mut id = String::new();
        let mut classes: Vec<String> = Vec::new();
        if let Some((a0, a1)) = m.attrs {
            let (attrs, rem) = attrs_and_remainder(&text[a0..a1]);
            if !rem.is_empty() {
                index = a1;
                continue 'outer;
            }
            for (k, v) in attrs {
                if k == "id" {
                    id = v;
                } else if k == "." {
                    classes.push(v);
                }
            }
            if !classes.is_empty() {
                lang = Some(classes.remove(0));
            }
        } else if let Some(l) = m.lang.filter(|l| !l.is_empty()) {
            lang = Some(l);
        }
        let mut id_attr = String::new();
        let mut class_attr = String::new();
        let mut lang_attr = String::new();
        if let Some(l) = &lang {
            lang_attr = format!(" class=\"language-{}\"", tree::escape_attrib_html(l));
        }
        if !classes.is_empty() {
            class_attr = format!(
                " class=\"{}\"",
                tree::escape_attrib_html(&classes.join(" "))
            );
        }
        if !id.is_empty() {
            id_attr = format!(" id=\"{}\"", tree::escape_attrib_html(&id));
        }
        let code = escape_fenced(&text[m.code.0..m.code.1]);
        let code = format!("<pre{id_attr}{class_attr}><code{lang_attr}>{code}</code></pre>");
        let ph = stash.store(code);
        let new = format!("{}\n{}\n{}", &text[..m.start], ph, &text[m.end..]);
        index = m.start + 1 + ph.len();
        text = new;
    }
    text
}

/// Python `\w` (str patterns): letters, numbers, underscore.
pub fn is_py_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// `PrettifyTreeprocessor`.
fn prettify(t: &mut Tree, root: Id) {
    fn pretty(t: &mut Tree, elem: Id) {
        let tag = t.nodes[elem].tag.clone();
        if is_block_level(&tag) && tag != "code" && tag != "pre" {
            let text_blank = t.nodes[elem]
                .text
                .as_ref()
                .is_none_or(|x| crate::pyfmt::strip(&x.s).is_empty());
            if text_blank
                && !t.nodes[elem].children.is_empty()
                && is_block_level(&t.nodes[t.nodes[elem].children[0]].tag.clone())
            {
                t.nodes[elem].text = Some(Txt::plain("\n"));
            }
            let kids = t.nodes[elem].children.clone();
            for e in kids {
                if is_block_level(&t.nodes[e].tag.clone()) {
                    pretty(t, e);
                }
            }
        }
        let tail_blank = t.nodes[elem]
            .tail
            .as_ref()
            .is_none_or(|x| crate::pyfmt::strip(&x.s).is_empty());
        if tail_blank {
            t.nodes[elem].tail = Some(Txt::plain("\n"));
        }
    }
    pretty(t, root);
    for id in t.iter(root) {
        if t.nodes[id].tag == "br" {
            let tail = t.nodes[id].tail.clone();
            let blank = tail
                .as_ref()
                .is_none_or(|x| crate::pyfmt::strip(&x.s).is_empty());
            t.nodes[id].tail = Some(if blank {
                Txt::plain("\n")
            } else {
                Txt::plain(format!("\n{}", tail.expect("non-blank").s))
            });
        }
    }
    for id in t.iter(root) {
        if t.nodes[id].tag == "pre" {
            if let Some(&code) = t.nodes[id].children.first() {
                if t.nodes[code].tag == "code" && t.nodes[code].children.is_empty() {
                    if let Some(txt) = &t.nodes[code].text {
                        let s = format!("{}\n", rstrip(&txt.s));
                        t.nodes[code].text = Some(Txt::atomic(s));
                    }
                }
            }
        }
    }
}

/// `\x02(\d+)\x03` → `chr(int(...))`.
pub fn unescape_stx(s: &str) -> String {
    if !s.contains(STX) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(STX) {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(ETX) {
            let n: u32 = after[..digits].parse().unwrap_or(0);
            if let Some(c) = char::from_u32(n) {
                out.push(c);
                rest = &after[digits + 1..];
                continue;
            }
        }
        out.push(STX);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// `UnescapeTreeprocessor`.
fn unescape_tree(t: &mut Tree, root: Id) {
    for id in t.iter(root) {
        let n = &mut t.nodes[id];
        if n.tag != "code" {
            if let Some(x) = &n.text {
                if !x.s.is_empty() {
                    n.text = Some(Txt::plain(unescape_stx(&x.s)));
                }
            }
        }
        if let Some(x) = &n.tail {
            if !x.s.is_empty() {
                n.tail = Some(Txt::plain(unescape_stx(&x.s)));
            }
        }
        for a in n.attrs.iter_mut() {
            a.1 = unescape_stx(&a.1);
        }
    }
}

/// `RawHtmlPostprocessor.run`.
fn raw_html_post(stash: &Stash, text: &str) -> String {
    if stash.blocks.is_empty() {
        return text.to_string();
    }
    raw_html_sub(stash, text)
}

/// One `\x02wzxhzdk:(\d+)\x03` at `i`: (key digits end, match end).
fn html_ph_at(s: &str, i: usize) -> Option<(usize, usize, usize)> {
    let pre = "\u{2}wzxhzdk:";
    if !s[i..].starts_with(pre) {
        return None;
    }
    let d0 = i + pre.len();
    let b = s.as_bytes();
    let mut d1 = d0;
    while d1 < b.len() && b[d1].is_ascii_digit() {
        d1 += 1;
    }
    if d1 > d0 && d1 < b.len() && b[d1] == 3 {
        Some((d0, d1, d1 + 1))
    } else {
        None
    }
}

fn raw_html_sub(stash: &Stash, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut last = 0;
    let b = text.as_bytes();
    while i < b.len() {
        // alternative 1: <p>PH</p>
        if b[i] == b'<' && text[i..].starts_with("<p>") {
            if let Some((d0, d1, e)) = html_ph_at(text, i + 3) {
                if text[e..].starts_with("</p>") {
                    out.push_str(&text[last..i]);
                    out.push_str(&raw_html_one(stash, &text[i..e + 4], &text[d0..d1], true));
                    i = e + 4;
                    last = i;
                    continue;
                }
            }
        }
        if b[i] == 2 {
            if let Some((d0, d1, e)) = html_ph_at(text, i) {
                out.push_str(&text[last..i]);
                out.push_str(&raw_html_one(stash, &text[i..e], &text[d0..d1], false));
                i = e;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}

fn raw_html_one(stash: &Stash, whole: &str, key: &str, wrapped: bool) -> String {
    let key: usize = match key.parse() {
        Ok(k) => k,
        Err(_) => return whole.to_string(),
    };
    if key >= stash.blocks.len() {
        return whole.to_string();
    }
    let html = &stash.blocks[key];
    if !wrapped || is_block_html(html) {
        raw_html_sub(stash, html)
    } else {
        raw_html_sub(stash, &format!("<p>{html}</p>"))
    }
}

/// `RawHtmlPostprocessor.isblocklevel`: `^\<\/?([^ >]+)`.
fn is_block_html(html: &str) -> bool {
    let Some(rest) = html.strip_prefix('<') else {
        return false;
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let end = rest.find([' ', '>']).unwrap_or(rest.len());
    if end == 0 {
        return false;
    }
    let name = &rest[..end];
    if matches!(name.chars().next(), Some('!' | '?' | '@' | '%')) {
        return true;
    }
    is_block_level(name)
}

#[cfg(test)]
mod tests {
    use super::markdown;

    #[test]
    fn basics() {
        assert_eq!(markdown("x </y z\n\nmore text"), "<p>x </p>");
        assert_eq!(
            markdown("a &#zz; b &#q; c <b>bold</b>"),
            "<p>a &amp;#zz; b &amp;#q; c <b>bold</b></p>"
        );
        assert_eq!(
            markdown("# T\n\n<thinking>\nhello *x*\n</thinking>\n\npara"),
            "<h1>T</h1>\n<p><thinking><br>\nhello <em>x</em><br>\n</thinking></p>\n<p>para</p>"
        );
        assert_eq!(markdown("[x](href)"), "<p><a href>x</a></p>");
        assert_eq!(
            markdown("<div>\nraw\n</div>\ntail"),
            "<div>\nraw\n</div>\n<p>tail</p>"
        );
        assert_eq!(markdown("a\nb  \nc"), "<p>a<br>\nb<br>\nc</p>");
        assert_eq!(
            markdown("```py\ncode <x>\n```\n"),
            "<pre><code class=\"language-py\">code &lt;x&gt;\n</code></pre>"
        );
        assert_eq!(
            markdown("1. a\n2. b\n\n    code?\n\n- c"),
            "<ol>\n<li>a</li>\n<li>\n<p>b</p>\n<p>code?</p>\n</li>\n<li>\n<p>c</p>\n</li>\n</ol>"
        );
    }
}
