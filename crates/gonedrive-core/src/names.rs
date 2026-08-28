//! OneDrive name validation.
//!
//! ext4 accepts almost anything; OneDrive does not. Uploading a locally-legal
//! name that the service rejects is one of the most common ways a Linux sync
//! client fails halfway through a tree, so every local name is checked *before*
//! it enters the upload queue. Rejected names are reported to the user — never
//! silently skipped and never auto-renamed, because renaming a user's file
//! behind their back is worse than refusing to sync it.
//!
//! Sources: the "Invalid file names and file types in OneDrive and SharePoint"
//! restrictions, which also apply to the Graph API.

use crate::error::NameError;

/// Characters OneDrive rejects anywhere in a name.
const ILLEGAL_CHARS: &[char] = &['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// Names that are rejected outright, case-insensitively.
const RESERVED_NAMES: &[&str] = &[
    ".lock",
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "_vti_",
    "desktop.ini",
];

/// Prefixes OneDrive reserves; these are Office lock/temp files.
const RESERVED_PREFIXES: &[&str] = &["~$", ".~"];

/// A single path component's maximum length.
pub const MAX_NAME_LEN: usize = 255;

/// Validate one path component (a single file or folder name, never a path).
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    if name.starts_with(' ') {
        return Err(NameError::LeadingSpace);
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(NameError::TrailingSpaceOrDot);
    }
    if name
        .chars()
        .any(|c| ILLEGAL_CHARS.contains(&c) || c.is_control())
    {
        return Err(NameError::IllegalCharacter);
    }
    for prefix in RESERVED_PREFIXES {
        if name.starts_with(prefix) {
            return Err(NameError::ReservedPrefix);
        }
    }
    // Reserved device names are rejected both bare and with an extension
    // (`CON` and `CON.txt` are both refused), matching the service.
    let stem = name.split('.').next().unwrap_or(name);
    for reserved in RESERVED_NAMES {
        if name.eq_ignore_ascii_case(reserved) || stem.eq_ignore_ascii_case(reserved) {
            return Err(NameError::ReservedName);
        }
    }
    Ok(())
}

/// Key used for the case-insensitive uniqueness constraint.
///
/// OneDrive treats `Report.docx` and `report.docx` as the same item while ext4
/// does not, so both the database index and the in-memory sync bookkeeping key
/// on this rather than on the raw name.
pub fn fold_case(name: &str) -> String {
    name.to_lowercase()
}

/// Build the conflict-copy name the client uses when both sides changed.
///
/// Mirrors the Windows client's `name-HOST 的冲突副本-timestamp.ext` shape so a
/// user with both clients sees a consistent convention.
pub fn conflict_copy_name(name: &str, host: &str, when: time::OffsetDateTime) -> String {
    let (stem, ext) = split_extension(name);
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}{:02}{:02}",
        when.year(),
        u8::from(when.month()),
        when.day(),
        when.hour(),
        when.minute(),
        when.second()
    );
    // Host names can legally contain characters OneDrive rejects, so sanitise
    // the one part of this name that we did not author.
    let host: String = host
        .chars()
        .filter(|c| !ILLEGAL_CHARS.contains(c) && !c.is_control())
        .collect();
    match ext {
        Some(ext) => format!("{stem} ({host} 的冲突副本 {stamp}).{ext}"),
        None => format!("{stem} ({host} 的冲突副本 {stamp})"),
    }
}

/// A variant of `candidate` that does not already exist on disk, inserting
/// " (2)", " (3)", … before the extension until the name is free.
///
/// Conflict-copy names are stamped to the second, so two conflicts on the same
/// file within one second would otherwise resolve to the same name and the
/// second rename would clobber the first — losing the first preserved copy.
pub fn free_path(candidate: &std::path::Path) -> std::path::PathBuf {
    if !candidate.exists() {
        return candidate.to_path_buf();
    }
    let name = candidate
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (stem, ext) = split_extension(&name);
    for n in 2..u32::MAX {
        let numbered = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = candidate.with_file_name(numbered);
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate.to_path_buf()
}

/// Split a name into stem and extension, treating dotfiles as extension-less.
pub fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        // A leading dot makes it a hidden file, not an extension.
        Some(0) | None => (name, None),
        Some(idx) => (&name[..idx], Some(&name[idx + 1..])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["report.docx", "照片 2024.jpg", "a", "file.tar.gz", ".hidden"] {
            assert!(validate_name(name).is_ok(), "rejected {name}");
        }
    }

    #[test]
    fn fold_case_matches_names_that_differ_only_in_case() {
        // The whole point: OneDrive is case-insensitive, ext4 is not, so two
        // names the service treats as one must fold to one key.
        assert_eq!(fold_case("Report.TXT"), fold_case("report.txt"));
        assert_eq!(fold_case("MixedCase"), fold_case("mixedcase"));
        assert_eq!(fold_case("照片"), fold_case("照片")); // non-cased scripts unchanged
        // Distinct names must not collide.
        assert_ne!(fold_case("report.txt"), fold_case("report2.txt"));
    }

    #[test]
    fn fold_case_is_idempotent() {
        for name in ["Report.TXT", "straße", "İstanbul", "照片 2024.JPG"] {
            assert_eq!(fold_case(&fold_case(name)), fold_case(name), "not idempotent: {name}");
        }
    }

    #[test]
    fn free_path_dodges_existing_conflict_copies() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("report 的冲突副本.docx");
        // Nothing there yet: the base name is free.
        assert_eq!(free_path(&base), base);

        // Once it exists, the next call must not return it — two conflicts in
        // the same second must not clobber each other.
        std::fs::write(&base, b"first").unwrap();
        let p2 = free_path(&base);
        assert_eq!(p2.file_name().unwrap().to_string_lossy(), "report 的冲突副本 (2).docx");

        std::fs::write(&p2, b"second").unwrap();
        let p3 = free_path(&base);
        assert_eq!(p3.file_name().unwrap().to_string_lossy(), "report 的冲突副本 (3).docx");
        // The first copy is untouched.
        assert_eq!(std::fs::read(&base).unwrap(), b"first");
    }

    #[test]
    fn rejects_illegal_characters() {
        for name in ["a:b", "a/b", "a?b", "a|b", "a*b", "a<b", "a>b", "a\"b", "a\\b"] {
            assert_eq!(validate_name(name), Err(NameError::IllegalCharacter), "{name}");
        }
        assert_eq!(validate_name("a\nb"), Err(NameError::IllegalCharacter));
    }

    #[test]
    fn rejects_trailing_space_or_dot() {
        assert_eq!(validate_name("report "), Err(NameError::TrailingSpaceOrDot));
        assert_eq!(validate_name("report."), Err(NameError::TrailingSpaceOrDot));
        assert_eq!(validate_name(" report"), Err(NameError::LeadingSpace));
    }

    #[test]
    fn rejects_reserved_names_with_and_without_extension() {
        assert_eq!(validate_name("CON"), Err(NameError::ReservedName));
        assert_eq!(validate_name("con.txt"), Err(NameError::ReservedName));
        assert_eq!(validate_name("desktop.ini"), Err(NameError::ReservedName));
        assert_eq!(validate_name(".lock"), Err(NameError::ReservedName));
        assert_eq!(validate_name("~$budget.xlsx"), Err(NameError::ReservedPrefix));
        // Not reserved just because it starts with the same letters.
        assert!(validate_name("console.log").is_ok());
    }

    #[test]
    fn rejects_empty_and_overlong() {
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name(&"x".repeat(256)), Err(NameError::TooLong));
        assert!(validate_name(&"x".repeat(255)).is_ok());
    }

    #[test]
    fn extension_split_treats_dotfiles_as_extensionless() {
        assert_eq!(split_extension("a.txt"), ("a", Some("txt")));
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", Some("gz")));
        assert_eq!(split_extension(".bashrc"), (".bashrc", None));
        assert_eq!(split_extension("README"), ("README", None));
    }

    #[test]
    fn conflict_copy_name_is_itself_valid() {
        let when = datetime!(2026-08-27 14:05:09 UTC);
        let name = conflict_copy_name("report.docx", "thinkpad", when);
        assert_eq!(name, "report (thinkpad 的冲突副本 2026-08-27 140509).docx");
        assert!(validate_name(&name).is_ok());

        // A host name containing illegal characters must not poison the result.
        let name = conflict_copy_name("notes", "my:host", when);
        assert!(validate_name(&name).is_ok());
        assert!(!name.contains(':'));
    }
}
