//! `markdown.blockparser` + `markdown.blockprocessors` + the tables
//! extension's `TableProcessor`, in registry priority order.

use super::tree::{truthy, Id, Txt};
use super::{lstrip, rstrip, Fail, Md, R};
use crate::pyfmt::strip;
use fancy_regex::Regex;
use std::collections::VecDeque;
use std::sync::OnceLock;

const TAB: usize = 4;

struct Res {
    hashheader: Regex,
    olist: Regex,
    ulist: Regex,
    child: Regex,
    indent: Regex,
    reference: Regex,
}

pub(super) fn build(p: &str) -> Regex {
    fancy_regex::RegexBuilder::new(p)
        .backtrack_limit(usize::MAX)
        .build()
        .expect("static regex")
}

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| Res {
        hashheader: build(r"(?:^|\n)(?P<level>#{1,6})(?P<header>(?:\\.|[^\\])*?)#*(?:\n|\z)"),
        olist: build(r"\A[ ]{0,3}\d+\.[ ]+"),
        ulist: build(r"\A[ ]{0,3}[*+-][ ]+"),
        child: build(r"\A[ ]{0,3}((\d+\.)|[*+-])[ ]+(.*)"),
        indent: build(r"\A[ ]{4,7}((\d+\.)|[*+-])[ ]+"),
        reference: build(
            r#"(?m)^[ ]{0,3}\[([^\[\]]*)\]:[ ]*(?:\n[ ]*)?([^\s\x1c-\x1f]+)[ ]*(?:\n[ ]*)?((["'])(.*)\4[ ]*|\((.*)\)[ ]*)?$"#,
        ),
    })
}

fn is_match(re: &Regex, s: &str) -> R<bool> {
    re.is_match(s).map_err(|_| Fail)
}

#[derive(Clone, Copy)]
enum P {
    Empty,
    Indent,
    Code,
    Table,
    HashHeader,
    Setext,
    Hr,
    OList,
    UList,
    Quote,
    Reference,
    Paragraph,
}

const PROCS: [P; 12] = [
    P::Empty,
    P::Indent,
    P::Code,
    P::Table,
    P::HashHeader,
    P::Setext,
    P::Hr,
    P::OList,
    P::UList,
    P::Quote,
    P::Reference,
    P::Paragraph,
];

fn code_escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `BlockProcessor.detab`.
fn detab(text: &str) -> (String, String) {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut newtext: Vec<&str> = Vec::new();
    for line in &lines {
        if let Some(rest) = line.strip_prefix("    ") {
            newtext.push(rest);
        } else if strip(line).is_empty() {
            newtext.push("");
        } else {
            break;
        }
    }
    let n = newtext.len();
    (newtext.join("\n"), lines[n..].join("\n"))
}

/// `BlockProcessor.looseDetab`.
fn loose_detab(text: &str, level: usize) -> String {
    let pre = " ".repeat(TAB * level);
    text.split('\n')
        .map(|l| l.strip_prefix(pre.as_str()).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `BlockQuoteProcessor.RE = (^|\n)[ ]{0,3}>[ ]?(.*)`: match start.
fn quote_search(block: &str) -> Option<usize> {
    let mut line_start = 0;
    loop {
        let line = &block[line_start..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        let sp = line.bytes().take_while(|&b| b == b' ').count();
        if sp <= 3 && line.as_bytes().get(sp) == Some(&b'>') {
            return Some(if line_start == 0 { 0 } else { line_start - 1 });
        }
        match block[line_start..].find('\n') {
            Some(i) => line_start += i + 1,
            None => return None,
        }
    }
}

/// `BlockQuoteProcessor.clean`.
fn quote_clean(line: &str) -> String {
    if strip(line) == ">" {
        return String::new();
    }
    let sp = line.bytes().take_while(|&b| b == b' ').count();
    if sp <= 3 && line.as_bytes().get(sp) == Some(&b'>') {
        let rest = &line[sp + 1..];
        return rest.strip_prefix(' ').unwrap_or(rest).to_string();
    }
    line.to_string()
}

/// `HRProcessor.SEARCH_RE` (MULTILINE search): (start, end) of the first
/// horizontal-rule line.
fn hr_search(block: &str) -> Option<(usize, usize)> {
    let mut ls = 0;
    loop {
        let le = block[ls..]
            .find('\n')
            .map(|i| ls + i)
            .unwrap_or(block.len());
        let line = &block.as_bytes()[ls..le];
        if let Some(end) = hr_line(line) {
            return Some((ls, ls + end));
        }
        if le == block.len() {
            return None;
        }
        ls = le + 1;
    }
}

fn hr_line(line: &[u8]) -> Option<usize> {
    let sp = line.iter().take_while(|&&b| b == b' ').count();
    if sp > 3 {
        return None;
    }
    let c = *line.get(sp)?;
    if !matches!(c, b'-' | b'_' | b'*') {
        return None;
    }
    // (c+[ ]{0,2}){3,} taken greedily (the lookahead is atomic)
    let mut j = sp;
    let mut count = 0;
    loop {
        let run = line[j..].iter().take_while(|&&b| b == c).count();
        if run == 0 {
            break;
        }
        count += run;
        j += run;
        let spaces = line[j..].iter().take(2).take_while(|&&b| b == b' ').count();
        j += spaces;
    }
    if count < 3 {
        return None;
    }
    let rest = &line[j..];
    let trailing = rest.iter().take_while(|&&b| b == b' ').count();
    (trailing == rest.len()).then_some(j + trailing)
}

/// `TableProcessor.RE_END_BORDER` search: `(?<!\\)(?:\\\\)*\|$`; returns
/// the match start.
fn end_border(row: &str) -> Option<usize> {
    let b = row.as_bytes();
    if b.last() != Some(&b'|') {
        return None;
    }
    let bs = b[..b.len() - 1]
        .iter()
        .rev()
        .take_while(|&&c| c == b'\\')
        .count();
    (bs % 2 == 0).then_some(b.len() - 1 - bs)
}

impl Md {
    fn isstate(&self, s: &str) -> bool {
        self.state.last().is_some_and(|x| *x == s)
    }

    pub(super) fn parse_chunk(&mut self, parent: Id, text: &str) -> R<()> {
        let blocks = text.split("\n\n").map(String::from).collect();
        self.parse_blocks(parent, blocks)
    }

    pub(super) fn parse_blocks(&mut self, parent: Id, mut blocks: VecDeque<String>) -> R<()> {
        while !blocks.is_empty() {
            for p in PROCS {
                if self.test(p, parent, &blocks[0])? && self.run(p, parent, &mut blocks)? {
                    break;
                }
            }
        }
        Ok(())
    }

    fn one(&mut self, parent: Id, block: String) -> R<()> {
        self.parse_blocks(parent, VecDeque::from([block]))
    }

    fn test(&mut self, p: P, parent: Id, block: &str) -> R<bool> {
        let t = &self.tree;
        Ok(match p {
            P::Empty => block.is_empty() || block.starts_with('\n'),
            P::Indent => {
                block.starts_with("    ")
                    && !self.isstate("detabbed")
                    && (t.tag(parent) == "li"
                        || t.last_child(parent)
                            .is_some_and(|c| matches!(t.tag(c), "ul" | "ol")))
            }
            P::Code => block.starts_with("    "),
            P::Table => self.table_test(block),
            P::HashHeader => is_match(&res().hashheader, block)?,
            P::Setext => setext_match(block),
            P::Hr => match hr_search(block) {
                Some(m) => {
                    self.hr_match = m;
                    true
                }
                None => false,
            },
            P::OList => is_match(&res().olist, block)?,
            P::UList => is_match(&res().ulist, block)?,
            P::Quote => quote_search(block).is_some(),
            P::Reference | P::Paragraph => true,
        })
    }

    /// Returns false only where Python's `run` returned False.
    fn run(&mut self, p: P, parent: Id, blocks: &mut VecDeque<String>) -> R<bool> {
        match p {
            P::Empty => self.run_empty(parent, blocks),
            P::Indent => self.run_indent(parent, blocks)?,
            P::Code => self.run_code(parent, blocks),
            P::Table => self.run_table(parent, blocks),
            P::HashHeader => self.run_hashheader(parent, blocks)?,
            P::Setext => self.run_setext(parent, blocks),
            P::Hr => self.run_hr(parent, blocks)?,
            P::OList => self.run_list(parent, blocks, "ol")?,
            P::UList => self.run_list(parent, blocks, "ul")?,
            P::Quote => self.run_quote(parent, blocks)?,
            P::Reference => return self.run_reference(blocks),
            P::Paragraph => self.run_paragraph(parent, blocks),
        }
        Ok(true)
    }

    fn pre_code(&self, sibling: Option<Id>) -> Option<Id> {
        let s = sibling?;
        let t = &self.tree;
        if t.tag(s) == "pre" && t.len(s) > 0 && t.tag(t.nodes[s].children[0]) == "code" {
            Some(t.nodes[s].children[0])
        } else {
            None
        }
    }

    fn run_empty(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
        let block = blocks.pop_front().expect("block");
        let mut filler = "\n\n";
        if !block.is_empty() {
            filler = "\n";
            let rest = &block[1..];
            if !rest.is_empty() {
                blocks.push_front(rest.to_string());
            }
        }
        if let Some(code) = self.pre_code(self.tree.last_child(parent)) {
            let cur = self.tree.nodes[code]
                .text
                .as_ref()
                .map(|t| t.s.clone())
                .unwrap_or_else(|| "None".into());
            self.tree.nodes[code].text = Some(Txt::atomic(format!("{cur}{filler}")));
        }
    }

    fn get_level(&self, parent: Id, block: &str) -> (usize, Id) {
        let spaces = block.bytes().take_while(|&b| b == b' ').count();
        let indent_level = spaces / TAB;
        let mut level = if self.isstate("list") { 1 } else { 0 };
        let mut parent = parent;
        while indent_level > level {
            match self.tree.last_child(parent) {
                Some(c) if matches!(self.tree.tag(c), "ul" | "ol" | "li") => {
                    if matches!(self.tree.tag(c), "ul" | "ol") {
                        level += 1;
                    }
                    parent = c;
                }
                _ => break,
            }
        }
        (level, parent)
    }

    fn run_indent(&mut self, parent: Id, blocks: &mut VecDeque<String>) -> R<()> {
        let block = blocks.pop_front().expect("block");
        let (level, sibling) = self.get_level(parent, &block);
        let block = loose_detab(&block, level);
        self.state.push("detabbed");
        let parent_last = self.tree.last_child(parent);
        let sibling_last = self.tree.last_child(sibling);
        if self.tree.tag(parent) == "li" {
            match parent_last {
                Some(c) if matches!(self.tree.tag(c), "ul" | "ol") => self.one(c, block)?,
                _ => self.one(parent, block)?,
            }
        } else if self.tree.tag(sibling) == "li" {
            self.one(sibling, block)?;
        } else if sibling_last.is_some_and(|c| self.tree.tag(c) == "li") {
            let li = sibling_last.expect("li");
            if truthy(&self.tree.nodes[li].text) {
                let p = self.tree.element("p");
                self.tree.nodes[p].text = self.tree.nodes[li].text.take();
                self.tree.nodes[li].text = Some(Txt::plain(""));
                self.tree.insert(li, 0, p);
            }
            self.parse_chunk(li, &block)?;
        } else {
            let li = self.tree.sub_element(sibling, "li");
            self.one(li, block)?;
        }
        self.state.pop();
        Ok(())
    }

    fn run_code(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
        let sibling = self.tree.last_child(parent);
        let block = blocks.pop_front().expect("block");
        let the_rest;
        if let Some(code) = self.pre_code(sibling) {
            let (b, rest) = detab(&block);
            the_rest = rest;
            let cur = self.tree.nodes[code]
                .text
                .as_ref()
                .map(|t| t.s.clone())
                .unwrap_or_else(|| "None".into());
            self.tree.nodes[code].text =
                Some(Txt::atomic(format!("{cur}\n{}\n", code_escape(rstrip(&b)))));
        } else {
            let pre = self.tree.sub_element(parent, "pre");
            let code = self.tree.sub_element(pre, "code");
            let (b, rest) = detab(&block);
            the_rest = rest;
            self.tree.nodes[code].text =
                Some(Txt::atomic(format!("{}\n", code_escape(rstrip(&b)))));
        }
        if !the_rest.is_empty() {
            blocks.push_front(the_rest);
        }
    }

    // ---- tables ----

    fn split_row(&self, row: &str) -> Vec<String> {
        let mut row = row.to_string();
        if self.table_border != 0 {
            if let Some(r) = row.strip_prefix('|') {
                row = r.to_string();
            }
            if let Some(i) = end_border(&row) {
                row.truncate(i);
            }
        }
        split_cells(&row)
    }

    fn table_test(&mut self, block: &str) -> bool {
        let rows: Vec<&str> = block.split('\n').map(|r| r.trim_matches(' ')).collect();
        let mut is_table = false;
        if rows.len() > 1 {
            let header0 = rows[0];
            self.table_border = 0;
            if header0.starts_with('|') {
                self.table_border |= 1;
            }
            if end_border(header0).is_some() {
                self.table_border |= 2;
            }
            let row = self.split_row(header0);
            let row0_len = row.len();
            is_table = row0_len > 1;
            if !is_table && row0_len == 1 && self.table_border != 0 {
                for r in &rows[1..] {
                    is_table = r.starts_with('|');
                    if !is_table {
                        is_table = end_border(r).is_some();
                    }
                    if !is_table {
                        break;
                    }
                }
            }
            if is_table {
                let row = self.split_row(rows[1]);
                is_table = row.len() == row0_len
                    && row
                        .concat()
                        .chars()
                        .all(|c| matches!(c, '|' | ':' | '-' | ' '));
                if is_table {
                    self.table_sep = row;
                }
            }
        }
        is_table
    }

    fn run_table(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
        let block = blocks.pop_front().expect("block");
        let lines: Vec<&str> = block.split('\n').collect();
        let header = lines[0].trim_matches(' ');
        let rows: &[&str] = if lines.len() < 3 { &[] } else { &lines[2..] };
        let align: Vec<Option<&str>> = self
            .table_sep
            .iter()
            .map(|c| {
                let c = c.trim_matches(' ');
                if c.starts_with(':') && c.ends_with(':') {
                    Some("center")
                } else if c.starts_with(':') {
                    Some("left")
                } else if c.ends_with(':') {
                    Some("right")
                } else {
                    None
                }
            })
            .collect();
        let table = self.tree.sub_element(parent, "table");
        let thead = self.tree.sub_element(table, "thead");
        self.build_row(header, thead, &align);
        let tbody = self.tree.sub_element(table, "tbody");
        if rows.is_empty() {
            let tr = self.tree.sub_element(tbody, "tr");
            for _ in 0..align.len() {
                self.tree.sub_element(tr, "td");
            }
        } else {
            for row in rows {
                self.build_row(row.trim_matches(' '), tbody, &align);
            }
        }
    }

    fn build_row(&mut self, row: &str, parent: Id, align: &[Option<&str>]) {
        let tr = self.tree.sub_element(parent, "tr");
        let tag = if self.tree.tag(parent) == "thead" {
            "th"
        } else {
            "td"
        };
        let cells = self.split_row(row);
        for (i, a) in align.iter().enumerate() {
            let c = self.tree.sub_element(tr, tag);
            let text = cells
                .get(i)
                .map(|x| x.trim_matches(' ').to_string())
                .unwrap_or_default();
            self.tree.nodes[c].text = Some(Txt::plain(text));
            if let Some(a) = a {
                self.tree.set(c, "style", format!("text-align: {a};"));
            }
        }
    }

    // ---- headers, rules ----

    fn run_hashheader(&mut self, parent: Id, blocks: &mut VecDeque<String>) -> R<()> {
        let block = blocks.pop_front().expect("block");
        let caps = res()
            .hashheader
            .captures(&block)
            .map_err(|_| Fail)?
            .ok_or(Fail)?;
        let m = caps.get(0).expect("group 0");
        let before = &block[..m.start()];
        let after = &block[m.end()..];
        if !before.is_empty() {
            self.one(parent, before.to_string())?;
        }
        let level = caps.name("level").expect("level").as_str().len();
        let h = self.tree.sub_element(parent, &format!("h{level}"));
        let header = caps.name("header").expect("header").as_str();
        self.tree.nodes[h].text = Some(Txt::plain(strip(header)));
        if !after.is_empty() {
            let after = if self.isstate("looselist") {
                loose_detab(after, 1)
            } else {
                after.to_string()
            };
            blocks.push_front(after);
        }
        Ok(())
    }

    fn run_setext(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
        let block = blocks.pop_front().expect("block");
        let lines: Vec<&str> = block.split('\n').collect();
        let level = if lines[1].starts_with('=') { 1 } else { 2 };
        let h = self.tree.sub_element(parent, &format!("h{level}"));
        self.tree.nodes[h].text = Some(Txt::plain(strip(lines[0])));
        if lines.len() > 2 {
            blocks.push_front(lines[2..].join("\n"));
        }
    }

    fn run_hr(&mut self, parent: Id, blocks: &mut VecDeque<String>) -> R<()> {
        let block = blocks.pop_front().expect("block");
        let (start, end) = self.hr_match;
        let prelines = block[..start].trim_end_matches('\n');
        if !prelines.is_empty() {
            self.one(parent, prelines.to_string())?;
        }
        self.tree.sub_element(parent, "hr");
        let postlines = block[end..].trim_start_matches('\n');
        if !postlines.is_empty() {
            blocks.push_front(postlines.to_string());
        }
        Ok(())
    }

    // ---- lists ----

    fn get_items(&self, block: &str) -> R<Vec<String>> {
        let mut items: Vec<String> = Vec::new();
        for line in block.split('\n') {
            if let Some(c) = res().child.captures(line).map_err(|_| Fail)? {
                items.push(c.get(3).map(|m| m.as_str()).unwrap_or("").to_string());
            } else if is_match(&res().indent, line)? {
                let last = items.last_mut().ok_or(Fail)?;
                if last.starts_with("    ") {
                    last.push('\n');
                    last.push_str(line);
                } else {
                    items.push(line.to_string());
                }
            } else {
                let last = items.last_mut().ok_or(Fail)?;
                last.push('\n');
                last.push_str(line);
            }
        }
        Ok(items)
    }

    fn run_list(&mut self, parent: Id, blocks: &mut VecDeque<String>, tag: &str) -> R<()> {
        let block = blocks.pop_front().expect("block");
        let mut items: VecDeque<String> = self.get_items(&block)?.into();
        let sibling = self.tree.last_child(parent);
        let lst;
        if let Some(sib) = sibling.filter(|&s| matches!(self.tree.tag(s), "ol" | "ul")) {
            lst = sib;
            let last = self.tree.last_child(lst).ok_or(Fail)?;
            if truthy(&self.tree.nodes[last].text) {
                let p = self.tree.element("p");
                self.tree.nodes[p].text = self.tree.nodes[last].text.take();
                self.tree.nodes[last].text = Some(Txt::plain(""));
                self.tree.insert(last, 0, p);
            }
            if let Some(lch) = self.tree.last_child(last) {
                if truthy(&self.tree.nodes[lch].tail) {
                    let p = self.tree.sub_element(last, "p");
                    let tail = self.tree.nodes[lch].tail.take().expect("tail");
                    self.tree.nodes[p].text = Some(Txt::plain(lstrip(&tail.s)));
                    self.tree.nodes[lch].tail = Some(Txt::plain(""));
                }
            }
            let li = self.tree.sub_element(lst, "li");
            self.state.push("looselist");
            let first = items.pop_front().ok_or(Fail)?;
            self.one(li, first)?;
            self.state.pop();
        } else if matches!(self.tree.tag(parent), "ol" | "ul") {
            lst = parent;
        } else {
            lst = self.tree.sub_element(parent, tag);
        }
        self.state.push("list");
        for item in items {
            if item.starts_with("    ") {
                let last = self.tree.last_child(lst).ok_or(Fail)?;
                self.one(last, item)?;
            } else {
                let li = self.tree.sub_element(lst, "li");
                self.one(li, item)?;
            }
        }
        self.state.pop();
        Ok(())
    }

    // ---- quotes, references, paragraphs ----

    fn run_quote(&mut self, parent: Id, blocks: &mut VecDeque<String>) -> R<()> {
        let mut block = blocks.pop_front().expect("block");
        if let Some(start) = quote_search(&block) {
            let before = block[..start].to_string();
            self.one(parent, before)?;
            block = block[start..]
                .split('\n')
                .map(quote_clean)
                .collect::<Vec<_>>()
                .join("\n");
        }
        let quote = match self.tree.last_child(parent) {
            Some(s) if self.tree.tag(s) == "blockquote" => s,
            _ => self.tree.sub_element(parent, "blockquote"),
        };
        self.state.push("blockquote");
        self.parse_chunk(quote, &block)?;
        self.state.pop();
        Ok(())
    }

    fn run_reference(&mut self, blocks: &mut VecDeque<String>) -> R<bool> {
        let block = blocks.pop_front().expect("block");
        let caps = res().reference.captures(&block).map_err(|_| Fail)?;
        let Some(m) = caps else {
            blocks.push_front(block);
            return Ok(false);
        };
        let id = strip(m.get(1).expect("g1").as_str()).to_lowercase();
        let link = m.get(2).expect("g2").as_str();
        let link = link
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string();
        // `m.group(5) or m.group(6)`
        let title = match m.get(5).map(|x| x.as_str()) {
            Some(s) if !s.is_empty() => Some(s),
            _ => m.get(6).map(|x| x.as_str()),
        }
        .map(String::from);
        self.references.insert(id, (link, title));
        let whole = m.get(0).expect("g0");
        let (start, end) = (whole.start(), whole.end());
        if !strip(&block[end..]).is_empty() {
            blocks.push_front(block[end..].trim_start_matches('\n').to_string());
        }
        if !strip(&block[..start]).is_empty() {
            blocks.push_front(block[..start].trim_end_matches('\n').to_string());
        }
        Ok(true)
    }

    fn run_paragraph(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
        let block = blocks.pop_front().expect("block");
        if strip(&block).is_empty() {
            return;
        }
        if self.isstate("list") {
            match self.tree.last_child(parent) {
                Some(sib) => {
                    let n = &mut self.tree.nodes[sib];
                    n.tail = Some(Txt::plain(match &n.tail {
                        Some(t) if !t.s.is_empty() => format!("{}\n{}", t.s, block),
                        _ => format!("\n{block}"),
                    }));
                }
                None => {
                    let n = &mut self.tree.nodes[parent];
                    n.text = Some(Txt::plain(match &n.text {
                        Some(t) if !t.s.is_empty() => format!("{}\n{}", t.s, block),
                        _ => lstrip(&block).to_string(),
                    }));
                }
            }
        } else {
            let p = self.tree.sub_element(parent, "p");
            self.tree.nodes[p].text = Some(Txt::plain(lstrip(&block)));
        }
    }
}

/// `SetextHeaderProcessor.RE.match`: `^.*?\n(?:=+|-+)[ ]*(\n|$)` (M).
fn setext_match(block: &str) -> bool {
    let Some(nl) = block.find('\n') else {
        return false;
    };
    let rest = &block[nl + 1..];
    let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let b = line.as_bytes();
    let Some(&c) = b.first() else {
        return false;
    };
    if c != b'=' && c != b'-' {
        return false;
    }
    let run = b.iter().take_while(|&&x| x == c).count();
    b[run..].iter().all(|&x| x == b' ')
}

/// `TableProcessor._split`: split a row on pipes outside code spans.
fn split_cells(row: &str) -> Vec<String> {
    let b = row.as_bytes();
    let mut pipes: Vec<usize> = Vec::new();
    let mut tics: Vec<usize> = Vec::new();
    let mut tic_points: Vec<(usize, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'\\') {
            i += 2;
        } else if b[i] == b'\\' && b.get(i + 1) == Some(&b'`') {
            let run = b[i + 1..].iter().take_while(|&&c| c == b'`').count();
            let len = run + 1;
            tics.push(len - 1);
            tic_points.push((i, i + len - 1, 1));
            i += len;
        } else if b[i] == b'`' {
            let run = b[i..].iter().take_while(|&&c| c == b'`').count();
            tics.push(run);
            tic_points.push((i, i + run - 1, 0));
            i += run;
        } else if b[i] == b'\\' && b.get(i + 1) == Some(&b'|') {
            i += 2;
        } else if b[i] == b'|' {
            pipes.push(i);
            i += 1;
        } else {
            i += 1;
        }
    }
    let mut tic_region: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    while pos < tics.len() {
        let tic_size = tics[pos] as isize - tic_points[pos].2 as isize;
        let found = if tic_size == 0 {
            None
        } else {
            tics[pos + 1..]
                .iter()
                .position(|&t| t as isize == tic_size)
                .map(|x| x + 1)
        };
        match found {
            Some(index) => {
                tic_region.push((tic_points[pos].0, tic_points[pos + index].1));
                pos += index + 1;
            }
            None => pos += 1,
        }
    }
    let mut good: Vec<usize> = Vec::new();
    for &pipe in &pipes {
        let mut throw_out = false;
        for region in &tic_region {
            if pipe < region.0 {
                break;
            } else if region.0 <= pipe && pipe <= region.1 {
                throw_out = true;
                break;
            }
        }
        if !throw_out {
            good.push(pipe);
        }
    }
    let mut out = Vec::new();
    let mut pos = 0;
    for pipe in good {
        out.push(row[pos..pipe].to_string());
        pos = pipe + 1;
    }
    out.push(row[pos..].to_string());
    out
}
