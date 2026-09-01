//! On-disk configuration and the XDG paths the daemon uses.
//!
//! # The client id
//!
//! GOnedrive is a public OAuth client, so its `client_id` is not a secret — it
//! only says *which application* is asking, never *who* is signing in. Every
//! established Linux OneDrive client (abraunegg's, rclone, onedriver) therefore
//! ships one baked in and lets users override it, and so does this one:
//!
//! 1. `GONEDRIVE_CLIENT_ID` in the environment — overrides everything.
//! 2. `client_id` in the config file.
//! 3. A value baked in at build time with
//!    `GONEDRIVE_CLIENT_ID=… cargo build --release`.
//!
//! Registering the app is therefore a **one-time step for whoever builds
//! GOnedrive**, not something each user has to repeat. The override exists for
//! the reason rclone documents: a shared registration means a shared API quota,
//! so anyone hitting throttling can drop in their own id without rebuilding.
//!
//! When none of the three is set the daemon refuses to start with instructions
//! rather than failing later against Microsoft with `AADSTS700016`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Runtime override, and the name used for the build-time default too.
pub const CLIENT_ID_ENV: &str = "GONEDRIVE_CLIENT_ID";

/// The default baked in at build time with `GONEDRIVE_CLIENT_ID=… cargo build`.
const BUILTIN_CLIENT_ID: Option<&str> = option_env!("GONEDRIVE_CLIENT_ID");

/// The placeholder in `config.example.toml`.
///
/// Caught by name because copying the example verbatim is the single most
/// likely way to get here, and leaving it in produces `AADSTS700016:
/// Application not found` from Microsoft — an error that says nothing about
/// the actual mistake.
const PLACEHOLDER_CLIENT_ID: &str = "00000000-0000-0000-0000-000000000000";

/// `common` covers both personal Microsoft accounts and work/school accounts.
pub const AUTHORITY: &str = "https://login.microsoftonline.com/common";
pub const GRAPH_ENDPOINT: &str = "https://graph.microsoft.com/v1.0";

/// Delegated permissions requested at login.
pub const SCOPES: &[&str] = &["offline_access", "User.Read", "Files.ReadWrite.All"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Entra ID application (client) id. See the module docs.
    pub client_id: String,

    /// Local directory that mirrors the drive root.
    pub sync_root: PathBuf,

    /// How often to poll `/delta` while the user is active.
    pub poll_interval_secs: u64,

    /// Ceiling for retry backoff after remote polling errors. Successful idle
    /// polls stay on `poll_interval_secs` so cloud changes have bounded delay.
    pub max_poll_interval_secs: u64,

    /// Low-frequency whole-tree verification for events missed while stopped
    /// or after an inotify overflow.
    pub full_scan_interval_secs: u64,

    /// Quiet period a local file must have before it is queued for upload.
    pub local_debounce_ms: u64,

    /// Files at or above this size use a resumable upload session.
    /// Graph caps the simple `PUT /content` path at 4 MiB.
    pub upload_session_threshold: u64,

    /// Chunk size for resumable uploads. Must be a multiple of 320 KiB.
    pub upload_chunk_size: u64,

    /// Accepted only so configurations written by older releases keep
    /// loading. Transfers are ordered for dependency and conflict safety, so
    /// the old setting never controlled real concurrency and is not emitted.
    #[doc(hidden)]
    #[serde(default, skip_serializing)]
    pub max_concurrent_transfers: Option<usize>,

    /// Refuse to start a download that would leave less than this much free
    /// space. A safety net independent of what the user pinned: running the
    /// root filesystem to zero takes the whole desktop session down with it.
    pub min_free_bytes: u64,

    /// Present remote files as sparse placeholders and hydrate them on access.
    pub on_demand: bool,

    /// Keep desktop indexers out of the sync root. Linux indexers do not
    /// understand cloud placeholders and recursively opening partial folders
    /// would both waste power and accidentally populate the whole namespace.
    /// The daemon implements this with the standard `.trackerignore` marker.
    pub exclude_desktop_indexer: bool,

    /// Storage-sense style automatic space recovery: when free space drops
    /// below this, downloaded-but-unpinned files are turned back into
    /// placeholders, least recently used first, until it is above it again.
    /// Zero disables it, which is the default — giving space back is a policy
    /// the user opts into, not something a sync client does on its own.
    pub auto_free_below_bytes: u64,

    /// Transfer rate ceilings, bytes per second. Zero means unlimited.
    pub download_limit_bytes_per_sec: u64,
    pub upload_limit_bytes_per_sec: u64,

    /// What happens to file content the user has not explicitly asked for.
    ///
    /// Defaults to [`DownloadPolicy::Never`]. A drive can easily be larger than
    /// the disk it is being synced to, so "download everything you find" is not
    /// a safe default — it fills the disk and fails, having spent hours and a
    /// lot of someone's bandwidth to get there. Enumerate first, let the user
    /// choose, then transfer.
    pub download_new: DownloadPolicy,

    /// Mass-deletion safety valve. Pending deletion batches that together
    /// would delete more than `mass_delete_min` items *and* more than this
    /// fraction of everything on record are treated as accidental data loss
    /// (a vanished mount, a bad bind, an `rm -rf` the user did not mean to
    /// propagate): the deletions are held and the user is asked to confirm
    /// rather than executed. `1.0` disables the fraction test.
    pub mass_delete_fraction: f64,
    /// The floor below which the fraction test never trips, so deleting a few
    /// files out of a small drive is never second-guessed.
    pub mass_delete_min: u64,
}

/// Whether newly discovered files are downloaded without being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DownloadPolicy {
    /// Enumerate only. Content arrives when a folder or file is pinned.
    #[default]
    Never,
    /// Classic full sync: mirror the entire drive locally.
    Always,
}

impl DownloadPolicy {
    /// The pin state an item inherits when nothing above it says otherwise.
    pub fn default_pin(self) -> crate::db::PinState {
        match self {
            DownloadPolicy::Never => crate::db::PinState::Unpinned,
            DownloadPolicy::Always => crate::db::PinState::Pinned,
        }
    }
}

/// Graph requires resumable upload chunks to be a multiple of this.
pub const UPLOAD_CHUNK_GRANULARITY: u64 = 320 * 1024;
const MAX_UPLOAD_CHUNK_SIZE: u64 = 60 * 1024 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: BUILTIN_CLIENT_ID.unwrap_or_default().to_string(),
            sync_root: default_sync_root(),
            poll_interval_secs: 30,
            max_poll_interval_secs: 300,
            full_scan_interval_secs: 30 * 60,
            local_debounce_ms: 500,
            upload_session_threshold: 4 * 1024 * 1024,
            upload_chunk_size: 10 * 1024 * 1024, // 32 x 320 KiB
            max_concurrent_transfers: None,
            min_free_bytes: 2 * 1024 * 1024 * 1024,
            auto_free_below_bytes: 0,
            download_limit_bytes_per_sec: 0,
            upload_limit_bytes_per_sec: 0,
            on_demand: false,
            exclude_desktop_indexer: true,
            download_new: DownloadPolicy::Never,
            mass_delete_fraction: 0.5,
            mass_delete_min: 10,
        }
    }
}

impl Config {
    /// Load from `$XDG_CONFIG_HOME/gonedrive/config.toml`, falling back to
    /// defaults when the file does not exist yet.
    pub fn load() -> Result<Self> {
        let path = config_file();
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            toml::from_str::<Config>(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
        } else {
            Config::default()
        };

        // A config file that omits `client_id`, or leaves it blank, should fall
        // back to the baked-in default rather than defeating it.
        if cfg.client_id.trim().is_empty() {
            cfg.client_id = BUILTIN_CLIENT_ID.unwrap_or_default().to_string();
        }
        if let Ok(id) = std::env::var(CLIENT_ID_ENV) {
            if !id.trim().is_empty() {
                cfg.client_id = id.trim().to_string();
            }
        }

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialising config: {e}")))?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let client_id = self.client_id.trim();
        if client_id.is_empty() || client_id == PLACEHOLDER_CLIENT_ID {
            let what = if client_id.is_empty() {
                "No OAuth client id is configured."
            } else {
                "The client id is still the placeholder from config.example.toml."
            };
            return Err(Error::Config(format!(
                "{what}\n\n\
                 GOnedrive needs an Entra ID application registration. It identifies \
                 the *application*, not you — you still sign in with your own Microsoft \
                 account. It is not a secret, and registering one is a one-time step.\n\n\
                 Register at https://entra.microsoft.com under App registrations:\n  \
                 - Supported account types: \"Accounts in any organizational directory \
                 and personal Microsoft accounts\"\n  \
                 - Platform: \"Mobile and desktop applications\", redirect URI exactly \
                 http://localhost (no path, no port)\n\n\
                 Then use it in any one of these ways:\n  \
                 - export {CLIENT_ID_ENV}=<id>            (quickest)\n  \
                 - set client_id in {}\n  \
                 - bake it into the binary so no user ever needs to repeat this:\n    \
                 {CLIENT_ID_ENV}=<id> cargo build --release",
                config_file().display()
            )));
        }
        if self.upload_chunk_size % UPLOAD_CHUNK_GRANULARITY != 0 || self.upload_chunk_size == 0 {
            return Err(Error::Config(format!(
                "upload_chunk_size ({}) must be a non-zero multiple of {UPLOAD_CHUNK_GRANULARITY}",
                self.upload_chunk_size
            )));
        }
        if self.upload_chunk_size >= MAX_UPLOAD_CHUNK_SIZE {
            return Err(Error::Config(format!(
                "upload_chunk_size must be less than {} MiB",
                MAX_UPLOAD_CHUNK_SIZE / 1024 / 1024
            )));
        }
        if self.upload_session_threshold > 4 * 1024 * 1024 {
            return Err(Error::Config(
                "upload_session_threshold cannot exceed 4 MiB; Graph rejects larger simple uploads"
                    .into(),
            ));
        }
        if self.poll_interval_secs == 0 || self.max_poll_interval_secs < self.poll_interval_secs {
            return Err(Error::Config(
                "poll_interval_secs must be non-zero and no greater than max_poll_interval_secs"
                    .into(),
            ));
        }
        if self.full_scan_interval_secs == 0 {
            return Err(Error::Config(
                "full_scan_interval_secs must be non-zero".into(),
            ));
        }
        if !self.mass_delete_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.mass_delete_fraction)
        {
            return Err(Error::Config(
                "mass_delete_fraction must be between 0.0 and 1.0".into(),
            ));
        }
        Ok(())
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn default_sync_root() -> PathBuf {
    home().join("OneDrive")
}

/// `$XDG_CONFIG_HOME/gonedrive`
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home().join(".config"))
        .join("gonedrive")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_DATA_HOME/gonedrive` — the state database lives here.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| home().join(".local/share"))
        .join("gonedrive")
}

pub fn state_db_path() -> PathBuf {
    data_dir().join("state.db")
}

/// `$XDG_CACHE_HOME/gonedrive` — partial downloads and the FUSE content cache.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| home().join(".cache"))
        .join("gonedrive")
}

/// `$XDG_RUNTIME_DIR/gonedrive` — sockets. Falls back to a per-uid /tmp path.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/gonedrive-{}", uid())))
        .join("gonedrive")
}

fn uid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail. `loginuid` is an audit
    // identity and is commonly unset in containers; using it here once made
    // unrelated users fall back to the same predictable /tmp directory.
    unsafe { libc::geteuid() }
}

/// Create every directory the daemon writes to, with private permissions.
pub fn ensure_dirs() -> Result<()> {
    // XDG_RUNTIME_DIR is normally created securely by the login manager. Its
    // /tmp fallback needs an owned, private parent too; otherwise another uid
    // could pre-create the predictable directory and replace children in it.
    if dirs::runtime_dir().is_none() {
        let fallback = PathBuf::from(format!("/tmp/gonedrive-{}", uid()));
        ensure_private_dir(&fallback)?;
    }
    for dir in [config_dir(), data_dir(), cache_dir(), runtime_dir()] {
        ensure_private_dir(&dir)?;
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Config(format!(
            "{} must be a real directory, not a symlink or special file",
            path.display()
        )));
    }
    if metadata.uid() != uid() {
        return Err(Error::Config(format!(
            "{} belongs to uid {}, expected {}",
            path.display(),
            metadata.uid(),
            uid()
        )));
    }
    let mut perms = metadata.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chunk_size_satisfies_graph_granularity() {
        let cfg = Config {
            client_id: "test".into(),
            ..Config::default()
        };
        assert_eq!(cfg.upload_chunk_size % UPLOAD_CHUNK_GRANULARITY, 0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn private_directories_never_follow_a_final_symlink() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("link");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(ensure_private_dir(&link).is_err());
    }

    #[test]
    fn rejects_misaligned_chunk_size() {
        let cfg = Config {
            client_id: "test".into(),
            // 8 MiB is 25.6 x 320 KiB. Note 5 and 10 MiB *are* exact
            // multiples, which is why the default is 10 MiB.
            upload_chunk_size: 8 * 1024 * 1024,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_graphs_maximum_chunk_size() {
        let cfg = Config {
            client_id: "test".into(),
            upload_chunk_size: MAX_UPLOAD_CHUNK_SIZE,
            ..Config::default()
        };
        let error = cfg.validate().unwrap_err().to_string();
        assert!(error.contains("less than 60 MiB"), "{error}");
    }

    #[test]
    fn rejects_missing_client_id() {
        let cfg = Config {
            client_id: String::new(),
            ..Config::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("No OAuth client id"), "{err}");
        // The message has to be actionable: it is the first thing a new user
        // ever sees from this program.
        assert!(err.contains("entra.microsoft.com"), "{err}");
        assert!(err.contains("http://localhost"), "{err}");
        assert!(err.contains("cargo build --release"), "{err}");
    }

    #[test]
    fn rejects_the_example_placeholder_by_name() {
        // Copying config.example.toml verbatim used to sail past validation and
        // fail much later as AADSTS700016, which names nothing useful.
        let cfg = Config {
            client_id: PLACEHOLDER_CLIENT_ID.into(),
            ..Config::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("placeholder"), "{err}");
    }

    #[test]
    fn whitespace_only_client_id_is_treated_as_missing() {
        let cfg = Config {
            client_id: "   ".into(),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_oversized_simple_upload_threshold() {
        let cfg = Config {
            client_id: "test".into(),
            upload_session_threshold: 8 * 1024 * 1024,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_a_disabled_full_scan_backstop() {
        let cfg = Config {
            client_id: "test".into(),
            full_scan_interval_secs: 0,
            ..Config::default()
        };
        let error = cfg.validate().unwrap_err().to_string();
        assert!(error.contains("full_scan_interval_secs"), "{error}");
    }

    #[test]
    fn rejects_a_mass_delete_fraction_that_cannot_guard() {
        for fraction in [-0.1, 1.1, f64::NAN] {
            let cfg = Config {
                client_id: "test".into(),
                mass_delete_fraction: fraction,
                ..Config::default()
            };
            let error = cfg.validate().unwrap_err().to_string();
            assert!(error.contains("mass_delete_fraction"), "{error}");
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config {
            client_id: "abc".into(),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.client_id, "abc");
        assert_eq!(back.upload_chunk_size, cfg.upload_chunk_size);
    }

    #[test]
    fn legacy_concurrency_setting_is_accepted_but_not_rewritten() {
        let text = "client_id = 'abc'\nmax_concurrent_transfers = 4\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.max_concurrent_transfers, Some(4));

        let rewritten = toml::to_string_pretty(&config).unwrap();
        assert!(!rewritten.contains("max_concurrent_transfers"));
    }
}
