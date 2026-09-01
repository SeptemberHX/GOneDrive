//! Shared classification of the processes that read placeholders.
//!
//! The privileged helper and the unprivileged daemon must agree on which
//! processes are background indexers (whose blocked reads must never pull
//! content down), which are bulk copiers (that must hydrate fully on open, or a
//! sparse-aware copy silently writes zeros), and which are browsers or
//! thumbnailers (that describe a file without downloading it). These lists were
//! duplicated in both crates and had already drifted apart; this is the single
//! source of truth both link against.
//!
//! `comm` is the kernel's process name, truncated to 15 bytes. The background
//! and browsing lists are matched as a *prefix* of `comm`; the copier list is
//! matched *exactly*, so "cpuinfo" is never mistaken for "cp".

/// Indexers, thumbnail sweeps, and scanners whose blocked reads must not pull a
/// file's content down. Matched as a prefix of `comm`.
pub const BACKGROUND_READERS: &[&str] = &[
    "localsearch", // GNOME's file indexer (formerly tracker-miner-fs)
    "tracker-miner",
    "tracker-extract",
    "tracker-store",
    "baloo_file",  // KDE's indexer (covers baloo_file_extractor by prefix)
    "thumbnailer", // gnome-desktop / tumbler thumbnail generation
    "tumblerd",
    "gsd-housekeep", // GNOME's disk-usage sweep
    "updatedb",      // locate(1) database builders
    "plocate",
    "mlocate",
    "clamd", // on-access antivirus
    "freshclam",
    // Recursive/search tools and shell completion inspect directory entries as
    // an implementation detail. Treating their opens as foreground navigation
    // would populate an entire lazy tree one directory at a time.
    "find",
    "fd",
    "rg",
    "grep",
    "du",
    "tree",
    "locate",
    "bash",
    "zsh",
    "fish",
];

/// Tools that copy whole files. Being sparse-aware, they would otherwise SEEK
/// over a placeholder's holes and copy zeros, so they must hydrate fully on
/// open. Matched exactly, never as a prefix.
pub const BULK_COPIERS: &[&str] = &[
    "cp", "mv", "install", "rsync", "tar", "dd", "cpio", "rclone", "7z", "7za", "zip", "gzip",
    "bsdtar", "pax", "cksum",
];

/// The file manager, its preview pane, and every thumbnailer — anything that
/// reads a file to *describe* it. Dedicated thumbnailers stay entirely offline.
/// Nautilus is also a copier, so the helper distinguishes a real copy by its
/// writable destination descriptor. Ordinary browsing stays offline; a proven
/// copy receives the complete source and never receives placeholder zeroes.
/// Matched as a prefix of `comm`.
pub const BROWSING_READERS: &[&str] = &[
    "nautilus",
    "sushi",  // GNOME's preview (spacebar)
    "glycin", // GNOME's image loader/thumbnailer sandbox
    "gdk-pixbuf-thum",
    "gnome-thumbnail",
    "gst-video-thumb",
    "gst-audio-thumb",
    "heif-thumbnaile",
    "papers-thumbnai",
    "totem-video-thu",
    "ffmpegthumbnail",
    "evince-thumbnai",
];

/// Browsers that also execute explicit user copies. Unlike a dedicated
/// thumbnailer, they cannot safely be given zeroes for a missing range.
pub const FILE_MANAGER_READERS: &[&str] = &["nautilus"];

/// Synthetic reader name emitted by the helper after it observes that Nautilus
/// holds a writable destination for the same source file. Unlike a metadata
/// sniff, this read must receive every real byte before it is allowed through.
pub const FILE_MANAGER_COPY_READER: &str = "nautilus-copy";

/// Interactive viewers that issue speculative reads for neighbouring files.
///
/// A gallery application commonly opens the selected image and several nearby
/// images in the same process. Process identity therefore cannot tell a real
/// open from preloading. These readers are served progressively, one kernel-
/// requested range at a time, instead of promoting their second read to a
/// whole-file hydration.
pub const PROGRESSIVE_READERS: &[&str] = &["loupe", "eog", "gwenview", "shotwell"];

/// The matching background-reader prefix, if `comm` is one.
pub fn is_background_reader(comm: &str) -> Option<&'static str> {
    BACKGROUND_READERS
        .iter()
        .find(|prefix| comm.starts_with(*prefix))
        .copied()
}

/// Whether `comm` is a whole-file copier that must hydrate on open.
pub fn is_bulk_copier(comm: &str) -> bool {
    BULK_COPIERS.contains(&comm)
}

/// Whether `comm` is a file manager or thumbnailer that must never download.
pub fn is_browsing_reader(comm: &str) -> bool {
    BROWSING_READERS
        .iter()
        .any(|prefix| comm.starts_with(prefix))
}

pub fn is_file_manager_reader(comm: &str) -> bool {
    FILE_MANAGER_READERS
        .iter()
        .any(|prefix| comm.starts_with(prefix))
}

pub fn is_file_manager_copy_reader(reader: &str) -> bool {
    reader == FILE_MANAGER_COPY_READER
}

/// Whether `comm` should be served only the ranges it actually requests.
pub fn is_progressive_reader(comm: &str) -> bool {
    PROGRESSIVE_READERS
        .iter()
        .any(|prefix| comm.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexers_are_background_readers_by_prefix() {
        assert!(is_background_reader("tracker-miner-f").is_some());
        assert!(is_background_reader("baloo_file_extr").is_some()); // by prefix
        assert!(is_background_reader("localsearch-3").is_some());
        assert!(is_background_reader("nautilus").is_none());
    }

    #[test]
    fn copiers_match_exactly_never_as_prefix() {
        assert!(is_bulk_copier("cp"));
        assert!(is_bulk_copier("rsync"));
        assert!(
            !is_bulk_copier("cpuinfo"),
            "a prefix hit would download for nothing"
        );
        assert!(!is_bulk_copier("nautilus"));
    }

    #[test]
    fn browsers_and_thumbnailers_match_by_prefix() {
        assert!(is_browsing_reader("nautilus"));
        assert!(is_browsing_reader("gdk-pixbuf-thum")); // truncated to 15
        assert!(is_browsing_reader("gst-video-thumb"));
        assert!(!is_browsing_reader("cp"));
        assert!(!is_browsing_reader("libreoffice"));
        assert!(is_file_manager_reader("nautilus"));
        assert!(!is_file_manager_reader("gdk-pixbuf-thum"));
        assert!(is_file_manager_copy_reader(FILE_MANAGER_COPY_READER));
        assert!(!is_file_manager_copy_reader("nautilus"));
    }

    #[test]
    fn gallery_viewers_are_progressive_not_browsers() {
        for reader in ["loupe", "eog", "gwenview", "shotwell"] {
            assert!(is_progressive_reader(reader), "{reader}");
            assert!(!is_browsing_reader(reader), "{reader}");
        }
        assert!(!is_progressive_reader("libreoffice"));
        assert!(!is_progressive_reader("cp"));
    }

    #[test]
    fn recursive_scanners_and_shells_are_background_readers() {
        for reader in [
            "find", "fd", "rg", "grep", "du", "tree", "bash", "zsh", "fish",
        ] {
            assert!(is_background_reader(reader).is_some(), "{reader}");
        }
        // An explicit directory listing remains a foreground action.
        assert!(is_background_reader("ls").is_none());
    }
}
