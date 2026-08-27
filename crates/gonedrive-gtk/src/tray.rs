//! The status-notifier item — the icon that sits in the top bar.
//!
//! Lives on the backend thread with the rest of the D-Bus work, because the
//! StatusNotifierItem protocol is itself D-Bus and the tray has to keep
//! answering the shell whether or not a window is open. It talks to the UI the
//! same way everything else does: through the channels.

use gonedrive_ipc::{AccountInfo, SyncStatus};
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Handle, MenuItem, Tray, TrayMethods};

use crate::backend::Command;

/// What the tray asks the UI to do.
#[derive(Debug, Clone)]
pub enum TrayAction {
    ShowPanel,
    OpenFolder,
    Quit,
}

pub struct GonedriveTray {
    status: SyncStatus,
    account: Option<AccountInfo>,
    /// Requests aimed at the window, which lives on the GTK thread.
    actions: async_channel::Sender<TrayAction>,
    /// Requests aimed at the daemon.
    commands: async_channel::Sender<Command>,
}

impl GonedriveTray {
    pub fn new(
        actions: async_channel::Sender<TrayAction>,
        commands: async_channel::Sender<Command>,
    ) -> Self {
        Self {
            status: SyncStatus::idle(0),
            account: None,
            actions,
            commands,
        }
    }

    /// Put the tray on the bus.
    pub async fn start(self) -> Result<Handle<Self>, ksni::Error> {
        self.spawn().await
    }

    pub fn set_status(&mut self, status: SyncStatus) {
        self.status = status;
    }

    pub fn set_account(&mut self, account: Option<AccountInfo>) {
        self.account = account;
    }

    fn is_paused(&self) -> bool {
        self.status.state == "paused"
    }

    /// One line describing what the daemon is doing, for the tooltip and for
    /// the inert first row of the menu.
    fn summary(&self) -> String {
        match self.status.state.as_str() {
            "syncing" | "enumerating" => "正在同步…".into(),
            "downloading" => match self.status.current_item.as_str() {
                "" => "正在下载".into(),
                name => format!("正在下载 {name}"),
            },
            "uploading" => match self.status.current_item.as_str() {
                "" => "正在上传".into(),
                name => format!("正在上传 {name}"),
            },
            "paused" => "已暂停".into(),
            "offline" => "离线，稍后重试".into(),
            "signed_out" => "需要重新登录".into(),
            "error" => "同步出错".into(),
            _ if self.status.pending > 0 => format!("等待处理 {} 项", self.status.pending),
            _ => "已是最新".into(),
        }
    }
}

impl Tray for GonedriveTray {
    fn id(&self) -> String {
        "org.gonedrive.Gonedrive".into()
    }

    fn title(&self) -> String {
        "GOnedrive".into()
    }

    /// Theme icon names rather than bundled pixmaps, so the icon follows the
    /// user's theme and their light/dark preference without us redrawing it.
    fn icon_name(&self) -> String {
        match self.status.state.as_str() {
            "syncing" | "enumerating" | "downloading" | "uploading" => {
                "emblem-synchronizing-symbolic".into()
            }
            "paused" => "media-playback-pause-symbolic".into(),
            "offline" => "network-offline-symbolic".into(),
            "signed_out" | "error" => "dialog-warning-symbolic".into(),
            _ => "folder-remote-symbolic".into(),
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "GOnedrive".into(),
            description: self.summary(),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Left-click opens the panel, which is what every other sync client does
    /// and therefore what people expect.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send_blocking(TrayAction::ShowPanel);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let account = self
            .account
            .as_ref()
            .map(|a| {
                if a.display_name.is_empty() {
                    a.address.clone()
                } else {
                    a.display_name.clone()
                }
            })
            .unwrap_or_else(|| "未登录".into());

        vec![
            // Inert: the menu's own header, showing who and what.
            StandardItem {
                label: account,
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.summary(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "打开 OneDrive 文件夹".into(),
                icon_name: "folder-open-symbolic".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send_blocking(TrayAction::OpenFolder);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "显示 GOnedrive".into(),
                icon_name: "view-list-symbolic".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send_blocking(TrayAction::ShowPanel);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "暂停同步".into(),
                checked: self.is_paused(),
                activate: Box::new(|tray: &mut Self| {
                    let paused = !tray.is_paused();
                    let _ = tray.commands.send_blocking(Command::SetPaused(paused));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "立即同步".into(),
                icon_name: "view-refresh-symbolic".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send_blocking(Command::SyncNow);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "更多".into(),
                submenu: vec![StandardItem {
                    label: "退出 GOnedrive".into(),
                    icon_name: "application-exit-symbolic".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.send_blocking(TrayAction::Quit);
                    }),
                    ..Default::default()
                }
                .into()],
                ..Default::default()
            }
            .into(),
        ]
    }
}
