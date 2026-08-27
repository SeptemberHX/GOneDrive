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

    /// Ceiling the poll interval backs off to while idle.
    pub max_poll_interval_secs: u64,

    /// Quiet period a local file must have before it is queued for upload.
    pub local_debounce_ms: u64,

    /// Files at or above this size use a resumable upload session.
    /// Graph caps the simple `PUT /content` path at 4 MiB.
    pub upload_session_threshold: u64,

    /// Chunk size for resumable uploads. Must be a multiple of 320 KiB.
    pub upload_chunk_size: u64,

    /// Concurrent transfers.
    pub max_concurrent_transfers: usize,

    /// Refuse to start a download that would leave less than this much free
    /// space. A safety net independent of what the user pinned: running the
    /// root filesystem to zero takes the whole desktop session down with it.
    pub min_free_bytes: u64,

    /// Files-on-demand. Wired up in M2; parsed now so config files stay stable.
    pub on_demand: bool,

    /// What happens to file content the user has not explicitly asked for.
    ///
    /// Defaults to [`DownloadPolicy::Never`]. A drive can easily be larger than
    /// the disk it is being synced to, so "download everything you find" is not
    /// a safe default — it fills the disk and fails, having spent hours and a
    /// lot of someone's bandwidth to get there. Enumerate first, let the user
    /// choose, then transfer.
    pub download_new: DownloadPolicy,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: BUILTIN_CLIENT_ID.unwrap_or_default().to_string(),
            sync_root: default_sync_root(),
            poll_interval_secs: 30,
            max_poll_interval_secs: 300,
            local_debounce_ms: 500,
            upload_session_threshold: 4 * 1024 * 1024,
            upload_chunk_size: 10 * 1024 * 1024, // 32 x 320 KiB
            max_concurrent_transfers: 4,
            min_free_bytes: 2 * 1024 * 1024 * 1024,
            on_demand: false,
            download_new: DownloadPolicy::Never,
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
    // Avoiding a libc dependency in this crate for one number.
    std::fs::read_to_string("/proc/self/loginuid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&u| u != u32::MAX)
        .unwrap_or(1000)
}

/// Create every directory the daemon writes to, with private permissions.
pub fn ensure_dirs() -> Result<()> {
    for dir in [config_dir(), data_dir(), cache_dir(), runtime_dir()] {
        std::fs::create_dir_all(&dir)?;
        restrict(&dir)?;
    }
    Ok(())
}

fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
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
}
