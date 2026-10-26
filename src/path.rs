//! Virtual paths.
//!
//! A path is a list of names under a workspace root. It never touches a real disk, so the classic
//! escapes (`..`, symlinks, device names, drive letters) do not exist; what is left is making
//! every spelling of the same path compare equal and refusing names that cannot be shown or
//! typed safely.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};

/// Longest single name, in bytes (the common file-system limit).
pub const MAX_NAME_BYTES: usize = 255;
/// Longest whole path, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Deepest nesting.
pub const MAX_DEPTH: usize = 64;

/// A normalized absolute path: `/` for the root, otherwise `/name/name` with no trailing slash.
///
/// Parsing accepts a leading slash or none (`docs/a.md` is `/docs/a.md`), collapses repeated
/// slashes, drops `.` segments, applies Unicode NFC (so `é` typed two ways is one name), and
/// refuses `..`, control characters and backslashes.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VPath(String);

impl VPath {
    /// The root, `/`.
    pub fn root() -> Self {
        VPath("/".into())
    }

    /// Normalize and validate a path; see the type docs for the rules.
    pub fn parse(input: &str) -> Result<Self> {
        if input.len() > MAX_PATH_BYTES {
            return Err(Error::InvalidPath(format!("longer than {MAX_PATH_BYTES} bytes")));
        }
        let normalized: String = input.nfc().collect();
        let mut out = String::with_capacity(normalized.len() + 1);
        let mut depth = 0;
        for segment in normalized.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            check_name(segment)?;
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Error::InvalidPath(format!("deeper than {MAX_DEPTH} levels")));
            }
            out.push('/');
            out.push_str(segment);
        }
        if out.is_empty() {
            out.push('/');
        }
        if out.len() > MAX_PATH_BYTES {
            return Err(Error::InvalidPath(format!("longer than {MAX_PATH_BYTES} bytes")));
        }
        Ok(VPath(out))
    }

    /// For paths read back from the database, which were validated when written.
    pub(crate) fn trusted(path: String) -> Self {
        VPath(path)
    }

    /// The normalized path, such as `/notes/todo.md`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for `/`.
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// Number of names in the path; the root is 0.
    pub fn depth(&self) -> i32 {
        if self.is_root() { 0 } else { self.0.matches('/').count() as i32 }
    }

    /// The last name, or `None` for the root.
    pub fn name(&self) -> Option<&str> {
        if self.is_root() { None } else { self.0.rsplit('/').next() }
    }

    /// The containing folder; `None` for the root.
    pub fn parent(&self) -> Option<VPath> {
        if self.is_root() {
            return None;
        }
        let cut = self.0.rfind('/').unwrap_or(0);
        Some(if cut == 0 { VPath::root() } else { VPath(self.0[..cut].to_string()) })
    }

    /// Every ancestor from the top down, excluding the root and the path itself.
    pub fn ancestors(&self) -> Vec<VPath> {
        let mut out = Vec::new();
        let mut at = self.parent();
        while let Some(p) = at {
            if p.is_root() {
                break;
            }
            at = p.parent();
            out.push(p);
        }
        out.reverse();
        out
    }

    /// `self` with `rel` appended: how a handle confined to `self` turns its own paths into
    /// workspace paths. `rel` is already normalized, so the result cannot leave `self`.
    pub fn join(&self, rel: &VPath) -> VPath {
        match (self.is_root(), rel.is_root()) {
            (true, _) => rel.clone(),
            (false, true) => self.clone(),
            (false, false) => VPath(format!("{}{}", self.0, rel.0)),
        }
    }

    /// `self` seen from `root`, or `None` when it is outside it.
    pub fn strip(&self, root: &VPath) -> Option<VPath> {
        if root.is_root() {
            return Some(self.clone());
        }
        if self == root {
            return Some(VPath::root());
        }
        self.0.strip_prefix(root.as_str()).filter(|rest| rest.starts_with('/')).map(|rest| VPath(rest.to_string()))
    }

    /// True when `self` is `other` or inside it.
    pub fn within(&self, other: &VPath) -> bool {
        self.strip(other).is_some()
    }

    /// The half-open range of stored paths strictly inside this folder. `/` sorts just before
    /// `0`, so with byte-order comparison every descendant `p/...` falls in `[p/, p0)`.
    pub(crate) fn subtree(&self) -> (String, String) {
        if self.is_root() { ("/".into(), "0".into()) } else { (format!("{}/", self.0), format!("{}0", self.0)) }
    }

    /// `self` moved from under `from` to under `to`.
    pub(crate) fn rebase(&self, from: &VPath, to: &VPath) -> Option<VPath> {
        self.strip(from).map(|rest| to.join(&rest))
    }
}

fn check_name(name: &str) -> Result<()> {
    if name == ".." {
        return Err(Error::InvalidPath("`..` is not allowed".into()));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(Error::InvalidPath(format!("a name is longer than {MAX_NAME_BYTES} bytes")));
    }
    if let Some(c) = name.chars().find(|c| c.is_control() || *c == '\\') {
        return Err(Error::InvalidPath(format!("names cannot contain {c:?}")));
    }
    Ok(())
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String {
        VPath::parse(s).unwrap().0
    }

    #[test]
    fn normalizes_spellings() {
        assert_eq!(p(""), "/");
        assert_eq!(p("/"), "/");
        assert_eq!(p("a/b"), "/a/b");
        assert_eq!(p("//a///b/"), "/a/b");
        assert_eq!(p("/a/./b/."), "/a/b");
        // "é" as one code point and as e + combining accent.
        assert_eq!(p("/caf\u{e9}"), p("/cafe\u{301}"));
    }

    #[test]
    fn refuses_escapes_and_unsafe_names() {
        for bad in ["/a/../b", "..", "/a\\b", "/a\u{0}b", "/a\nb", "/a\u{7f}"] {
            assert!(VPath::parse(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(VPath::parse(&format!("/{}", "x".repeat(256))).is_err());
        assert!(VPath::parse(&"/a".repeat(65)).is_err());
        assert!(VPath::parse(&"/a".repeat(64)).is_ok());
    }

    #[test]
    fn family() {
        let x = VPath::parse("/a/b/c").unwrap();
        assert_eq!(x.depth(), 3);
        assert_eq!(x.name(), Some("c"));
        assert_eq!(x.parent().unwrap().as_str(), "/a/b");
        assert_eq!(VPath::parse("/a").unwrap().parent().unwrap(), VPath::root());
        assert_eq!(VPath::root().parent(), None);
        let names: Vec<_> = x.ancestors().iter().map(|a| a.to_string()).collect();
        assert_eq!(names, ["/a", "/a/b"]);
    }

    #[test]
    fn join_and_strip_stay_inside() {
        let root = VPath::parse("/agents/a1").unwrap();
        let rel = VPath::parse("/notes/x.md").unwrap();
        let abs = root.join(&rel);
        assert_eq!(abs.as_str(), "/agents/a1/notes/x.md");
        assert_eq!(abs.strip(&root).unwrap(), rel);
        assert_eq!(root.join(&VPath::root()), root);
        assert_eq!(root.strip(&root).unwrap(), VPath::root());
        // A sibling that merely shares the prefix is outside.
        assert!(VPath::parse("/agents/a10").unwrap().strip(&root).is_none());
        assert!(abs.within(&root));
    }

    #[test]
    fn subtree_range_matches_descendants_only() {
        let (lo, hi) = VPath::parse("/a").unwrap().subtree();
        let inside = |s: &str| s >= lo.as_str() && s < hi.as_str();
        assert!(inside("/a/b"));
        assert!(inside("/a/b/c"));
        assert!(inside("/a/\u{10ffff}"));
        assert!(!inside("/a"));
        assert!(!inside("/a0"));
        assert!(!inside("/a-b"));
        assert!(!inside("/ab"));
        assert!(!inside("/b"));
    }
}
