//! A minimal `xml.etree.ElementTree` for the Markdown port: an arena of
//! elements with text/tail (each possibly an `AtomicString`) and the
//! `markdown.serializers.to_html_string` serializer.

pub type Id = usize;

#[derive(Clone, Debug)]
pub struct Txt {
    pub s: String,
    /// `markdown.util.AtomicString`: never run through inline patterns.
    pub atomic: bool,
}

impl Txt {
    pub fn plain(s: impl Into<String>) -> Self {
        Txt {
            s: s.into(),
            atomic: false,
        }
    }
    pub fn atomic(s: impl Into<String>) -> Self {
        Txt {
            s: s.into(),
            atomic: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub tag: String,
    pub text: Option<Txt>,
    pub tail: Option<Txt>,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Id>,
}

#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

/// Python truthiness of an optional text.
pub fn truthy(t: &Option<Txt>) -> bool {
    t.as_ref().is_some_and(|t| !t.s.is_empty())
}

impl Tree {
    pub fn element(&mut self, tag: &str) -> Id {
        self.nodes.push(Node {
            tag: tag.to_string(),
            ..Default::default()
        });
        self.nodes.len() - 1
    }

    pub fn sub_element(&mut self, parent: Id, tag: &str) -> Id {
        let id = self.element(tag);
        self.nodes[parent].children.push(id);
        id
    }

    pub fn append(&mut self, parent: Id, child: Id) {
        self.nodes[parent].children.push(child);
    }

    /// `list.insert` semantics (index past the end appends).
    pub fn insert(&mut self, parent: Id, index: usize, child: Id) {
        let ch = &mut self.nodes[parent].children;
        let i = index.min(ch.len());
        ch.insert(i, child);
    }

    pub fn len(&self, id: Id) -> usize {
        self.nodes[id].children.len()
    }

    pub fn last_child(&self, id: Id) -> Option<Id> {
        self.nodes[id].children.last().copied()
    }

    pub fn tag(&self, id: Id) -> &str {
        &self.nodes[id].tag
    }

    pub fn set(&mut self, id: Id, key: &str, value: String) {
        let attrs = &mut self.nodes[id].attrs;
        if let Some(a) = attrs.iter_mut().find(|(k, _)| k == key) {
            a.1 = value;
        } else {
            attrs.push((key.to_string(), value));
        }
    }

    /// `elem.iter()` — pre-order, the element itself first.
    pub fn iter(&self, root: Id) -> Vec<Id> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            for &c in self.nodes[id].children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// `''.join(elem.itertext())`.
    pub fn itertext(&self, id: Id) -> String {
        let mut out = String::new();
        self.itertext_into(id, &mut out);
        out
    }

    fn itertext_into(&self, id: Id, out: &mut String) {
        let n = &self.nodes[id];
        if let Some(t) = &n.text {
            out.push_str(&t.s);
        }
        for &c in &n.children {
            self.itertext_into(c, out);
            if let Some(t) = &self.nodes[c].tail {
                out.push_str(&t.s);
            }
        }
    }

    /// `to_html_string(elem)` — includes the element's tail, like
    /// `_serialize_html`.
    pub fn to_html(&self, id: Id) -> String {
        let mut out = String::new();
        self.serialize(id, &mut out);
        out
    }

    fn serialize(&self, id: Id, out: &mut String) {
        let n = &self.nodes[id];
        let tag = n.tag.as_str();
        out.push('<');
        out.push_str(tag);
        if !n.attrs.is_empty() {
            let mut items: Vec<&(String, String)> = n.attrs.iter().collect();
            items.sort();
            for (k, v) in items {
                let v = escape_attrib_html(v);
                if *k == v {
                    out.push(' ');
                    out.push_str(&v);
                } else {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&v);
                    out.push('"');
                }
            }
        }
        out.push('>');
        let lower = tag.to_lowercase();
        if let Some(t) = &n.text {
            if !t.s.is_empty() {
                if lower == "script" || lower == "style" {
                    out.push_str(&t.s);
                } else {
                    out.push_str(&escape_cdata(&t.s));
                }
            }
        }
        for &c in &n.children {
            self.serialize(c, out);
        }
        if !HTML_EMPTY.contains(&lower.as_str()) {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        if let Some(t) = &n.tail {
            if !t.s.is_empty() {
                out.push_str(&escape_cdata(&t.s));
            }
        }
    }
}

/// `xml.etree.ElementTree.HTML_EMPTY` (Python 3.11).
const HTML_EMPTY: [&str; 17] = [
    "area", "base", "basefont", "br", "col", "embed", "frame", "hr", "img", "input", "isindex",
    "link", "meta", "param", "source", "track", "wbr",
];

/// `RE_AMP = re.compile(r'&(?!(?:\#[0-9]+|\#x[0-9a-f]+|[0-9a-z]+);)', re.I)`:
/// an `&` that doesn't start an entity-like reference.
fn amp_is_bare(s: &str, i: usize) -> bool {
    let rest = &s.as_bytes()[i + 1..];
    let digits = |b: &[u8], hex: bool| {
        b.iter()
            .take_while(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
            .count()
    };
    if rest.first() == Some(&b'#') {
        let n = digits(&rest[1..], false);
        if n > 0 && rest.get(1 + n) == Some(&b';') {
            return false;
        }
        if matches!(rest.get(1), Some(b'x' | b'X')) {
            let n = digits(&rest[2..], true);
            if n > 0 && rest.get(2 + n) == Some(&b';') {
                return false;
            }
        }
    }
    // [0-9a-z]+ under IGNORECASE also matches U+0130, U+0131, U+017F, U+212A.
    let tail = &s[i + 1..];
    let mut n = 0;
    for (j, c) in tail.char_indices() {
        if c.is_ascii_alphanumeric() || matches!(c, '\u{130}' | '\u{131}' | '\u{17f}' | '\u{212a}')
        {
            n = j + c.len_utf8();
        } else {
            break;
        }
    }
    !(n > 0 && tail[n..].starts_with(';'))
}

fn escape_amp(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut last = 0;
    for (i, _) in s.match_indices('&') {
        out.push_str(&s[last..i]);
        if amp_is_bare(s, i) {
            out.push_str("&amp;");
        } else {
            out.push('&');
        }
        last = i + 1;
    }
    out.push_str(&s[last..]);
    out
}

/// `_escape_cdata`.
pub fn escape_cdata(s: &str) -> String {
    let mut t = if s.contains('&') {
        escape_amp(s)
    } else {
        s.to_string()
    };
    if t.contains('<') {
        t = t.replace('<', "&lt;");
    }
    if t.contains('>') {
        t = t.replace('>', "&gt;");
    }
    t
}

/// `_escape_attrib_html`.
pub fn escape_attrib_html(s: &str) -> String {
    let mut t = escape_cdata(s);
    if t.contains('"') {
        t = t.replace('"', "&quot;");
    }
    t
}
