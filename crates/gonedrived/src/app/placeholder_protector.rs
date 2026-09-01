//! Delta-page bridge from sparse placeholder creation to HSM registration.

use std::sync::{Arc, Weak};
use std::time::Duration;

use gonedrive_core::sync::engine::{Engine, PlaceholderProtector};
use gonedrive_core::Error;

use super::App;

const DELTA_HSM_WAIT: Duration = Duration::from_secs(5);

struct DaemonPlaceholderProtector {
    app: Weak<App>,
}

impl PlaceholderProtector for DaemonPlaceholderProtector {
    fn protect<'a>(
        &'a self,
        paths: &'a [std::path::PathBuf],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = gonedrive_core::Result<()>> + Send + 'a>>
    {
        Box::pin(async move {
            let app = self.app.upgrade().ok_or_else(|| {
                Error::other("the daemon stopped during placeholder registration")
            })?;
            let deadline = tokio::time::Instant::now() + DELTA_HSM_WAIT;
            loop {
                if let Some(hsm) = app.hsm.read().await.clone() {
                    return app
                        .protect_placeholders(&hsm, paths)
                        .await
                        .map_err(|error| Error::other(error.to_string()));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::other(format!(
                        "files-on-demand helper is unavailable; {} new placeholder(s) remain safety-locked",
                        paths.len()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    }
}

impl App {
    pub(super) fn install_placeholder_protector(self: &Arc<Self>, engine: &Arc<Engine>) {
        engine.set_placeholder_protector(Arc::new(DaemonPlaceholderProtector {
            app: Arc::downgrade(self),
        }));
    }
}
