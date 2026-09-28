//! Every tmux session name the API accepts goes through here: one validator
//! for all entry points (INC-049) and exact targets (INC-050).
//!
//! A bare `-t name` makes tmux fall back to prefix and pattern matching when
//! `name` doesn't exist, so a request for a vanished `csm-bench-a` would type
//! into `csm-bench-ab`. `=name` matches the session name exactly; pane and
//! window commands take `=name:`, which is that session's current window and
//! active pane.

use std::borrow::Cow;

/// `^[A-Za-z0-9._-]{1,200}$`. Stricter than Python's `_is_safe_tmux_name`,
/// whose `$` also accepts one trailing newline.
pub fn is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Exact session target (`has-session`, `kill-session`, `list-clients`).
pub fn session(name: &str) -> String {
    format!("={name}")
}

/// Exact pane/window target (`send-keys`, `capture-pane`, `display-message`,
/// `resize-window`, `set-option`, `respawn-pane`).
pub fn pane(name: &str) -> String {
    format!("={name}:")
}

/// tmux's stderr when the target (or the whole server) isn't there.
pub fn is_missing(stderr: &str) -> bool {
    let e = stderr.to_ascii_lowercase();
    e.contains("can't find")
        || e.contains("no server running")
        || e.contains("error connecting to")
        || e.contains("no such file or directory")
}

/// An argument tmux must take literally. tmux treats an argument that ends
/// in `;` as a command separator, so `send-keys x; kill-server` would run a
/// second command. `\;` at the end is tmux's escape for a literal `;`.
pub fn literal(arg: &str) -> Cow<'_, str> {
    match arg.strip_suffix(';') {
        Some(head) => Cow::Owned(format!("{head}\\;")),
        None => Cow::Borrowed(arg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_safe("claude-1d693e35"));
        assert!(is_safe("a.b_c-d"));
        assert!(is_safe(&"a".repeat(200)));
        for bad in [
            "", "\n", "abc\n", "a b", "a;b", "a:b", "=a", "é", "*", "../x",
        ] {
            assert!(!is_safe(bad), "{bad:?}");
        }
        assert!(!is_safe(&"a".repeat(201)));
    }

    #[test]
    fn targets() {
        assert_eq!(session("csm-bench-a"), "=csm-bench-a");
        assert_eq!(pane("csm-bench-a"), "=csm-bench-a:");
        assert!(is_missing("can't find session: =csm-bench-a\n"));
        assert!(is_missing("no server running on /tmp/x/default\n"));
        assert!(!is_missing("pane in mode"));
    }

    #[test]
    fn literal_semicolons() {
        assert_eq!(literal("hello"), "hello");
        assert_eq!(literal("a;b"), "a;b");
        assert_eq!(literal("x;"), "x\\;");
        assert_eq!(literal(";"), "\\;");
        assert_eq!(literal("a\\;"), "a\\\\;");
    }
}
