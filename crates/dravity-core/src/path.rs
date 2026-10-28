//! Validation of file paths taken from untrusted metainfo.
//!
//! Every path in a `.torrent` is attacker-controlled. Before Dravity ever
//! touches the filesystem, each component is checked so that joining the
//! components below a download directory can never escape it — on any
//! platform. On Windows, names that the filesystem would reinterpret are
//! refused as well: reserved device names (`CON`, `NUL`, `COM1`, …, also
//! with an extension), characters Windows forbids (`<>:"|?*`, where `:`
//! would open an NTFS alternate data stream) and trailing dots or spaces
//! (which Windows strips, aliasing two different names). [`Platform`] selects
//! the rules; [`validate_component`] uses the platform Dravity runs on.
//!
//! Symlink-safe opening (`O_NOFOLLOW`, directory descriptors) belongs to the
//! storage layer.

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
    /// A component is a reserved Windows device name such as `CON`, `NUL` or
    /// `COM1`, with or without an extension.
    #[error("path component {0:?} is a reserved device name on Windows")]
    Reserved(String),
    /// A component contains a character Windows does not allow in file
    /// names (`<`, `>`, `:`, `"`, `|`, `?`, `*`).
    #[error("path component {0:?} contains a character Windows does not allow")]
    InvalidCharacter(String),
    /// A component ends with a dot or a space, which Windows strips.
    #[error("path component {0:?} ends with a dot or space")]
    TrailingDotOrSpace(String),
    /// A component is longer than [`MAX_COMPONENT_LEN`] bytes.
    #[error("path component {0:?} is longer than 255 bytes")]
    TooLong(String),
}

/// Longest accepted component, in bytes of UTF-8. Common filesystems
/// (ext4, APFS, NTFS, …) limit a name to 255 bytes or UTF-16 units.
pub const MAX_COMPONENT_LEN: usize = 255;

/// Whose file naming rules a path must satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Platform {
    /// Unix-like systems: only the portable checks.
    Unix,
    /// Windows: the portable checks plus reserved device names, forbidden
    /// characters and trailing dots or spaces.
    Windows,
}

impl Platform {
    /// The platform this build runs on.
    pub const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// Device names Windows reserves in every directory, case-insensitively and
/// regardless of extension (`nul.txt` is the `NUL` device too).
const WINDOWS_RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
/// Numbered devices (`COM0`–`COM9`, `LPT0`–`LPT9`, and the superscript digits
/// `¹²³` Windows also treats as numbers).
const WINDOWS_NUMBERED: [&str; 2] = ["COM", "LPT"];

/// Whether `c` names a reserved Windows device.
fn is_windows_reserved(c: &str) -> bool {
    // Windows ignores everything from the first dot, and trailing spaces
    // before it (`"con .txt"` is still `CON`).
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    if WINDOWS_RESERVED
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
    {
        return true;
    }
    WINDOWS_NUMBERED.iter().any(|prefix| {
        stem.get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && stem.get(3..).is_some_and(|digit| {
                matches!(digit, "¹" | "²" | "³")
                    || (digit.len() == 1 && digit.bytes().all(|b| b.is_ascii_digit()))
            })
    })
}

/// Checks that one path component is safe to join below a download
/// directory on the platform Dravity runs on ([`Platform::CURRENT`]).
///
/// # Errors
/// Returns the [`PathError`] describing the first problem found; see
/// [`validate_component_for`].
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
    validate_component_for(c, Platform::CURRENT)
}

/// Checks that one path component is safe to join below a download
/// directory on `platform`.
///
/// Rejected everywhere: empty strings, `.`/`..` (and Windows aliases such as
/// `.. ` or `...`), `/` and `\` separators, drive prefixes like `C:`,
/// control characters including NUL, and names longer than
/// [`MAX_COMPONENT_LEN`] bytes. On [`Platform::Windows`] also: reserved
/// device names, `<>:"|?*`, and a trailing dot or space.
///
/// # Errors
/// Returns the [`PathError`] describing the first problem found.
///
/// ```
/// use dravity_core::path::{Platform, validate_component_for};
///
/// assert!(validate_component_for("aux.c", Platform::Unix).is_ok());
/// assert!(validate_component_for("aux.c", Platform::Windows).is_err());
/// assert!(validate_component_for("file:stream", Platform::Windows).is_err());
/// ```
pub fn validate_component_for(c: &str, platform: Platform) -> Result<(), PathError> {
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
    if c.len() > MAX_COMPONENT_LEN {
        return Err(PathError::TooLong(c.to_owned()));
    }
    if platform == Platform::Windows {
        if c.contains(['<', '>', ':', '"', '|', '?', '*']) {
            return Err(PathError::InvalidCharacter(c.to_owned()));
        }
        if c.ends_with(['.', ' ']) {
            return Err(PathError::TrailingDotOrSpace(c.to_owned()));
        }
        if is_windows_reserved(c) {
            return Err(PathError::Reserved(c.to_owned()));
        }
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
    /// Validates every component for the current platform.
    ///
    /// # Errors
    /// Returns [`PathError::Empty`] for no components, or the first component error.
    pub fn new<I, S>(components: I) -> Result<Self, PathError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new_for(components, Platform::CURRENT)
    }

    /// Validates every component for `platform`.
    ///
    /// # Errors
    /// Returns [`PathError::Empty`] for no components, or the first component error.
    pub fn new_for<I, S>(components: I, platform: Platform) -> Result<Self, PathError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let components: Vec<String> = components.into_iter().map(Into::into).collect();
        if components.is_empty() {
            return Err(PathError::Empty);
        }
        for c in &components {
            validate_component_for(c, platform)?;
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
            "aux.c",
            "trailing.",
        ] {
            assert!(validate_component_for(ok, Platform::Unix).is_ok(), "{ok}");
        }
        for ok in [
            "a",
            "file.txt",
            ".hidden",
            "a..b",
            "日本語",
            "CONSOLE",
            "auxiliary.c",
            "COM10",
            "LPT",
            "nul_file",
        ] {
            assert!(
                validate_component_for(ok, Platform::Windows).is_ok(),
                "{ok}"
            );
        }
    }

    #[test]
    fn windows_rules() {
        let w = |c: &str| validate_component_for(c, Platform::Windows);
        for reserved in [
            "CON",
            "con",
            "Prn",
            "AUX",
            "nul",
            "COM1",
            "com9",
            "LPT0",
            "lpt5",
            "COM¹",
            "LPT³",
            "nul.txt",
            "CON.tar.gz",
            "con .txt",
            "aux.c",
        ] {
            assert_eq!(
                w(reserved),
                Err(PathError::Reserved(reserved.into())),
                "{reserved}"
            );
        }
        for bad in ["a<b", "a>b", "ab:c", "q\"?", "a|b", "what?", "star*"] {
            assert_eq!(
                w(bad),
                Err(PathError::InvalidCharacter(bad.into())),
                "{bad}"
            );
        }
        for bad in ["name.", "name ", "a. "] {
            assert_eq!(
                w(bad),
                Err(PathError::TrailingDotOrSpace(bad.into())),
                "{bad}"
            );
        }
        // Portable checks still apply first.
        assert_eq!(w(".."), Err(PathError::Traversal("..".into())));
        assert!(SafePath::new_for(["dir", "NUL"], Platform::Windows).is_err());
        assert!(SafePath::new_for(["dir", "NUL"], Platform::Unix).is_ok());
    }

    #[test]
    fn long_components_are_rejected() {
        let max = "a".repeat(MAX_COMPONENT_LEN);
        assert!(validate_component(&max).is_ok());
        let long = "a".repeat(MAX_COMPONENT_LEN + 1);
        assert_eq!(
            validate_component(&long),
            Err(PathError::TooLong(long.clone()))
        );
        // Multi-byte characters count in bytes.
        let wide = "é".repeat(128);
        assert_eq!(
            validate_component(&wide),
            Err(PathError::TooLong(wide.clone()))
        );
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
            for platform in [Platform::Unix, Platform::Windows] {
                assert_eq!(
                    validate_component_for(input, platform),
                    Err(err.clone()),
                    "{input:?}"
                );
            }
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
