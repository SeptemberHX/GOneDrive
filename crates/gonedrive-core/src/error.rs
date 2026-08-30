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

    /// Graph acknowledged the final fragment, but the returned driveItem could
    /// not be decoded and has not yet been independently verified. Retrying
    /// must reconcile the cloud item; it must not create another same-name file.
    #[error("upload completion for {item} is awaiting verification: {reason}")]
    UploadCommitUncertain { item: String, reason: String },

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

    /// The credentials still exist, but Secret Service requires the user to
    /// unlock the collection before they can be read or updated.
    #[error("credential keyring is locked: {0}")]
    KeyringLocked(String),

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
            // Nothing is wrong with the saved credential. Once the user
            // unlocks the login keyring, the exact same operation can run.
            Error::KeyringLocked(_) => true,
            Error::UploadCommitUncertain { .. } => true,
            // `is_body` is the one that matters on a flaky link: a connection
            // dropped or reset *while streaming a response body* (a train
            // entering a tunnel mid-download) surfaces here, not as a connect
            // or request error. Without it such an interruption counts as
            // permanent and the transfer is abandoned on the first failure.
            Error::Http(e) => {
                e.is_timeout()
                    || e.is_connect()
                    || e.is_request()
                    || e.is_body()
                    // A successful status with a truncated or temporarily
                    // malformed body is recoverable by querying the resource
                    // again. Treating it as permanent used to discard uploads.
                    || e.is_decode()
            }
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    // A body that ended early, or a connection torn down under
                    // an in-flight transfer — all retryable, none a reason to
                    // give up on the file.
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
            Error::Graph { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use std::io;

    #[test]
    fn a_dropped_connection_mid_transfer_is_retryable() {
        // The failure this guards: a signal drop mid-download must be treated
        // as transient (retry later), never as a permanent error that abandons
        // the transfer on its first attempt.
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
            // Used when a local inode changes during a transfer. Retrying is
            // required so the queue does not abandon the user's newer edit.
            io::ErrorKind::WouldBlock,
        ] {
            assert!(
                Error::Io(io::Error::from(kind)).is_transient(),
                "{kind:?} should be retryable"
            );
        }
        // A genuine local problem is not a network hiccup: do not retry forever.
        assert!(!Error::Io(io::Error::from(io::ErrorKind::PermissionDenied)).is_transient());
        assert!(!Error::other("bad name").is_transient());
        assert!(Error::KeyringLocked("login collection".into()).is_transient());
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
            NameError::IllegalCharacter => {
                "contains one of \" * : < > ? / \\ | or a control character"
            }
            NameError::TrailingSpaceOrDot => "ends with a space or a dot",
            NameError::LeadingSpace => "starts with a space",
            NameError::ReservedName => "is a name OneDrive reserves",
            NameError::ReservedPrefix => "starts with a prefix OneDrive reserves (~$ or .~)",
        };
        f.write_str(s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
