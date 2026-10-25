//! Validation of file paths taken from untrusted metainfo.
//!
//! Every path in a `.torrent` is attacker-controlled. Before Dravity ever
//! touches the filesystem, each component is checked so that joining the
//! components below a download directory can never escape it — on any
//! platform. Symlink-safe opening (`O_NOFOLLOW`, dir fds) belongs to the
//! storage layer and is out of scope here.

use std::fmt;
use std::path::PathBuf;

/// Why a path or path component was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PathError {
    /// The path has no components.
    #[error("path is empty")]
    Empty,
    /// A component is empty (e.g. produced by `a//b`).
    #[error("path component is empty")]
    EmptyComponent,
    /// A component is `..`, or would become `..`/`.` on Windows, which strips
    /// trailing dots and spaces.
    #[error("path component {0:?} refers to a parent or current directory")]
    Traversal(String),
    /// A component contains a path separator (`/` or `\`).
    #[error("path component {0:?} contains a path separator")]
    Separator(String),
    /// A component is absolute or carries a Windows drive/UNC prefix.
    #[error("path component {0:?} is absolute or has a drive prefix")]
    Absolute(String),
    /// A component contains a NUL or other ASCII control character.
    #[error("path component {0:?} contains a control character")]
    ControlCharacter(String),
}

/// Checks that one path component is safe to join below a download directory.
///
/// Rejected: empty strings, `.`/`..` (and Windows aliases such as `.. ` or
/// `...`), `/` and `\` separators, drive prefixes like `C:`, and control
/// characters including NUL.
///
/// # Errors
/// Returns the [`PathError`] describing the first problem found.
///
/// ```
/// use dravity_core::path::validate_component;
///
/// assert!(validate_component("movie.mkv").is_ok());
/// assert!(validate_component("..").is_err());
/// assert!(validate_component("/etc").is_err());
/// assert!(validate_component("C:").is_err());
/// ```
pub fn validate_component(c: &str) -> Result<(), PathError> {
    if c.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if c.chars().any(char::is_control) {
        return Err(PathError::ControlCharacter(c.to_owned()));
    }
    if c.starts_with('/') || c.starts_with('\\') {
        return Err(PathError::Absolute(c.to_owned()));
    }
    let bytes = c.as_bytes();
    if bytes.len() >= 2
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
    {
        return Err(PathError::Absolute(c.to_owned()));
    }
    if c.contains(['/', '\\']) {
        return Err(PathError::Separator(c.to_owned()));
    }
    // Windows silently strips trailing dots and spaces, so "...", ". ." or
    // ".. " would resolve to "." or "..". Reject any component made only of
    // dots and spaces.
    if c.chars().all(|ch| ch == '.' || ch == ' ') {
        return Err(PathError::Traversal(c.to_owned()));
    }
    Ok(())
}

/// A validated, relative path made of safe components.
///
/// Construct with [`SafePath::new`]; joining it under any base directory with
/// [`SafePath::to_path_buf`] stays inside that directory.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SafePath(Vec<String>);

impl SafePath {
    /// Validates every component.
    ///
    /// # Errors
    /// Returns [`PathError::Empty`] for no components, or the first component error.
    pub fn new<I, S>(components: I) -> Result<Self, PathError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let components: Vec<String> = components.into_iter().map(Into::into).collect();
        if components.is_empty() {
            return Err(PathError::Empty);
        }
        for c in &components {
            validate_component(c)?;
        }
        Ok(Self(components))
    }

    /// The components.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.0
    }

    /// Builds a relative [`PathBuf`] using the platform separator.
    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.iter().collect()
    }
}

impl fmt::Display for SafePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

impl fmt::Debug for SafePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SafePath({:?})", self.0.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Component;

    use super::*;

    #[test]
    fn accepts_normal_names() {
        for ok in [
            "a",
            "file.txt",
            ".hidden",
            "a..b",
            "..a",
            "name with spaces",
            "日本語",
            "C",
            "ab:c",
        ] {
            assert!(validate_component(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_dangerous_components() {
        let cases = [
            ("", PathError::EmptyComponent),
            (".", PathError::Traversal(".".into())),
            ("..", PathError::Traversal("..".into())),
            ("...", PathError::Traversal("...".into())),
            (".. ", PathError::Traversal(".. ".into())),
            ("/etc", PathError::Absolute("/etc".into())),
            ("\\\\server", PathError::Absolute("\\\\server".into())),
            ("a/b", PathError::Separator("a/b".into())),
            ("a\\..", PathError::Separator("a\\..".into())),
            ("C:", PathError::Absolute("C:".into())),
            ("c:evil", PathError::Absolute("c:evil".into())),
            // Drive-relative on Windows.
            ("a:b", PathError::Absolute("a:b".into())),
            ("a\0b", PathError::ControlCharacter("a\0b".into())),
            ("a\nb", PathError::ControlCharacter("a\nb".into())),
        ];
        for (input, err) in cases {
            assert_eq!(validate_component(input), Err(err), "{input:?}");
        }
    }

    #[test]
    fn safe_path_never_escapes() {
        let p = SafePath::new(["dir", "sub", "file.bin"]).unwrap();
        assert_eq!(p.to_string(), "dir/sub/file.bin");
        assert!(
            p.to_path_buf()
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        );
        assert_eq!(SafePath::new(Vec::<String>::new()), Err(PathError::Empty));
        assert!(SafePath::new(["ok", ".."]).is_err());
    }
}
