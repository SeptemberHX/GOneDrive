//! Exact desktop open intents for gallery applications that preload siblings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use tokio::sync::{broadcast, Mutex, Notify};
use tokio::time::Instant;
use zbus::zvariant::OwnedValue;

const INTENT_TTL: Duration = Duration::from_secs(10);
const INTENT_WAIT: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_INTENTS: usize = 128;

const DESKTOP_OPEN_RULES: &[&str] = &[
    "type='method_call',destination='org.gnome.Loupe',interface='org.freedesktop.Application',member='Open'",
    "type='method_call',destination='org.gnome.Loupe',interface='org.gtk.Application',member='Open'",
    "type='method_call',destination='org.gnome.Nautilus',interface='org.freedesktop.Application',member='Open'",
    "type='method_call',destination='org.gnome.Nautilus',interface='org.gtk.Application',member='Open'",
    "type='method_call',destination='org.freedesktop.FileManager1',interface='org.freedesktop.FileManager1',member='ShowFolders'",
    "type='signal',sender='org.freedesktop.FileManager1',path='/org/freedesktop/FileManager1',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenDecision {
    /// This exact path was named by the desktop's Open request.
    Explicit,
    /// The monitor is healthy, but the viewer is speculatively reading a path
    /// that the user did not ask the desktop to open.
    Prefetch,
    /// Monitoring is unavailable; retain the range-based compatibility policy.
    Unavailable,
}

pub(crate) struct OpenIntents {
    root: PathBuf,
    entries: Mutex<HashMap<PathBuf, Instant>>,
    active: AtomicBool,
    changed: Notify,
    explicit: broadcast::Sender<PathBuf>,
}

impl OpenIntents {
    pub(crate) fn new(root: PathBuf) -> Arc<Self> {
        let (explicit, _) = broadcast::channel(MAX_INTENTS);
        Arc::new(Self {
            root,
            entries: Mutex::new(HashMap::new()),
            active: AtomicBool::new(false),
            changed: Notify::new(),
            explicit,
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<PathBuf> {
        self.explicit.subscribe()
    }

    pub(crate) async fn decide(&self, path: &Path) -> OpenDecision {
        if !self.active.load(Ordering::Acquire) {
            return OpenDecision::Unavailable;
        }

        let deadline = Instant::now() + INTENT_WAIT;
        loop {
            // Arm before checking the map so a record arriving between the
            // check and await cannot be missed.
            let changed = self.changed.notified();
            if self.claim(path).await {
                tracing::debug!(path = %path.display(), "claimed explicit desktop open intent");
                return OpenDecision::Explicit;
            }
            if !self.active.load(Ordering::Acquire) {
                return OpenDecision::Unavailable;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                tracing::debug!(path = %path.display(), "desktop open intent expired; treating read as prefetch");
                return OpenDecision::Prefetch;
            }
        }
    }

    async fn claim(&self, path: &Path) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, expires| *expires > now);
        entries.remove(path).is_some()
    }

    async fn record_uri(&self, uri: &str) {
        let Some(path) = confined_file_uri(&self.root, uri) else {
            return;
        };
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, expires| *expires > now);
        if entries.len() >= MAX_INTENTS && !entries.contains_key(&path) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(path, _)| path.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(path.clone(), now + INTENT_TTL);
        drop(entries);
        tracing::debug!(path = %path.display(), "recorded desktop open intent");
        let _ = self.explicit.send(path);
        self.changed.notify_waiters();
    }

    pub(crate) async fn supervise(self: Arc<Self>) {
        let mut reported = false;
        loop {
            match self.monitor_once().await {
                Ok(()) => {
                    self.set_active(false).await;
                    if !reported {
                        tracing::warn!("desktop open-intent monitor stopped; retrying");
                        reported = true;
                    }
                }
                Err(error) => {
                    self.set_active(false).await;
                    if !reported {
                        tracing::warn!(%error, "desktop open-intent monitor unavailable; gallery reads use range fallback");
                        reported = true;
                    }
                }
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }

    async fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
        if !active {
            self.entries.lock().await.clear();
        }
        self.changed.notify_waiters();
    }

    async fn monitor_once(&self) -> Result<()> {
        let connection = zbus::Connection::session()
            .await
            .context("could not connect the open-intent monitor to the session bus")?;
        connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Monitoring"),
                "BecomeMonitor",
                &(DESKTOP_OPEN_RULES, 0u32),
            )
            .await
            .context("the session bus refused BecomeMonitor")?;

        self.set_active(true).await;
        tracing::info!("desktop open-intent monitor is active");
        let mut stream = zbus::MessageStream::from(connection);
        while let Some(message) = stream.try_next().await? {
            match open_uris(&message) {
                Ok(uris) => {
                    for uri in uris {
                        self.record_uri(&uri).await;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "ignored malformed desktop Open request");
                }
            }
        }
        Ok(())
    }
}

fn open_uris(message: &zbus::Message) -> zbus::Result<Vec<String>> {
    type Options = HashMap<String, OwnedValue>;
    if let Ok((interface, mut changed, _invalidated)) = message
        .body()
        .deserialize::<(String, Options, Vec<String>)>()
    {
        if interface != "org.freedesktop.FileManager1" {
            return Ok(Vec::new());
        }
        return match changed.remove("OpenLocations") {
            Some(value) => Vec::<String>::try_from(value).map_err(zbus::Error::Variant),
            None => Ok(Vec::new()),
        };
    }
    if let Ok((uris, _options)) = message.body().deserialize::<(Vec<String>, Options)>() {
        return Ok(uris);
    }
    if let Ok((uris, _startup_id)) = message.body().deserialize::<(Vec<String>, String)>() {
        return Ok(uris);
    }
    let (uris, _hint, _options) = message
        .body()
        .deserialize::<(Vec<String>, String, Options)>()?;
    Ok(uris)
}

fn confined_file_uri(root: &Path, uri: &str) -> Option<PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    if path == root || !path.starts_with(root) {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_file_uris_beneath_the_sync_root_become_intents() {
        let root = Path::new("/home/u/OneDrive");
        assert_eq!(
            confined_file_uri(root, "file:///home/u/OneDrive/A%20B/photo.jpg"),
            Some(PathBuf::from("/home/u/OneDrive/A B/photo.jpg"))
        );
        assert_eq!(confined_file_uri(root, "file:///home/u/outside.jpg"), None);
        assert_eq!(
            confined_file_uri(root, "https://example.com/photo.jpg"),
            None
        );
        assert_eq!(confined_file_uri(root, "file:///home/u/OneDrive"), None);
    }

    #[test]
    fn standard_and_gtk_open_messages_expose_the_exact_uri_list() {
        let uris = vec!["file:///home/u/OneDrive/target.jpg".to_string()];
        let options = HashMap::<String, OwnedValue>::new();
        let standard = zbus::Message::method_call("/org/gnome/Loupe", "Open")
            .unwrap()
            .interface("org.freedesktop.Application")
            .unwrap()
            .build(&(uris.clone(), options))
            .unwrap();
        assert_eq!(open_uris(&standard).unwrap(), uris);

        let options = HashMap::<String, OwnedValue>::new();
        let gtk = zbus::Message::method_call("/org/gnome/Loupe", "Open")
            .unwrap()
            .interface("org.gtk.Application")
            .unwrap()
            .build(&(uris.clone(), String::new(), options))
            .unwrap();
        assert_eq!(open_uris(&gtk).unwrap(), uris);
    }

    #[tokio::test]
    async fn an_explicit_intent_is_exact_and_consumed_once() {
        let intents = OpenIntents::new(PathBuf::from("/home/u/OneDrive"));
        let mut explicit = intents.subscribe();
        intents.set_active(true).await;
        intents
            .record_uri("file:///home/u/OneDrive/photos/target.jpg")
            .await;

        assert_eq!(
            explicit.recv().await.unwrap(),
            PathBuf::from("/home/u/OneDrive/photos/target.jpg")
        );

        assert_eq!(
            intents
                .decide(Path::new("/home/u/OneDrive/photos/target.jpg"))
                .await,
            OpenDecision::Explicit
        );
        assert!(
            !intents
                .claim(Path::new("/home/u/OneDrive/photos/sibling.jpg"))
                .await
        );
        assert!(
            !intents
                .claim(Path::new("/home/u/OneDrive/photos/target.jpg"))
                .await
        );
    }

    #[tokio::test]
    async fn an_inactive_monitor_uses_the_compatibility_fallback_immediately() {
        let intents = OpenIntents::new(PathBuf::from("/home/u/OneDrive"));
        assert_eq!(
            intents
                .decide(Path::new("/home/u/OneDrive/photos/target.jpg"))
                .await,
            OpenDecision::Unavailable
        );
    }

    #[test]
    fn file_manager_location_changes_expose_only_open_locations() {
        use zbus::zvariant::Value;

        let uris = vec!["file:///home/u/OneDrive/Folder".to_string()];
        let locations = OwnedValue::try_from(Value::from(uris.clone())).unwrap();
        let changed = HashMap::from([
            ("OpenLocations".to_string(), locations),
            ("Unrelated".to_string(), OwnedValue::from(true)),
        ]);
        let message = zbus::Message::signal(
            "/org/freedesktop/FileManager1",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap()
        .build(&(
            "org.freedesktop.FileManager1".to_string(),
            changed,
            Vec::<String>::new(),
        ))
        .unwrap();

        assert_eq!(open_uris(&message).unwrap(), uris);
    }
}
