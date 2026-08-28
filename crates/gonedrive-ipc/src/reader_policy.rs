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
    "localsearch",   // GNOME's file indexer (formerly tracker-miner-fs)
    "tracker-miner",
    "tracker-extract",
    "tracker-store",
    "baloo_file",    // KDE's indexer (covers baloo_file_extractor by prefix)
    "thumbnailer",   // gnome-desktop / tumbler thumbnail generation
    "tumblerd",
    "gsd-housekeep", // GNOME's disk-usage sweep
    "updatedb",      // locate(1) database builders
    "plocate",
    "mlocate",
    "clamd",         // on-access antivirus
    "freshclam",
];

/// Tools that copy whole files. Being sparse-aware, they would otherwise SEEK
/// over a placeholder's holes and copy zeros, so they must hydrate fully on
/// open. Matched exactly, never as a prefix.
pub const BULK_COPIERS: &[&str] = &[
    "cp", "mv", "install", "rsync", "tar", "dd", "cpio", "rclone", "7z", "7za",
    "zip", "gzip", "bsdtar", "pax", "cksum",
];

/// The file manager, its preview pane, and every thumbnailer — anything that
/// reads a file to *describe* it. A thumbnail of an online-only file simply
/// does not render until the file is opened for real, exactly as on Windows; it
/// must never pull the whole image down. Matched as a prefix of `comm`.
pub const BROWSING_READERS: &[&str] = &[
    "nautilus",
    "sushi",          // GNOME's preview (spacebar)
    "glycin",         // GNOME's image loader/thumbnailer sandbox
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
    BROWSING_READERS.iter().any(|prefix| comm.starts_with(prefix))
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
        assert!(!is_bulk_copier("cpuinfo"), "a prefix hit would download for nothing");
        assert!(!is_bulk_copier("nautilus"));
    }

    #[test]
    fn browsers_and_thumbnailers_match_by_prefix() {
        assert!(is_browsing_reader("nautilus"));
        assert!(is_browsing_reader("gdk-pixbuf-thum")); // truncated to 15
        assert!(is_browsing_reader("gst-video-thumb"));
        assert!(!is_browsing_reader("cp"));
        assert!(!is_browsing_reader("libreoffice"));
    }
}
