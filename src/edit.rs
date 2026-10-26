//! Exact-text replacement, the way coding agents edit files.

use crate::error::{Error, Result};

/// Replace `old` with `new`. `old` must appear exactly once unless `replace_all` is set.
#[derive(Debug, Clone)]
pub struct Edit {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

impl Edit {
    pub fn new(old: impl Into<String>, new: impl Into<String>) -> Self {
        Edit { old: old.into(), new: new.into(), replace_all: false }
    }

    pub fn all(old: impl Into<String>, new: impl Into<String>) -> Self {
        Edit { old: old.into(), new: new.into(), replace_all: true }
    }
}

/// Apply `edits` in order, each to the result of the one before. Returns the new text and how
/// many replacements were made.
///
/// A model usually writes `\n` even when the file uses `\r\n`; when the text is not found as
/// given and the file has Windows line endings, the edit is retried with `\r\n`.
pub(crate) fn apply(text: &str, edits: &[Edit], shown: &str) -> Result<(String, usize)> {
    if edits.is_empty() {
        return Err(Error::InvalidInput("no edits given".into()));
    }
    let mut text = text.to_string();
    let mut total = 0;
    for edit in edits {
        if edit.old.is_empty() {
            return Err(Error::InvalidInput("the text to replace is empty".into()));
        }
        if edit.old == edit.new {
            return Err(Error::InvalidInput("the replacement is the same as the original".into()));
        }
        let (old, new) = match text.matches(edit.old.as_str()).count() {
            0 if text.contains("\r\n") && edit.old.contains('\n') => (crlf(&edit.old), crlf(&edit.new)),
            _ => (edit.old.clone(), edit.new.clone()),
        };
        let count = text.matches(old.as_str()).count();
        if count == 0 {
            return Err(Error::EditNotFound(shown.to_string()));
        }
        if count > 1 && !edit.replace_all {
            return Err(Error::EditAmbiguous { path: shown.to_string(), count });
        }
        text = text.replace(old.as_str(), &new);
        total += count;
    }
    Ok((text, total))
}

/// `\n` to `\r\n`, leaving existing `\r\n` alone.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Split text into lines without their terminators. A final newline does not start a new line.
pub(crate) fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// UTF-8 text without NUL bytes, or `None` for anything binary.
pub(crate) fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_unique_match() {
        let (out, n) = apply("a b c", &[Edit::new("b", "B")], "f").unwrap();
        assert_eq!((out.as_str(), n), ("a B c", 1));
    }

    #[test]
    fn refuses_missing_ambiguous_empty_and_no_op_edits() {
        assert!(matches!(apply("abc", &[Edit::new("x", "y")], "f"), Err(Error::EditNotFound(_))));
        assert!(matches!(apply("aa", &[Edit::new("a", "b")], "f"), Err(Error::EditAmbiguous { count: 2, .. })));
        assert!(matches!(apply("a", &[Edit::new("", "b")], "f"), Err(Error::InvalidInput(_))));
        assert!(matches!(apply("a", &[Edit::new("a", "a")], "f"), Err(Error::InvalidInput(_))));
        assert!(matches!(apply("a", &[], "f"), Err(Error::InvalidInput(_))));
    }

    #[test]
    fn replace_all_and_sequential_edits() {
        let (out, n) = apply("x x x", &[Edit::all("x", "y"), Edit::new("y y y", "z")], "f").unwrap();
        assert_eq!((out.as_str(), n), ("z", 4));
    }

    #[test]
    fn falls_back_to_crlf() {
        let (out, _) = apply("one\r\ntwo\r\n", &[Edit::new("one\ntwo", "1\n2")], "f").unwrap();
        assert_eq!(out, "1\r\n2\r\n");
    }

    #[test]
    fn splits_lines() {
        assert_eq!(lines(""), Vec::<&str>::new());
        assert_eq!(lines("\n"), vec![""]);
        assert_eq!(lines("a"), vec!["a"]);
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("a\r\nb\n\nc"), vec!["a", "b", "", "c"]);
    }

    #[test]
    fn detects_binary() {
        assert!(as_text(b"plain").is_some());
        assert!(as_text(b"nul\0").is_none());
        assert!(as_text(&[0xff, 0xfe]).is_none());
    }
}
