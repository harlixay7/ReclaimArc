//! Volume discovery: find all parts of a multi-volume RAR archive by name
//! convention, and normalize to the first volume.

use std::path::{Path, PathBuf};

use crate::error::ArchiveError;

/// A discovered volume set: ordered part paths plus which index was the
/// originally-given path.
#[derive(Debug, Clone)]
pub struct VolumeSet {
    /// Ordered part paths (index 0 = first volume).
    pub paths: Vec<PathBuf>,
    /// The original path the user supplied.
    pub given: PathBuf,
}

/// Try to parse `name` as a new-numbering part: `foo.partNN.rar`.
fn parse_new_numbering(name: &str) -> Option<(String, u64)> {
    let lower = name.to_lowercase();
    if !lower.ends_with(".rar") {
        return None;
    }
    let stem = &lower[..lower.len() - 4];
    let pos = stem.rfind(".part")?;
    let digits = &stem[pos + 5..];
    if digits.is_empty() || digits.len() > 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let num: u64 = digits.parse().ok()?;
    Some((stem[..pos].to_string(), num))
}

/// Try to parse `name` as an old-numbering part: `foo.rar`, `foo.r00`,
/// `foo.r01`, ... (also `foo.s00` for recovery volumes, ignored here).
///
/// Returns `(stem, part_index)` where `part_index` is 0 for the main `.rar`
/// volume and `n + 1` for `.rNN`.
fn parse_old_numbering(name: &str) -> Option<(String, u64)> {
    let lower = name.to_lowercase();
    if lower.ends_with(".rar") {
        let stem = lower.trim_end_matches(".rar");
        return Some((stem.to_string(), 0));
    }
    if lower.len() >= 4 && lower.ends_with(".r") {
        return None;
    }
    let dot = lower.rfind('.')?;
    let ext = &lower[dot + 1..];
    if ext.len() >= 2 && ext.starts_with('r') && ext[1..].chars().all(|c| c.is_ascii_digit()) {
        let num: u64 = ext[1..].parse().ok()?;
        // .rNN is part NN+2 (after the main .rar): .r00 = part 2, ...
        return Some((lower[..dot].to_string(), num + 1));
    }
    None
}

/// Discover the full ordered volume list for an archive given any part path.
pub fn discover_volumes(first_or_any: &Path) -> Result<VolumeSet, ArchiveError> {
    let given = first_or_any.to_path_buf();
    if !given.exists() {
        return Err(ArchiveError::open(format!(
            "archive '{}' does not exist",
            given.display()
        )));
    }
    let dir = given
        .parent()
        .ok_or_else(|| ArchiveError::open("archive path has no parent directory"))?
        .to_path_buf();

    let name = given
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| ArchiveError::open("archive path has no file name"))?;

    // Identify the numbering scheme.
    let (stem, _given_num): (String, u64) = if let Some(parsed) = parse_new_numbering(&name) {
        parsed
    } else if let Some(parsed) = parse_old_numbering(&name) {
        parsed
    } else {
        return Err(ArchiveError::open(format!(
            "'{}' is not a recognized RAR volume name",
            given.display()
        )));
    };

let entries = std::fs::read_dir(&dir).map_err(|e| {
        ArchiveError::open(format!("cannot list directory '{}': {e}", dir.display()))
    })?;

    let mut parts: Vec<(u64, PathBuf)> = Vec::new();
    let is_new_numbering = parse_new_numbering(&name).is_some();

    for entry in entries.flatten() {
        let p = entry.path();
        let n = p
            .file_name()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some((s, num)) = parse_new_numbering(&n) {
            if s == stem {
                parts.push((num, p));
            }
        } else if let Some((s, num)) = parse_old_numbering(&n) {
            if s == stem {
                parts.push((num, p));
            }
        }
    }

    if parts.is_empty() {
        return Err(ArchiveError::open(format!(
            "no RAR volumes found for '{}'",
            given.display()
        )));
    }

    parts.sort_by_key(|(num, _)| *num);

    // Sanity: part numbers must be contiguous from 0/1.
    let expected_first = if is_new_numbering { 1 } else { 0 };
    let first_num = parts[0].0;
    if first_num != expected_first {
        return Err(ArchiveError::open(format!(
            "volume set for '{}' starts at part {first_num} (expected {expected_first}) — is the first volume missing?",
            given.display()
        )));
    }
    for (i, (num, _)) in parts.iter().enumerate() {
        let want = expected_first + i as u64;
        if *num != want {
            return Err(ArchiveError::missing_volume(format!(
                "volume {} is missing (found part {num} instead)",
                want
            )));
        }
    }

    let paths: Vec<PathBuf> = parts.into_iter().map(|(_, p)| p).collect();

    // The user may have supplied a non-first part: use the discovered first.
    Ok(VolumeSet { paths, given })
}

/// Sort-friendly formatting for diagnostics.
pub fn describe(set: &VolumeSet) -> String {
    set.paths
        .iter()
        .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

#[test]
    fn numbering_parsers() {
        assert_eq!(
            parse_new_numbering("movie.part01.rar"),
            Some(("movie".to_string(), 1))
        );
        assert_eq!(
            parse_new_numbering("movie.part10.rar"),
            Some(("movie".to_string(), 10))
        );
        assert_eq!(parse_new_numbering("movie.part1.rar"), Some(("movie".to_string(), 1)));
        assert_eq!(parse_new_numbering("movie.rar"), None);
        // Old scheme part indexes: .rar = 0, .r00 = 1, .r01 = 2, .r99 = 100.
        assert_eq!(parse_old_numbering("movie.rar"), Some(("movie".to_string(), 0)));
        assert_eq!(parse_old_numbering("movie.r00"), Some(("movie".to_string(), 1)));
        assert_eq!(parse_old_numbering("movie.r01"), Some(("movie".to_string(), 2)));
        assert_eq!(parse_old_numbering("movie.r99"), Some(("movie".to_string(), 100)));
        assert_eq!(parse_old_numbering("movie.txt"), None);
    }

    #[test]
    fn discover_sorts_new_numbering() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["a.part01.rar", "a.part03.rar", "a.part02.rar"];
        for n in &names {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let set = discover_volumes(&dir.path().join("a.part02.rar")).unwrap();
        assert_eq!(set.paths.len(), 3);
        let file_names: Vec<String> = set
            .paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(file_names, ["a.part01.rar", "a.part02.rar", "a.part03.rar"]);
    }

    #[test]
    fn discover_detects_missing_middle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.part01.rar"), b"x").unwrap();
        std::fs::write(dir.path().join("b.part03.rar"), b"x").unwrap();
        let err = discover_volumes(&dir.path().join("b.part01.rar")).unwrap_err();
        assert!(matches!(err, ArchiveError::MissingVolume(_)), "got: {err}");
    }

#[test]
    fn discover_sorts_old_numbering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.rar"), b"x").unwrap();
        std::fs::write(dir.path().join("c.r00"), b"x").unwrap();
        std::fs::write(dir.path().join("c.r01"), b"x").unwrap();
        let set = discover_volumes(&dir.path().join("c.rar")).unwrap();
        let file_names: Vec<String> = set
            .paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(file_names, ["c.rar", "c.r00", "c.r01"]);
    }
}
