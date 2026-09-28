//! `markdown.htmlparser.HTMLExtractor` on top of CPython 3.11's
//! `html.parser.HTMLParser` with Python-Markdown's monkey patches, as the
//! `html_block` preprocessor runs it (`convert_charrefs=False`). Indices
//! are code points, like Python's, because the parser mixes line/offset
//! bookkeeping with slicing.

use super::{is_block_level, Stash};
use crate::pyfmt::is_space;

const CDATA_CONTENT_ELEMENTS: [&str; 2] = ["script", "style"];

pub struct Extractor<'a> {
    stash: &'a mut Stash,
    raw: Vec<char>,
    lineno: usize,
    offset: usize,
    lineno_start_cache: Vec<usize>,
    inraw: bool,
    intail: bool,
    stack: Vec<String>,
    cache: Vec<String>,
    pub cleandoc: Vec<String>,
    cdata_elem: Option<String>,
    starttag_text: Option<String>,
}

fn s(chars: &[char]) -> String {
    chars.iter().collect()
}

/// Python slice `raw[a:b]` with clamping.
fn sl(raw: &[char], a: usize, b: usize) -> &[char] {
    let b = b.min(raw.len());
    let a = a.min(b);
    &raw[a..b]
}

fn starts(raw: &[char], i: usize, p: &str) -> bool {
    let mut j = i;
    for c in p.chars() {
        if raw.get(j) != Some(&c) {
            return false;
        }
        j += 1;
    }
    true
}

fn find_char(raw: &[char], c: char, from: usize) -> Option<usize> {
    if from >= raw.len() {
        return None;
    }
    raw[from..].iter().position(|&x| x == c).map(|p| p + from)
}

fn find_str(raw: &[char], p: &str, from: usize) -> Option<usize> {
    let pat: Vec<char> = p.chars().collect();
    if from > raw.len() || raw.len() - from < pat.len() {
        return None;
    }
    (from..=raw.len() - pat.len()).find(|&i| raw[i..i + pat.len()] == pat[..])
}

fn is_ws5(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ')
}

/// `starttagopen` (patched): `<[a-zA-Z]|</>`.
fn starttagopen(raw: &[char], i: usize) -> bool {
    raw.get(i) == Some(&'<')
        && (raw.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) || starts(raw, i, "</>"))
}

/// Patched `locatetagend` matched at `pos` (which holds an ASCII letter);
/// returns the match end.
fn locatetagend(raw: &[char], pos: usize) -> usize {
    let n = raw.len();
    let mut j = pos + 1;
    while j < n && !matches!(raw[j], '`' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        j += 1;
    }
    while j < n && matches!(raw[j], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/') {
        j += 1;
    }
    loop {
        // (?<=['"\t\n\r\f /])[^`\t\n\r\f />][^\t\n\r\f /=>]*
        if j == 0 || j >= n {
            break;
        }
        if !matches!(
            raw[j - 1],
            '\'' | '"' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/'
        ) {
            break;
        }
        if matches!(raw[j], '`' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
            break;
        }
        j += 1;
        while j < n && !matches!(raw[j], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '=' | '>') {
            j += 1;
        }
        // (?:=(?:'[^']*'|"[^"]*"|(?!['"])[^>\t\n\r\f ]*))?
        if j < n && raw[j] == '=' {
            let v = j + 1;
            match raw.get(v) {
                Some(&q @ ('\'' | '"')) => {
                    if let Some(close) = find_char(raw, q, v + 1) {
                        j = close + 1;
                    }
                }
                _ => {
                    let mut e = v;
                    while e < n && !matches!(raw[e], '>' | '\t' | '\n' | '\r' | '\x0c' | ' ') {
                        e += 1;
                    }
                    j = e;
                }
            }
        }
        while j < n && matches!(raw[j], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/') {
            j += 1;
        }
    }
    if j < n && raw[j] == '>' {
        j += 1;
    }
    j
}

/// `tagfind_tolerant` at `pos`: (name end, match end).
fn tagfind(raw: &[char], pos: usize) -> (usize, usize) {
    let n = raw.len();
    let mut j = pos + 1;
    while j < n && !matches!(raw[j], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        j += 1;
    }
    let name_end = j;
    loop {
        if j < n && (is_ws5(raw[j]) || (raw[j] == '/' && raw.get(j + 1) != Some(&'>'))) {
            j += 1;
        } else {
            break;
        }
    }
    (name_end, j)
}

/// `attrfind_tolerant` at `k`: match end, if it matches.
fn attrfind(raw: &[char], k: usize) -> Option<usize> {
    let n = raw.len();
    if k == 0 || k >= n {
        return None;
    }
    if !matches!(
        raw[k - 1],
        '\'' | '"' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/'
    ) {
        return None;
    }
    if matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        return None;
    }
    let mut j = k + 1;
    while j < n && !matches!(raw[j], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '=' | '>') {
        j += 1;
    }
    // ([\t\n\r\f ]*=[\t\n\r\f ]*('[^']*'|"[^"]*"|(?!['"])[^>\t\n\r\f ]*))?
    let mut e = j;
    while e < n && is_ws5(raw[e]) {
        e += 1;
    }
    if e < n && raw[e] == '=' {
        let mut v = e + 1;
        while v < n && is_ws5(raw[v]) {
            v += 1;
        }
        match raw.get(v) {
            Some(&q @ ('\'' | '"')) => {
                if let Some(close) = find_char(raw, q, v + 1) {
                    j = close + 1;
                }
            }
            _ => {
                let mut x = v;
                while x < n && !matches!(raw[x], '>' | '\t' | '\n' | '\r' | '\x0c' | ' ') {
                    x += 1;
                }
                j = x;
            }
        }
    }
    loop {
        if j < n && (is_ws5(raw[j]) || (raw[j] == '/' && raw.get(j + 1) != Some(&'>'))) {
            j += 1;
        } else {
            break;
        }
    }
    Some(j)
}

/// `charref`: `&#(?:[0-9]+|[xX][0-9a-fA-F]+)[^0-9a-fA-F]` at `i`.
fn charref(raw: &[char], i: usize) -> Option<usize> {
    let n = raw.len();
    let p = i + 2;
    let mut j = p;
    while j < n && raw[j].is_ascii_digit() {
        j += 1;
    }
    if j > p && j < n && !raw[j].is_ascii_hexdigit() {
        return Some(j + 1);
    }
    if p < n && matches!(raw[p], 'x' | 'X') {
        let mut j = p + 1;
        while j < n && raw[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j > p + 1 && j < n && !raw[j].is_ascii_hexdigit() {
            return Some(j + 1);
        }
    }
    None
}

/// Patched `entityref`: `&([a-zA-Z][-.a-zA-Z0-9]*);` at `i`.
fn entityref(raw: &[char], i: usize) -> Option<usize> {
    let n = raw.len();
    let mut j = i + 1;
    if j >= n || !raw[j].is_ascii_alphabetic() {
        return None;
    }
    j += 1;
    while j < n && (raw[j].is_ascii_alphanumeric() || raw[j] == '-' || raw[j] == '.') {
        j += 1;
    }
    (j < n && raw[j] == ';').then_some(j + 1)
}

/// `blank_line_re = ^([ ]*\n){2}` matched at the start of `rest`.
fn blank_lines(rest: &[char]) -> bool {
    let mut j = 0;
    for _ in 0..2 {
        while j < rest.len() && rest[j] == ' ' {
            j += 1;
        }
        if rest.get(j) != Some(&'\n') {
            return false;
        }
        j += 1;
    }
    true
}

impl<'a> Extractor<'a> {
    pub fn new(stash: &'a mut Stash) -> Self {
        Extractor {
            stash,
            raw: Vec::new(),
            lineno: 1,
            offset: 0,
            lineno_start_cache: vec![0],
            inraw: false,
            intail: false,
            stack: Vec::new(),
            cache: Vec::new(),
            cleandoc: Vec::new(),
            cdata_elem: None,
            starttag_text: None,
        }
    }

    pub fn feed(&mut self, data: &str) {
        self.raw.extend(data.chars());
        self.goahead(false);
    }

    pub fn close(&mut self) {
        self.goahead(true);
        if !self.raw.is_empty() {
            let d = s(&self.raw);
            self.handle_data(&d);
        }
        if !self.cache.is_empty() {
            let joined = self.cache.concat();
            let ph = self.stash.store(joined);
            self.cleandoc.push(ph);
            self.cache.clear();
        }
    }

    fn updatepos(&mut self, i: usize, j: usize) -> usize {
        if i >= j {
            return j;
        }
        let seg = &self.raw[i..j];
        let nlines = seg.iter().filter(|&&c| c == '\n').count();
        if nlines > 0 {
            self.lineno += nlines;
            let pos = i + seg.iter().rposition(|&c| c == '\n').expect("counted");
            self.offset = j - (pos + 1);
        } else {
            self.offset += j - i;
        }
        j
    }

    fn line_offset(&mut self) -> usize {
        let start = self.lineno_start_cache.len() - 1;
        for ii in start..self.lineno.saturating_sub(1) {
            let last = self.lineno_start_cache[ii];
            let lf = find_char(&self.raw, '\n', last).unwrap_or(self.raw.len());
            self.lineno_start_cache.push(lf + 1);
        }
        self.lineno_start_cache[self.lineno - 1]
    }

    fn at_line_start(&mut self) -> bool {
        if self.offset == 0 {
            return true;
        }
        if self.offset > 3 {
            return false;
        }
        let lo = self.line_offset();
        sl(&self.raw, lo, lo + self.offset)
            .iter()
            .all(|&c| is_space(c))
    }

    fn get_endtag_text(&mut self, tag: &str) -> String {
        let start = self.line_offset() + self.offset;
        match find_char(&self.raw, '>', start) {
            Some(p) => s(&self.raw[start..p + 1]),
            None => format!("</{tag}>"),
        }
    }

    fn rest_after(&mut self, len: usize) -> bool {
        let p = self.line_offset() + self.offset + len;
        blank_lines(sl(&self.raw, p, self.raw.len()))
    }

    // ---- handlers (HTMLExtractor) ----

    fn handle_starttag(&mut self, tag: &str) {
        if tag == "hr" {
            self.handle_startendtag(tag);
            return;
        }
        if is_block_level(tag) && (self.intail || (self.at_line_start() && !self.inraw)) {
            self.inraw = true;
            self.cleandoc.push("\n".into());
        }
        let text = self.starttag_text.clone().unwrap_or_default();
        if self.inraw {
            self.stack.push(tag.to_string());
            self.cache.push(text);
        } else {
            self.cleandoc.push(text);
            if CDATA_CONTENT_ELEMENTS.contains(&tag) {
                self.cdata_elem = None;
            }
        }
    }

    fn handle_endtag(&mut self, tag: &str) {
        let text = self.get_endtag_text(tag);
        if self.inraw {
            let tlen = text.chars().count();
            self.cache.push(text);
            if self.stack.iter().any(|t| t == tag) {
                while let Some(t) = self.stack.pop() {
                    if t == tag {
                        break;
                    }
                }
            }
            if self.stack.is_empty() {
                if self.rest_after(tlen) {
                    self.cache.push("\n".into());
                } else {
                    self.intail = true;
                }
                self.inraw = false;
                let joined = self.cache.concat();
                let ph = self.stash.store(joined);
                self.cleandoc.push(ph);
                self.cleandoc.push("\n\n".into());
                self.cache.clear();
            }
        } else {
            self.cleandoc.push(text);
        }
    }

    fn handle_data(&mut self, data: &str) {
        if self.intail && data.contains('\n') {
            self.intail = false;
        }
        if self.inraw {
            self.cache.push(data.to_string());
        } else {
            self.cleandoc.push(data.to_string());
        }
    }

    fn handle_empty_tag(&mut self, data: String, is_block: bool) {
        let mut data = data;
        if self.inraw || self.intail {
            self.cache.push(data);
        } else if self.at_line_start() && is_block {
            if self.rest_after(data.chars().count()) {
                data.push('\n');
            } else {
                self.intail = true;
            }
            let item = self.cleandoc.last().map(String::as_str).unwrap_or("");
            if !item.ends_with("\n\n") && item.ends_with('\n') {
                self.cleandoc.push("\n".into());
            }
            let ph = self.stash.store(data);
            self.cleandoc.push(ph);
            self.cleandoc.push("\n\n".into());
        } else {
            self.cleandoc.push(data);
        }
    }

    fn handle_startendtag(&mut self, tag: &str) {
        let text = self.starttag_text.clone().unwrap_or_default();
        self.handle_empty_tag(text, is_block_level(tag));
    }

    fn handle_comment(&mut self, data: String) {
        self.handle_empty_tag(format!("<!--{data}-->"), true);
    }

    fn handle_decl(&mut self, data: String) {
        self.handle_empty_tag(format!("<!{data}>"), true);
    }

    fn handle_pi(&mut self, data: String) {
        self.handle_empty_tag(format!("<?{data}?>"), true);
    }

    fn unknown_decl(&mut self, data: String) {
        let end = if data.starts_with("CDATA[") {
            "]]>"
        } else {
            "]>"
        };
        self.handle_empty_tag(format!("<![{data}{end}"), true);
    }

    // ---- parse_* (patched/overridden) ----

    fn parse_starttag(&mut self, i: usize) -> isize {
        if starts(&self.raw, i, "</>") {
            self.handle_data("</>");
            return (i + 3) as isize;
        }
        self.starttag_text = None;
        let endpos = {
            let j = locatetagend(&self.raw, i + 1);
            if self.raw[j - 1] != '>' {
                None
            } else {
                Some(j)
            }
        };
        let Some(endpos) = endpos else {
            let d = s(&self.raw[i..i + 1]);
            self.handle_data(&d);
            return (i + 1) as isize;
        };
        self.starttag_text = Some(s(&self.raw[i..endpos]));
        let (name_end, mut k) = tagfind(&self.raw, i + 1);
        let tag = s(&self.raw[i + 1..name_end]).to_lowercase();
        while k < endpos {
            match attrfind(&self.raw, k) {
                Some(e) => k = e,
                None => break,
            }
        }
        let end = s(sl(&self.raw, k, endpos));
        let end = crate::pyfmt::strip(&end);
        if end != ">" && end != "/>" {
            let d = s(&self.raw[i..endpos]);
            self.handle_data(&d);
            return endpos as isize;
        }
        if end.ends_with("/>") {
            self.handle_startendtag(&tag);
        } else {
            if CDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) {
                self.cdata_elem = Some(tag.clone());
            }
            self.handle_starttag(&tag);
        }
        endpos as isize
    }

    fn parse_endtag(&mut self, i: usize) -> isize {
        let start = sl(&self.raw, i, i + 3);
        let c = *start.last().expect("'</'");
        if start.len() < 3 || !c.is_ascii_alphabetic() {
            let d = s(sl(&self.raw, i, i + 2));
            self.handle_data(&d);
            return (i + 2) as isize;
        }
        if find_char(&self.raw, '>', i + 2).is_none() {
            return -1;
        }
        let j = locatetagend(&self.raw, i + 2);
        if self.raw[j - 1] != '>' {
            return -1;
        }
        let (name_end, _) = tagfind(&self.raw, i + 2);
        let tag = s(&self.raw[i + 2..name_end]).to_lowercase();
        self.handle_endtag(&tag);
        self.cdata_elem = None;
        j as isize
    }

    /// `commentclose = --!?>` searched from `from`: (start, end).
    fn commentclose(&self, from: usize) -> Option<(usize, usize)> {
        let raw = &self.raw;
        let mut p = from;
        while p + 2 < raw.len() {
            if raw[p] == '-' && raw[p + 1] == '-' {
                if raw.get(p + 2) == Some(&'>') {
                    return Some((p, p + 3));
                }
                if raw.get(p + 2) == Some(&'!') && raw.get(p + 3) == Some(&'>') {
                    return Some((p, p + 4));
                }
            }
            p += 1;
        }
        None
    }

    fn parse_comment(&mut self, i: usize) -> isize {
        match self.commentclose(i + 4) {
            None => {
                self.handle_data("<");
                (i + 1) as isize
            }
            Some((j, end)) => {
                let d = s(&self.raw[i + 4..j]);
                self.handle_comment(d);
                end as isize
            }
        }
    }

    fn parse_pi(&mut self, i: usize) -> isize {
        if self.at_line_start() || self.intail {
            return match find_str(&self.raw, "?>", i + 2) {
                None => -1,
                Some(j) => {
                    let d = s(&self.raw[i + 2..j]);
                    self.handle_pi(d);
                    (j + 2) as isize
                }
            };
        }
        self.handle_data("<?");
        (i + 2) as isize
    }

    fn parse_bogus_comment(&mut self, i: usize) -> isize {
        match find_char(&self.raw, '>', i + 2) {
            None => -1,
            Some(p) => {
                let pos = p + 1;
                let d = s(&self.raw[i..pos]);
                self.handle_empty_tag(d, false);
                pos as isize
            }
        }
    }

    fn parse_html_declaration(&mut self, i: usize) -> isize {
        if self.at_line_start() || self.intail {
            if starts(&self.raw, i, "<![") && !starts(&self.raw, i, "<![CDATA[") {
                let r = self.parse_bogus_comment(i);
                if r == -1 {
                    let d = s(&self.raw[i..i + 1]);
                    self.handle_data(&d);
                    return (i + 1) as isize;
                }
                return r;
            }
            if starts(&self.raw, i, "<!--") {
                return self.parse_comment(i);
            }
            if starts(&self.raw, i, "<![CDATA[") {
                return match find_str(&self.raw, "]]>", i + 9) {
                    None => -1,
                    Some(j) => {
                        let d = s(&self.raw[i + 3..j]);
                        self.unknown_decl(d);
                        (j + 3) as isize
                    }
                };
            }
            if is_doctype(&self.raw, i) {
                return match find_char(&self.raw, '>', i + 9) {
                    None => -1,
                    Some(g) => {
                        let d = s(&self.raw[i + 2..g]);
                        self.handle_decl(d);
                        (g + 1) as isize
                    }
                };
            }
            return self.parse_bogus_comment(i);
        }
        self.handle_data("<!");
        (i + 2) as isize
    }

    /// `self.interesting.search(rawdata, i)`.
    fn interesting(&self, i: usize) -> Option<usize> {
        let raw = &self.raw;
        match &self.cdata_elem {
            None => (i..raw.len()).find(|&p| raw[p] == '&' || raw[p] == '<'),
            Some(elem) => {
                let e: Vec<char> = elem.chars().collect();
                (i..raw.len()).find(|&p| {
                    if raw[p] != '<' || raw.get(p + 1) != Some(&'/') {
                        return false;
                    }
                    for (k, &c) in e.iter().enumerate() {
                        match raw.get(p + 2 + k) {
                            Some(x) if x.to_ascii_lowercase() == c && x.is_ascii() => {}
                            _ => return false,
                        }
                    }
                    raw.get(p + 2 + e.len()).is_some_and(|&c| {
                        matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>')
                    })
                })
            }
        }
    }

    fn goahead(&mut self, end: bool) {
        let n = self.raw.len();
        let mut i = 0usize;
        while i < n {
            let j = match self.interesting(i) {
                Some(j) => j,
                None => {
                    if self.cdata_elem.is_some() {
                        break;
                    }
                    n
                }
            };
            if i < j {
                let d = s(&self.raw[i..j]);
                self.handle_data(&d);
            }
            i = self.updatepos(i, j);
            if i == n {
                break;
            }
            if self.raw[i] == '<' {
                let mut k: isize = if starttagopen(&self.raw, i) {
                    self.parse_starttag(i)
                } else if starts(&self.raw, i, "</") {
                    self.parse_endtag(i)
                } else if starts(&self.raw, i, "<!--") {
                    self.parse_comment(i)
                } else if starts(&self.raw, i, "<?") {
                    self.parse_pi(i)
                } else if starts(&self.raw, i, "<!") {
                    self.parse_html_declaration(i)
                } else if i + 1 < n || end {
                    self.handle_data("<");
                    (i + 1) as isize
                } else {
                    break;
                };
                if k < 0 {
                    if !end {
                        break;
                    }
                    if starttagopen(&self.raw, i) {
                    } else if starts(&self.raw, i, "</") {
                        if i + 2 == n {
                            self.handle_data("</");
                        }
                        // else: patched endtagopen `</[a-zA-Z]?` always matches
                    } else if starts(&self.raw, i, "<!--") {
                        let mut j = n;
                        for suffix in ["--!", "--", "-"] {
                            let sfx: Vec<char> = suffix.chars().collect();
                            if n >= i + 4 + sfx.len() && self.raw[n - sfx.len()..] == sfx[..] {
                                j -= sfx.len();
                                break;
                            }
                        }
                        let d = s(sl(&self.raw, i + 4, j));
                        self.handle_comment(d);
                    } else if starts(&self.raw, i, "<![CDATA[") {
                        let d = s(&self.raw[i + 3..]);
                        self.unknown_decl(d);
                    } else if is_doctype(&self.raw, i) {
                        let d = s(&self.raw[i + 2..]);
                        self.handle_decl(d);
                    } else if starts(&self.raw, i, "<!") {
                        let d = s(&self.raw[i + 2..]);
                        self.handle_comment(d);
                    } else if starts(&self.raw, i, "<?") {
                        let d = s(&self.raw[i + 2..]);
                        self.handle_pi(d);
                    }
                    k = n as isize;
                }
                i = self.updatepos(i, k as usize);
            } else if starts(&self.raw, i, "&#") {
                if let Some(mend) = charref(&self.raw, i) {
                    let name = s(&self.raw[i + 2..mend - 1]);
                    self.handle_empty_tag(format!("&#{name};"), false);
                    let mut k = mend;
                    if self.raw[k - 1] != ';' {
                        k -= 1;
                    }
                    i = self.updatepos(i, k);
                    continue;
                }
                if self.raw[i..].contains(&';') {
                    let d = s(&self.raw[i..i + 2]);
                    self.handle_data(&d);
                    i = self.updatepos(i, i + 2);
                }
                break;
            } else if self.raw[i] == '&' {
                if let Some(mend) = entityref(&self.raw, i) {
                    let name = s(&self.raw[i + 1..mend - 1]);
                    self.handle_empty_tag(format!("&{name};"), false);
                    i = self.updatepos(i, mend);
                    continue;
                }
                if i + 1 < n {
                    self.handle_data("&");
                    i = self.updatepos(i, i + 1);
                } else {
                    break;
                }
            } else {
                unreachable!("interesting.search() lied");
            }
        }
        if end && i < n {
            let d = s(&self.raw[i..n]);
            self.handle_data(&d);
            i = self.updatepos(i, n);
        }
        self.raw.drain(..i);
    }
}

fn is_doctype(raw: &[char], i: usize) -> bool {
    let seg = sl(raw, i, i + 9);
    let lower: String = seg.iter().collect::<String>().to_lowercase();
    lower == "<!doctype"
}
