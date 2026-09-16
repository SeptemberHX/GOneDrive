//! Switching between files-on-demand and classic full sync.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::config::{Config, DownloadPolicy};
use gonedrive_core::sync::LazyConversion;

use super::App;

/// How the drive is kept on this device, as the user picks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Placeholders that fetch themselves when opened; only browsed folders
    /// are indexed. The product default.
    OnDemand,
    /// Classic full sync: every item indexed, every file downloaded.
    Full,
    /// Every item indexed, nothing downloaded unless pinned. The legacy
    /// configuration before files-on-demand existed; offered only for
    /// reporting, never chosen from the UI.
    Metadata,
}

impl SyncMode {
    pub fn parse(mode: &str) -> Option<Self> {
        match mode.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "on-demand" | "ondemand" | "lazy" => Some(Self::OnDemand),
            "full" => Some(Self::Full),
            "metadata" | "metadata-only" => Some(Self::Metadata),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on-demand",
            Self::Full => "full",
            Self::Metadata => "metadata",
        }
    }

    pub fn of(config: &Config) -> Self {
        match (config.on_demand, config.download_new) {
            (_, DownloadPolicy::Always) => Self::Full,
            (true, DownloadPolicy::Never) => Self::OnDemand,
            (false, DownloadPolicy::Never) => Self::Metadata,
        }
    }
}

/// What a switch did, for the CLI and the settings dialog to report.
#[derive(Debug, Clone, Default)]
pub struct ModeSwitch {
    pub mode: &'static str,
    pub conversion: Option<LazyConversion>,
    /// The download policy changed, which only a restarted engine applies.
    pub policy_changed: bool,
    /// The daemon is restarting itself to apply that policy.
    pub restarting: bool,
}

impl App {
    /// The mode the daemon was started with. A pending switch that needs a
    /// restart is not reported until it has taken effect.
    pub fn sync_mode(&self) -> SyncMode {
        SyncMode::of(&self.config())
    }

    /// Switch modes, persisting the choice for every later start.
    ///
    /// To on-demand: prune the tree in place so downloaded files, pins and
    /// local edits stay and never-opened folders become lazy — no re-login,
    /// no re-enumeration. To full: reset the delta cursor so the next pass
    /// walks the whole drive under the download-everything policy. The
    /// download policy lives in the immutable engine configuration, so a
    /// change of policy ends with a daemon restart; the on-demand flag itself
    /// is applied live.
    pub async fn set_sync_mode(self: &Arc<Self>, mode: SyncMode) -> Result<ModeSwitch> {
        let _one_at_a_time = self.mode_switch.lock().await;
        let current = self.sync_mode();
        let previous = self.config();
        let mut config = (*previous).clone();
        let mut switch = ModeSwitch {
            mode: mode.as_str(),
            ..Default::default()
        };
        match mode {
            SyncMode::OnDemand => {
                // Publish first: pruning ends by registering the directory
                // marks that make a partial folder repopulate, and the
                // inventory behind that returns nothing while the live
                // configuration still says files-on-demand is off.
                config.on_demand = true;
                config.download_new = DownloadPolicy::Never;
                self.replace_config(config.clone());
                switch.conversion = Some(self.convert_to_lazy().await?);
            }
            SyncMode::Full => {
                config.on_demand = false;
                config.download_new = DownloadPolicy::Always;
            }
            SyncMode::Metadata => {
                config.on_demand = false;
                config.download_new = DownloadPolicy::Never;
            }
        }
        if config.on_demand != previous.on_demand || config.download_new != previous.download_new {
            config.save().context("could not persist the sync mode")?;
            // Publish the new mode to everything that reads the live
            // configuration — `sync_mode()`, the placeholder inventories, a
            // later login's persistence — so an in-place switch is neither
            // reported nor acted on as the mode the daemon started with.
            self.replace_config(config.clone());
            tracing::info!(
                from = current.as_str(),
                to = mode.as_str(),
                "sync mode changed"
            );
        }

        // Both directions, live. `on_demand` decides whether the engine turns
        // new items into placeholders, and the daemon's mark inventories are
        // keyed off the same configuration flag: if the two disagree, the
        // engine makes placeholders the helper is never told to watch.
        if config.on_demand != previous.on_demand {
            for engine in self.engines.read().await.values() {
                if config.on_demand {
                    engine.enable_on_demand();
                } else {
                    engine.disable_on_demand();
                }
            }
            // The marks that belonged to the old mode are now wrong in one
            // direction or the other; one registration pass settles it.
            if let Some(hsm) = self.hsm.read().await.clone() {
                if let Err(error) = hsm.watch_all(self).await {
                    tracing::warn!(%error, "could not reconcile placeholder marks after the switch");
                }
            }
        }

        // A policy change needs the engines rebuilt. Full mode also needs the
        // whole drive enumerated again: a lazy index knows only browsed
        // folders, and the download-everything policy has to see every file.
        if config.download_new != previous.download_new {
            switch.policy_changed = true;
            if mode == SyncMode::Full {
                for engine in self.engines.read().await.values() {
                    self.store.abort_delta_batch(engine.account_id())?;
                    self.store.set_delta_link(engine.account_id(), None)?;
                }
            }
            // Pause only once we know a restart is coming. Pausing first and
            // then discovering there is no service to restart left the daemon
            // stopped for good, with "paused" as the only explanation on
            // screen and no way back but a manual resume.
            switch.restarting = self.systemd_managed();
            if switch.restarting {
                self.set_paused(true).await;
                self.spawn_restart();
            } else {
                tracing::warn!("restart gonedrived to apply the new sync mode");
            }
        }
        Ok(switch)
    }

    /// Prune every signed-in account's tree to lazy population.
    async fn convert_to_lazy(&self) -> Result<LazyConversion> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut total = LazyConversion::default();
        for engine in engines {
            tracing::info!("converting the placeholder tree to lazy population");
            let worker = engine.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                worker.convert_to_lazy(|done, total| {
                    if done % 5000 == 0 || done == total {
                        tracing::info!(done, total, "pruning never-opened folders");
                    }
                    true
                })
            })
            .await??;
            tracing::info!(
                kept = outcome.folders_kept,
                pruned = outcome.folders_pruned,
                placeholders = outcome.placeholders_removed,
                rows = outcome.rows_removed,
                "placeholder tree converted"
            );
            total.folders_kept += outcome.folders_kept;
            total.folders_pruned += outcome.folders_pruned;
            total.placeholders_removed += outcome.placeholders_removed;
            total.rows_removed += outcome.rows_removed;
        }

        // Unlinked placeholders leave stale marks behind, and the directories
        // that just became partial need open notifications. One registration
        // pass reconciles both; without the helper the next connection does.
        if let Some(hsm) = self.hsm.read().await.clone() {
            if let Err(error) = hsm.watch_all(self).await {
                tracing::warn!(%error, "could not reconcile placeholder marks after conversion");
            }
        }
        self.refresh_pending_status().await;
        Ok(total)
    }

    /// Whether *this* process is the `gonedrived.service` unit.
    ///
    /// `INVOCATION_ID` alone is not that question. Every child of a
    /// systemd-managed process inherits it, so a daemon run by hand from a
    /// terminal — itself a unit — claimed to be a service and promised a
    /// restart nothing would perform. The control group path names the unit
    /// that actually owns the process, and a shell run from a terminal is in
    /// the terminal's group, not this one's.
    fn systemd_managed(&self) -> bool {
        std::env::var_os("INVOCATION_ID").is_some()
            && std::fs::read_to_string("/proc/self/cgroup")
                .is_ok_and(|cgroup| cgroup.contains("gonedrived.service"))
    }

    /// Restart through systemd once the current D-Bus reply has gone out.
    ///
    /// A failure here has to undo the pause. The pause exists only to keep the
    /// old policy from acting in the seconds before the process goes away; if
    /// it is not going away, leaving it paused strands the user with a daemon
    /// that has silently stopped syncing.
    fn spawn_restart(self: &Arc<Self>) {
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let outcome = tokio::process::Command::new("systemctl")
                .args(["--user", "restart", "gonedrived.service"])
                .status()
                .await;
            let failure = match outcome {
                Ok(status) if status.success() => return,
                Ok(status) => format!("systemctl exited with {status}"),
                Err(error) => format!("could not launch systemctl: {error}"),
            };
            tracing::error!(%failure, "the sync mode restart did not happen");
            app.set_paused(false).await;
            app.set_state(
                "idle",
                Some(format!(
                    "切换同步模式需要重启守护进程，但自动重启失败（{failure}）。请手动运行 systemctl --user restart gonedrived"
                )),
            )
            .await;
        });
    }
}
