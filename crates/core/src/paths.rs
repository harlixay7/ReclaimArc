//! Path security: archive names are hostile input.
//!
//! The engine rejects any entry whose name could escape the destination or
//! alias system resources. Only validated names reach the decoder.

use std::path::{Path, PathBuf};

use crate::error::CoreError;

/// Windows device names and aliases that are reserved even with an extension.
const DEVICE_NAMES: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "COM\u{00B9}",
    "COM\u{00B2}",
    "COM\u{00B3}",
    "LPT\u{00B9}",
    "LPT\u{00B2}",
    "LPT\u{00B3}",
];

/// Prohibited Win32 filename characters.
const PROHIBITED_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// A validated entry path: safe to join onto the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeEntry {
    /// Original name from the archive (hostile input, kept for reporting).
    pub original: String,
    /// Validated relative components.
    pub components: Vec<String>,
    /// Whether the entry is a directory.
    pub is_directory: bool,
}

impl SafeEntry {
    /// The relative path with Windows separators.
    pub fn relative(&self) -> String {
        self.components.join("\\")
    }

    /// The path under `dest` (never escapes it).
    pub fn output_path(&self, dest: &Path) -> PathBuf {
        let s = dest.to_string_lossy();
        let base = if s.len() == 2 && s.ends_with(':') {
            PathBuf::from(format!("{}\\", s))
        } else {
            dest.to_path_buf()
        };
        base.join(self.relative())
    }
}

/// Validate an archive entry name.
///
/// Rejects:
/// - absolute paths (`/`, `\`, drive letters, UNC prefixes)
/// - `..` and `.` components
/// - Windows device names and aliases (with or without extension)
/// - prohibited Win32 characters (`<`, `>`, `:`, `"`, `|`, `?`, `*`)
/// - trailing dots/spaces in components (normalization ambiguity)
/// - empty components, control characters, `\0`
pub fn validate_entry(name: &str, is_directory: bool) -> Result<SafeEntry, CoreError> {
    let trimmed = name.trim_end_matches('/').trim_end_matches('\\');
    if trimmed.is_empty() {
        return Err(CoreError::Precondition(
            "archive contains an empty path name".to_string(),
        ));
    }

    // Absolute path detection (both separator styles and drive prefixes).
    if name.starts_with('/')
        || name.starts_with('\\')
        || name.starts_with("\\\\")
        || is_drive_letter_prefix(name)
    {
        return Err(CoreError::Precondition(format!(
            "archive contains an absolute path: '{name}'"
        )));
    }
    if name.to_ascii_lowercase().starts_with("\\\\?\\") {
        return Err(CoreError::Precondition(format!(
            "archive contains an extended-length path: '{name}'"
        )));
    }

    let mut components: Vec<String> = Vec::new();
    for raw in trimmed.split(['/', '\\']) {
        if raw.is_empty() {
            return Err(CoreError::Precondition(format!(
                "archive contains an empty path component in '{name}'"
            )));
        }
        if raw == "." || raw == ".." {
            return Err(CoreError::Precondition(format!(
                "archive contains a traversal component in '{name}'"
            )));
        }
        if raw
            .chars()
            .any(|c| PROHIBITED_CHARS.contains(&c) || (c as u32) < 0x20 || c == '\0')
        {
            return Err(CoreError::Precondition(format!(
                "archive contains prohibited Win32 characters or control characters in '{name}'"
            )));
        }
        if raw.ends_with('.') || raw.ends_with(' ') {
            return Err(CoreError::Precondition(format!(
                "archive contains a trailing dot/space component in '{name}'"
            )));
        }
        // Device names are reserved with or without extension.
        let stem = raw.split('.').next().unwrap_or(raw);
        if DEVICE_NAMES
            .iter()
            .any(|d| stem.eq_ignore_ascii_case(d) || stem.to_uppercase() == d.to_uppercase())
        {
            return Err(CoreError::Precondition(format!(
                "archive contains a reserved device name in '{name}'"
            )));
        }
        if raw.len() > 255 {
            return Err(CoreError::Precondition(format!(
                "archive contains a path component longer than 255 chars: '{name}'"
            )));
        }
        components.push(raw.to_string());
    }

    Ok(SafeEntry {
        original: name.to_string(),
        components,
        is_directory,
    })
}

/// Detect a drive-letter prefix ("C:/..." or "C:\..." or "C:name").
fn is_drive_letter_prefix(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Case-insensitive collision detection across a set of entries.
///
/// Returns the indexes of colliding entries. Windows filesystems are
/// case-insensitive, so "A.txt" and "a.txt" would overwrite each other.
/// Trailing-dot/space normalization is already rejected by `validate_entry`.
pub fn find_case_collisions(names: &[(usize, String)]) -> Vec<(usize, usize)> {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut collisions = Vec::new();
    for (idx, name) in names {
        let key = name.to_lowercase();
        if let Some(prev) = seen.insert(key, *idx) {
            collisions.push((prev, *idx));
        }
    }
    collisions
}

/// The partial-file suffix for a job.
pub fn partial_suffix(job_id: &str) -> String {
    format!(".sx-partial-{job_id}")
}

/// The partial path for an entry: final path + suffix.
pub fn partial_path(final_path: &Path, job_id: &str) -> PathBuf {
    let mut name = final_path.as_os_str().to_os_string();
    name.push(partial_suffix(job_id));
    PathBuf::from(name)
}

/// Verify that no existing directory between `dest_root` and `target` is a
/// symlink, junction, or reparse point that could divert files outside `dest_root`.
pub fn ensure_no_reparse_ancestors(target: &Path, dest_root: &Path) -> Result<(), CoreError> {
    let mut current = target.to_path_buf();
    while let Some(parent) = current.parent() {
        if !parent.starts_with(dest_root) {
            break;
        }
        if parent == dest_root {
            break;
        }
        match std::fs::symlink_metadata(parent) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(CoreError::Precondition(format!(
                        "Path component '{}' is a symlink inside destination root",
                        parent.display()
                    )));
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        // FILE_ATTRIBUTE_REPARSE_POINT = 0x400
                        return Err(CoreError::Precondition(format!(
                            "Path component '{}' is a reparse point/junction inside destination root",
                            parent.display()
                        )));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Directory does not exist yet; will be created safely as normal dir
            }
            Err(e) => {
                return Err(CoreError::Precondition(format!(
                    "Cannot verify security of ancestor directory '{}': {e}",
                    parent.display()
                )));
            }
        }
        current = parent.to_path_buf();
    }
    match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CoreError::Precondition(format!(
                    "Target path '{}' is a symlink",
                    target.display()
                )));
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(CoreError::Precondition(format!(
                        "Target path '{}' is a reparse point/junction",
                        target.display()
                    )));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CoreError::Precondition(format!(
                "Cannot verify security of target path '{}': {e}",
                target.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        for evil in ["../evil", "a/../../evil", "..\\evil", "a\\..\\b"] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_absolute() {
        for evil in [
            "/etc/passwd",
            "\\windows\\system32",
            "C:\\evil",
            "C:/evil",
            "\\\\server\\share",
            "\\\\?\\C:\\x",
        ] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_device_names() {
        for evil in [
            "CON",
            "con.txt",
            "NUL",
            "nul.txt",
            "COM1",
            "COM9",
            "lpt3",
            "PRN",
            "aux.tar.gz",
            "COM¹",
            "com¹.dat",
            "COM²",
            "COM³",
            "LPT¹",
            "LPT²",
            "LPT³",
        ] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_prohibited_characters() {
        for evil in [
            "file<name.txt",
            "file>name.txt",
            "file\"name.txt",
            "file|name.txt",
            "file?name.txt",
            "file*name.txt",
            "dir/file<test.bin",
        ] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_ads() {
        for evil in ["file.txt:evil", "file.txt::$DATA", "stream:data"] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_trailing_dot_space() {
        for evil in ["file.", "file ", "dir/file. "] {
            assert!(
                validate_entry(evil, false).is_err(),
                "{evil} must be rejected"
            );
        }
    }

    #[test]
    fn accepts_normal_entries() {
        let ok = validate_entry("docs/readme.txt", false).unwrap();
        assert_eq!(ok.relative(), "docs\\readme.txt");
        assert_eq!(ok.components.len(), 2);
        let ok2 = validate_entry("ÑÑ‚Ñ€Ð¾ÐºÐ°/Ñ„Ð°Ð¹Ð».bin", false).unwrap();
        assert_eq!(ok2.components.len(), 2);
        assert!(validate_entry("emptydir", true).is_ok());
        assert!(validate_entry("a b/c.d e", false).is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_entry("", false).is_err());
        assert!(validate_entry("a//b", false).is_err());
        assert!(validate_entry("a/", true).is_ok()); // trailing slash on dir is normalized
    }

    #[test]
    fn case_collisions_detected() {
        let names = vec![
            (0usize, "A.txt".to_string()),
            (1, "a.TXT".to_string()),
            (2, "b.txt".to_string()),
            (3, "B.TXT".to_string()),
        ];
        let collisions = find_case_collisions(&names);
        assert_eq!(collisions.len(), 2);
    }

    #[test]
    fn no_false_collisions() {
        let names = vec![
            (0usize, "a.txt".to_string()),
            (1, "b.txt".to_string()),
            (2, "ab.txt".to_string()),
        ];
        assert!(find_case_collisions(&names).is_empty());
    }
}
