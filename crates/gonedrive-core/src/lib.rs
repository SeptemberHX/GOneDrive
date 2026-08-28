//! Core of GOnedrive: Microsoft Graph client, sync engine and state store.
//!
//! Everything here is UI-agnostic and privilege-free. The GTK front end talks
//! to `gonedrived` over D-Bus and `gonedrived` links this crate directly; the
//! privileged fanotify helper does *not* — it never touches the network or the
//! user's tokens.

pub mod auth;
pub mod config;
pub mod credentials;
pub mod db;
pub mod error;
pub mod graph;
pub mod hash;
pub mod hydrate;
pub mod models;
pub mod names;
pub mod sync;

pub use config::{Config, DownloadPolicy};
pub use credentials::{Authenticator, TokenSource, TokenStore};
pub use db::Store;
pub use graph::GraphClient;
pub use error::{Error, NameError, Result};
