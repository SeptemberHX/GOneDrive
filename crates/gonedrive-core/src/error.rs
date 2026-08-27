//! Crate-wide error type.
//!
//! Graph failures are modelled explicitly rather than collapsed into a string,
//! because the sync engine has to branch on them: `ResyncRequired` restarts the
//! delta enumeration, `Throttled` feeds the rate limiter, and `AuthRequired`
//! surfaces to the UI as a re-login prompt.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The refresh token is gone, revoked, or rejected. The user must log in again.
    #[error("authentication required: {0}")]
    AuthRequired(String),

    /// A structured error returned by Microsoft Graph.
    #[error("graph error {status} ({code}): {message}")]
    Graph {
        status: u16,
        code: String,
        message: String,
    },

    /// HTTP 410 `resyncRequired` — the stored deltaLink is no longer usable.
    #[error("delta token expired; a full resync is required")]
    ResyncRequired,

    /// HTTP 429/503 with a `Retry-After` we should honour.
    #[error("throttled by the service; retry after {0:?}")]
    Throttled(Duration),

    /// A name that OneDrive will not accept. Reported to the user, never auto-renamed.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName { name: String, reason: NameError },

    /// An upload session was rejected or expired mid-transfer.
    #[error("upload session for {item} is no longer valid: {reason}")]
    UploadSessionExpired { item: String, reason: String },

    /// The user asked for this transfer to stop.
    #[error("cancelled")]
    Cancelled,

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Db(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("secret service: {0}")]
    Keyring(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// Whether retrying the same operation later could plausibly succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Throttled(_) => true,
            Error::Http(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            ),
            Error::Graph { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// Why a file or folder name is unusable on OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    IllegalCharacter,
    TrailingSpaceOrDot,
    LeadingSpace,
    ReservedName,
    ReservedPrefix,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NameError::Empty => "the name is empty",
            NameError::TooLong => "the name exceeds 255 characters",
            NameError::IllegalCharacter => "contains one of \" * : < > ? / \\ | or a control character",
            NameError::TrailingSpaceOrDot => "ends with a space or a dot",
            NameError::LeadingSpace => "starts with a space",
            NameError::ReservedName => "is a name OneDrive reserves",
            NameError::ReservedPrefix => "starts with a prefix OneDrive reserves (~$ or .~)",
        };
        f.write_str(s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
