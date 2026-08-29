//! Rendering of daemon events into the persistent window widgets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gonedrive_ipc::{AccountInfo, Activity, FolderEntry, SyncStatus};
use gtk::{gio, glib};

use super::{Command, Ui, UiEvent};

impl Ui {
    pub fn current_account(&self) -> Rc<RefCell<Option<String>>> {
        self.account_id.clone()
    }

    pub fn is_ready(&self) -> Rc<Cell<bool>> {
        self.ready.clone()
    }

    pub fn apply(&self, event: UiEvent) {
        match event {
            UiEvent::Connected { accounts, status } => {
                if !self.on_folders_page.get() {
                    self.ready.set(true);
                }
                self.banner.set_revealed(false);
                match accounts.first() {
                    Some(account) => {
                        *self.account_id.borrow_mut() = Some(account.id.clone());
                        self.show_account(account);
                        self.stack.set_visible_child_name("main");
                        self.apply_status(&status);
                        let _ = self.commands.send_blocking(Command::GetActivity);
                    }
                    None => {
                        *self.account_id.borrow_mut() = None;
                        self.reset_login_button();
                        self.stack.set_visible_child_name("login");
                    }
                }
            }
            UiEvent::Disconnected(reason) => {
                self.stack.set_visible_child_name("offline");
                tracing::debug!(%reason, "daemon unreachable");
            }
            UiEvent::Status(status) => {
                self.apply_status(&status);
                if status.state == "idle" {
                    let _ = self.commands.send_blocking(Command::GetActivity);
                }
            }
            UiEvent::Activity(items) => self.show_activity(&items),
            UiEvent::FolderListing { path, entries } => {
                self.ready.set(true);
                self.show_folders(&path, &entries);
            }
            UiEvent::LoginStarted(url) => {
                self.login_hint.set_visible(true);
                self.login_hint
                    .set_text(&format!("浏览器没有打开？手动访问：\n{url}"));
            }
            UiEvent::LoginCompleted => {
                self.reset_login_button();
                self.login_hint.set_visible(false);
                self.banner.set_revealed(false);
            }
            UiEvent::LoginFailed(message) => {
                self.reset_login_button();
                self.show_banner(&format!("登录失败：{message}"));
            }
            UiEvent::Conflict { original, copy } => {
                self.show_banner(&format!(
                    "{original} 在两端都被修改，本地版本已保留为 {copy}"
                ));
            }
            UiEvent::ItemFailed { name, error } => {
                self.show_banner(&format!("{name} 同步失败：{error}"));
            }
            UiEvent::HydrationStarted(h) | UiEvent::HydrationProgress(h) => {
                self.transfer_box.set_visible(true);
                self.transfer_label.set_text(&if h.total > 0 {
                    format!("{} · {} / {}", h.name, human(h.transferred), human(h.total))
                } else {
                    format!("{} · 正在连接…", h.name)
                });
                let fraction = if h.total > 0 {
                    h.transferred as f64 / h.total as f64
                } else {
                    0.0
                };
                self.transfer_bar.set_fraction(fraction.clamp(0.0, 1.0));
            }
            UiEvent::HydrationFinished { .. } => {
                self.transfer_box.set_visible(false);
            }
        }
    }

    fn reset_login_button(&self) {
        self.login_button.set_sensitive(true);
        self.login_button.set_label("登录 OneDrive");
    }

    fn show_banner(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }

    fn show_account(&self, account: &AccountInfo) {
        let name = if account.display_name.is_empty() {
            account.address.clone()
        } else {
            account.display_name.clone()
        };
        self.avatar.set_text(Some(&name));
        self.account_name
            .set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&name)));
        self.account_email.set_text(&account.address);
        self.folder_row.set_subtitle(&account.root_path);
        *self.root_path.borrow_mut() = account.root_path.clone();

        if account.quota_total > 0 {
            let used = account.quota_used as f64 / account.quota_total as f64;
            self.quota_bar.set_fraction(used.clamp(0.0, 1.0));
            self.quota_used.set_text(&format!(
                "已用 {} / {}",
                human(account.quota_used),
                human(account.quota_total)
            ));
            let free = account.quota_total.saturating_sub(account.quota_used);
            self.quota_free.set_text(&format!("剩余 {}", human(free)));
        } else {
            self.quota_used.set_text("配额未知");
            self.quota_free.set_text("");
        }
    }

    fn apply_status(&self, status: &SyncStatus) {
        let busy = matches!(
            status.state.as_str(),
            "syncing" | "enumerating" | "downloading" | "uploading"
        );
        self.spinner.set_visible(busy);
        self.status_icon.set_visible(!busy);

        let (title, icon) = match status.state.as_str() {
            "enumerating" => ("正在读取文件列表…", "emblem-synchronizing-symbolic"),
            "syncing" => ("正在同步…", "emblem-synchronizing-symbolic"),
            "downloading" => ("正在下载", "emblem-synchronizing-symbolic"),
            "uploading" => ("正在上传", "emblem-synchronizing-symbolic"),
            "paused" => ("已暂停", "media-playback-pause-symbolic"),
            "offline" => ("离线，稍后重试", "network-offline-symbolic"),
            "keyring_locked" => ("请解锁钥匙环", "dialog-password-symbolic"),
            "signed_out" => ("需要重新登录", "dialog-warning-symbolic"),
            "error" => ("同步出错", "dialog-error-symbolic"),
            _ if status.pending > 0 => return self.apply_pending(status),
            _ => ("已是最新", "emblem-ok-symbolic"),
        };
        self.status_title
            .set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
        self.status_icon.set_icon_name(Some(icon));
        self.status_sub.set_text(&status.last_error);
        self.status_sub.set_visible(!status.last_error.is_empty());

        if !busy || status.current_item.is_empty() {
            self.transfer_box.set_visible(false);
        } else {
            self.transfer_box.set_visible(true);
            let fraction = if status.total > 0 {
                status.transferred as f64 / status.total as f64
            } else {
                0.0
            };
            self.transfer_bar.set_fraction(fraction.clamp(0.0, 1.0));
            self.transfer_label.set_text(&format!(
                "{} · {} / {}",
                status.current_item,
                human(status.transferred),
                human(status.total)
            ));
        }

        let paused = status.state == "paused";
        if self.pause_switch.is_active() != paused {
            self.suppress.set(true);
            self.pause_switch.set_active(paused);
            self.suppress.set(false);
        }
    }

    fn apply_pending(&self, status: &SyncStatus) {
        self.spinner.set_visible(true);
        self.status_icon.set_visible(false);
        self.status_title.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&format!("等待处理 {} 项", status.pending))
        ));
        self.status_sub.set_visible(false);
        self.transfer_box.set_visible(false);
    }

    fn show_folders(&self, path: &str, entries: &[FolderEntry]) {
        self.suppress_folder_toggle.set(true);
        self.folders_loaded.set(true);
        *self.folders_current.borrow_mut() = path.to_string();
        self.folders_path.set_text(&if path.is_empty() {
            "OneDrive".into()
        } else {
            format!("OneDrive/{path}")
        });
        self.folders_back.set_sensitive(!path.is_empty());

        while let Some(child) = self.folders_list.first_child() {
            self.folders_list.remove(&child);
        }

        for entry in entries {
            let child_path = if path.is_empty() {
                entry.name.clone()
            } else {
                format!("{path}/{}", entry.name)
            };

            let (dot, word, extra) = if entry.selected {
                (
                    "<span foreground='#5bd98a'>●</span>",
                    "本机",
                    if entry.explicit { "" } else { " · 随上级" },
                )
            } else {
                ("<span foreground='#4aa3e8'>☁</span>", "仅在线", "")
            };
            let size = if entry.is_folder {
                format!(
                    "{dot} {word}{extra} · {} 项 · {}",
                    entry.files,
                    human(entry.bytes)
                )
            } else {
                format!("{dot} {word}{extra} · {}", human(entry.bytes))
            };

            let row = adw::ActionRow::builder()
                .title(&entry.name)
                .title_lines(1)
                .subtitle(&size)
                .activatable(entry.is_folder)
                .build();
            row.add_prefix(&file_icon(&entry.name, entry.is_folder));

            if entry.is_folder {
                let chevron = gtk::Image::from_icon_name("go-next-symbolic");
                chevron.add_css_class("dim-label");
                row.add_suffix(&chevron);
            } else {
                let spacer = gtk::Box::builder().width_request(16).build();
                row.add_suffix(&spacer);
            }

            let keep = gtk::Switch::builder()
                .active(entry.selected)
                .valign(gtk::Align::Center)
                .tooltip_text("保留在此设备")
                .build();
            let commands = self.commands.clone();
            let root = self.root_path.clone();
            let toggle_path = child_path.clone();
            let refresh_path = path.to_string();
            let suppress = self.suppress_folder_toggle.clone();
            keep.connect_state_set(move |switch, keep| {
                if suppress.get() {
                    return glib::Propagation::Proceed;
                }
                switch.set_sensitive(false);
                let absolute = format!("{}/{}", root.borrow(), toggle_path);
                let _ = commands.send_blocking(Command::SetKeep {
                    path: absolute,
                    keep,
                });
                let _ = commands.send_blocking(Command::ListFolder(refresh_path.clone()));
                glib::Propagation::Proceed
            });
            row.add_suffix(&keep);

            if entry.is_folder {
                let commands = self.commands.clone();
                row.connect_activated(move |_| {
                    let _ = commands.send_blocking(Command::ListFolder(child_path.clone()));
                });
            }
            self.folders_list.append(&row);
        }
        self.suppress_folder_toggle.set(false);
    }

    fn show_activity(&self, items: &[Activity]) {
        for list in [&self.activity_list, &self.peek_list] {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
        }
        self.activity_empty.set_visible(items.is_empty());
        self.activity_list.set_visible(!items.is_empty());
        self.peek_group.set_visible(!items.is_empty());

        for item in items.iter().take(50) {
            self.activity_list.append(&activity_row(item));
        }
        for item in items.iter().take(2) {
            self.peek_list.append(&activity_row(item));
        }
    }
}

fn file_icon(name: &str, is_folder: bool) -> gtk::Image {
    let content_type = if is_folder {
        glib::GString::from("inode/directory")
    } else {
        gio::content_type_guess(Some(std::path::Path::new(name)), None).0
    };
    let image = gtk::Image::from_gicon(&gio::content_type_get_icon(&content_type));
    image.set_pixel_size(32);
    image
}

fn tile(icon: &str, variant: &str) -> gtk::Box {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    image.set_halign(gtk::Align::Center);
    image.set_valign(gtk::Align::Center);
    image.set_hexpand(true);
    image.set_vexpand(true);
    let tile = gtk::Box::builder()
        .width_request(34)
        .height_request(34)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .hexpand(false)
        .vexpand(false)
        .build();
    tile.add_css_class("od-tile");
    tile.add_css_class(variant);
    tile.append(&image);
    tile
}

fn activity_row(item: &Activity) -> adw::ActionRow {
    let (icon, variant, verb) = match item.kind.as_str() {
        "downloaded" => ("go-down-symbolic", "od-tile-blue", "已下载"),
        "uploaded" => ("go-up-symbolic", "od-tile-green", "已上传"),
        "deleted" => ("user-trash-symbolic", "od-tile-grey", "已删除"),
        "conflict" => ("dialog-warning-symbolic", "od-tile-amber", "冲突已保留两份"),
        _ => ("dialog-error-symbolic", "od-tile-red", "失败"),
    };
    let subtitle = if item.detail.is_empty() {
        format!("{verb} · {}", relative_time(item.when))
    } else {
        format!("{verb} · {} · {}", relative_time(item.when), item.detail)
    };
    let row = adw::ActionRow::builder()
        .title(&item.name)
        .title_lines(1)
        .subtitle(&subtitle)
        .subtitle_lines(1)
        .build();
    row.add_prefix(&tile(icon, variant));
    row
}

fn relative_time(when: i64) -> String {
    let now = glib::DateTime::now_utc()
        .map(|time| time.to_unix())
        .unwrap_or(when);
    let elapsed = now.saturating_sub(when);
    match elapsed {
        i64::MIN..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", elapsed / 60),
        3600..=86_399 => format!("{} 小时前", elapsed / 3600),
        _ => glib::DateTime::from_unix_utc(when)
            .and_then(|time| time.to_local())
            .and_then(|time| time.format("%-m月%-d日"))
            .map(|text| text.to_string())
            .unwrap_or_else(|_| format!("{} 天前", elapsed / 86_400)),
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
