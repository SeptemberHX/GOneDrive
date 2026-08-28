//! The main panel — what opens from the tray icon.
//!
//! Modelled on the Windows client's flyout: one glance answers "is my stuff
//! synced", one tab shows what just happened, one tab holds the few settings
//! that matter. Three states share the window: no daemon, signed out, and the
//! panel itself.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gonedrive_ipc::{AccountInfo, Activity, FolderEntry, SyncStatus};
use gtk::glib;

use crate::backend::{Command, UiEvent};

pub struct Ui {
    pub window: adw::ApplicationWindow,
    stack: gtk::Stack,
    banner: adw::Banner,

    // -- overview page --
    avatar: adw::Avatar,
    account_row: adw::ActionRow,
    quota_bar: gtk::LevelBar,
    quota_label: gtk::Label,
    status_row: adw::ActionRow,
    status_icon: gtk::Image,
    spinner: adw::Spinner,
    transfer_row: adw::ActionRow,
    transfer_bar: gtk::ProgressBar,

    // -- activity page --
    activity_list: gtk::ListBox,
    activity_empty: adw::StatusPage,

    // -- folder chooser --
    folders_list: gtk::ListBox,
    folders_path: gtk::Label,
    folders_back: gtk::Button,
    folders_current: Rc<RefCell<String>>,
    /// Sync root from the daemon; SetPinState/FreeUpSpace address by absolute
    /// path, since their other caller is the file manager.
    root_path: Rc<RefCell<String>>,
    folders_loaded: Cell<bool>,

    // -- settings page --
    pause_switch: gtk::Switch,
    folder_row: adw::ActionRow,

    login_button: gtk::Button,
    login_hint: gtk::Label,
    commands: async_channel::Sender<Command>,
    account_id: Rc<RefCell<Option<String>>>,
    suppress: Rc<Cell<bool>>,
    /// True once the first Connected has been applied. The snapshot hook waits
    /// on this so a capture never races the initial D-Bus round-trip.
    ready: Rc<Cell<bool>>,
    on_folders_page: Rc<Cell<bool>>,
    /// Set while rebuilding the folder list, so the switches' own state-set
    /// signals during population do not fire commands.
    suppress_folder_toggle: Rc<Cell<bool>>,
}

impl Ui {
    pub fn build(app: &adw::Application, commands: async_channel::Sender<Command>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("GOnedrive")
            .default_width(420)
            .default_height(600)
            .build();

        let banner = adw::Banner::builder().revealed(false).build();

        // The crossfade needs the frame clock; on a fully-occluded window the
        // clock is paused and the transition sits at frame zero — logically
        // switched, visually stuck. Harmless for people (revealing the window
        // resumes the clock), fatal for automated captures, so the snapshot
        // hook runs without it.
        let stack = gtk::Stack::builder()
            .transition_type(if std::env::var_os("GONEDRIVE_SNAPSHOT").is_some() {
                gtk::StackTransitionType::None
            } else {
                gtk::StackTransitionType::Crossfade
            })
            .vexpand(true)
            .build();

        // ---- page: the daemon is not running -----------------------------
        stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("network-offline-symbolic")
                .title("后台服务未运行")
                .description("在终端运行 `gonedrived run` 后，此窗口会自动连接。")
                .build(),
            Some("offline"),
        );

        // ---- page: signed out --------------------------------------------
        let login_button = gtk::Button::builder()
            .label("登录 Microsoft 账户")
            .halign(gtk::Align::Center)
            .build();
        login_button.add_css_class("suggested-action");
        login_button.add_css_class("pill");

        let login_hint = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .visible(false)
            .selectable(true)
            .build();
        login_hint.add_css_class("dim-label");
        login_hint.add_css_class("caption");

        let login_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        login_box.append(&login_button);
        login_box.append(&login_hint);

        stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("folder-remote-symbolic")
                .title("连接到 OneDrive")
                .description("登录后，你的 OneDrive 会同步到本地文件夹。")
                .child(&login_box)
                .build(),
            Some("login"),
        );

        // ---- the panel: three tabs ---------------------------------------
        let views = adw::ViewStack::new();

        // == overview ==
        let overview = adw::PreferencesPage::new();

        let account_group = adw::PreferencesGroup::new();
        let account_row = adw::ActionRow::builder().title("未登录").build();
        let avatar = adw::Avatar::builder().size(40).show_initials(true).build();
        account_row.add_prefix(&avatar);
        account_group.add(&account_row);

        let quota_bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let quota_label = gtk::Label::new(None);
        quota_label.add_css_class("dim-label");
        quota_label.add_css_class("caption");
        let quota_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .valign(gtk::Align::Center)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        quota_box.append(&quota_bar);
        quota_box.append(&quota_label);
        let quota_row = adw::ActionRow::builder().title("存储空间").build();
        quota_row.add_suffix(&quota_box);
        account_group.add(&quota_row);
        overview.add(&account_group);

        let status_group = adw::PreferencesGroup::new();
        let status_row = adw::ActionRow::builder().title("已是最新").build();
        let status_icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
        let spinner = adw::Spinner::builder().visible(false).build();
        status_row.add_prefix(&status_icon);
        status_row.add_suffix(&spinner);
        status_group.add(&status_row);

        let transfer_bar = gtk::ProgressBar::builder()
            .valign(gtk::Align::Center)
            .width_request(160)
            .build();
        let transfer_row = adw::ActionRow::builder()
            .title_lines(1)
            .visible(false)
            .build();
        transfer_row.add_suffix(&transfer_bar);
        status_group.add(&transfer_row);
        overview.add(&status_group);

        let quick = adw::PreferencesGroup::new();
        let sync_button = gtk::Button::builder()
            .label("立即同步")
            .valign(gtk::Align::Center)
            .build();
        let sync_row = adw::ActionRow::builder().title("手动同步").build();
        sync_row.add_suffix(&sync_button);
        quick.add(&sync_row);

        let open_button = gtk::Button::builder()
            .label("打开")
            .valign(gtk::Align::Center)
            .build();
        let open_row = adw::ActionRow::builder().title("OneDrive 文件夹").build();
        open_row.add_suffix(&open_button);
        quick.add(&open_row);
        overview.add(&quick);

        views
            .add_titled(&overview, Some("overview"), "概览")
            .set_icon_name(Some("go-home-symbolic"));

        // == activity ==
        let activity_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        activity_list.add_css_class("boxed-list");

        let activity_empty = adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("还没有活动")
            .description("同步的文件会显示在这里。")
            .build();

        let activity_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        activity_box.append(&activity_empty);
        activity_box.append(&activity_list);
        let activity_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&activity_box)
            .vexpand(true)
            .build();

        views
            .add_titled(&activity_scroll, Some("activity"), "活动")
            .set_icon_name(Some("document-open-recent-symbolic"));

        // == folder chooser ==
        let folders_back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("上一级")
            .sensitive(false)
            .build();
        folders_back.add_css_class("flat");
        let folders_path = gtk::Label::builder()
            .label("/")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        folders_path.add_css_class("dim-label");
        let crumb = gtk::Box::builder().spacing(6).build();
        crumb.append(&folders_back);
        crumb.append(&folders_path);

        let folders_hint = gtk::Label::builder()
            .label("打开的文件夹会保留在此设备；关闭则释放本机空间，文件仍可随时打开。")
            .wrap(true)
            .xalign(0.0)
            .build();
        folders_hint.add_css_class("dim-label");
        folders_hint.add_css_class("caption");

        let folders_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        folders_list.add_css_class("boxed-list");

        let folders_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_start(12)
            .margin_end(12)
            .margin_top(10)
            .margin_bottom(12)
            .build();
        folders_box.append(&crumb);
        folders_box.append(&folders_hint);
        folders_box.append(&folders_list);

        let folders_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&folders_box)
            .vexpand(true)
            .build();
        views
            .add_titled(&folders_scroll, Some("folders"), "文件夹")
            .set_icon_name(Some("folder-symbolic"));

        // == settings ==
        let settings = adw::PreferencesPage::new();
        let sync_group = adw::PreferencesGroup::builder().title("同步").build();
        let pause_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let pause_row = adw::ActionRow::builder()
            .title("暂停同步")
            .subtitle("暂停期间本地改动会累积，恢复后一并处理")
            .build();
        pause_row.add_suffix(&pause_switch);
        sync_group.add(&pause_row);

        let folder_row = adw::ActionRow::builder()
            .title("同步文件夹")
            .subtitle("")
            .activatable(true)
            .build();
        folder_row.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        sync_group.add(&folder_row);
        settings.add(&sync_group);

        let danger = adw::PreferencesGroup::builder().title("账户").build();
        let signout_button = gtk::Button::builder()
            .label("退出登录")
            .valign(gtk::Align::Center)
            .build();
        signout_button.add_css_class("destructive-action");
        let signout_row = adw::ActionRow::builder()
            .title("退出登录")
            .subtitle("凭据会被移除，本地文件保留")
            .build();
        signout_row.add_suffix(&signout_button);
        danger.add(&signout_row);
        settings.add(&danger);

        views
            .add_titled(&settings, Some("settings"), "设置")
            .set_icon_name(Some("emblem-system-symbolic"));

        {
            // Loading the whole listing at start-up would race the first
            // enumeration; fetch a level only when the user actually looks.
            let commands = commands.clone();
            views.connect_visible_child_name_notify(move |views| {
                if views.visible_child_name().as_deref() == Some("folders") {
                    let _ = commands.send_blocking(Command::ListFolder(String::new()));
                }
            });
        }

        // Screenshot hook: pre-select a page so every tab can be captured
        // from a script. Harmless in normal use.
        //
        // Load explicitly after selecting: the notify handler above fires on
        // *changes*, and a page selected before the handler was connected —
        // here, or a restored last-page on a future launch — would otherwise
        // open empty until the user clicked away and back.
        if let Ok(page) = std::env::var("GONEDRIVE_PAGE") {
            views.set_visible_child_name(&page);
            if page == "folders" {
                let _ = commands.send_blocking(Command::ListFolder(String::new()));
            }
        }

        let switcher = adw::ViewSwitcher::builder()
            .stack(&views)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&switcher));

        let panel = adw::ToolbarView::new();
        panel.add_top_bar(&header);
        panel.set_content(Some(&views));
        stack.add_named(&panel, Some("main"));
        stack.set_visible_child_name("offline");

        // Outer shell: pages that have no header get a plain one.
        let outer_header = adw::HeaderBar::new();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        content.append(&banner);
        content.append(&stack);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&outer_header);
        toolbar.set_content(Some(&content));

        // The panel brings its own header (with the switcher); showing both
        // stacks a second bar on top. Swap them as the page changes.
        let outer = outer_header.clone();
        stack.connect_visible_child_name_notify(move |stack| {
            outer.set_visible(stack.visible_child_name().as_deref() != Some("main"));
        });

        window.set_content(Some(&toolbar));

        let ui = Self {
            window,
            stack,
            banner,
            avatar,
            account_row,
            quota_bar,
            quota_label,
            status_row,
            status_icon,
            spinner,
            transfer_row,
            transfer_bar,
            activity_list,
            activity_empty,
            folders_list,
            folders_path,
            folders_back,
            folders_current: Rc::new(RefCell::new(String::new())),
            root_path: Rc::new(RefCell::new(String::new())),
            folders_loaded: Cell::new(false),
            pause_switch,
            folder_row,
            login_button,
            login_hint,
            commands,
            account_id: Rc::new(RefCell::new(None)),
            suppress: Rc::new(Cell::new(false)),
            ready: Rc::new(Cell::new(false)),
            on_folders_page: Rc::new(Cell::new(
                std::env::var("GONEDRIVE_PAGE").as_deref() == Ok("folders"),
            )),
            suppress_folder_toggle: Rc::new(Cell::new(false)),
        };
        ui.connect(&sync_button, &open_button, &signout_button);
        ui
    }

    fn connect(&self, sync: &gtk::Button, open: &gtk::Button, signout: &gtk::Button) {
        let commands = self.commands.clone();
        let current = self.folders_current.clone();
        self.folders_back.connect_clicked(move |_| {
            let up = {
                let path = current.borrow();
                match path.rfind('/') {
                    Some(index) => path[..index].to_string(),
                    None => String::new(),
                }
            };
            let _ = commands.send_blocking(Command::ListFolder(up));
        });

        let commands = self.commands.clone();
        self.login_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("正在打开浏览器…");
            let _ = commands.send_blocking(Command::StartLogin);
        });

        let commands = self.commands.clone();
        sync.connect_clicked(move |_| {
            let _ = commands.send_blocking(Command::SyncNow);
        });

        open.connect_clicked(move |button| {
            if let Some(window) = button.root().and_downcast::<gtk::Window>() {
                let _ = window
                    .application()
                    .and_then(|app| app.lookup_action("open-folder"))
                    .map(|action| action.activate(None));
            }
        });
        self.folder_row.connect_activated(|row| {
            if let Some(window) = row.root().and_downcast::<gtk::Window>() {
                let _ = window
                    .application()
                    .and_then(|app| app.lookup_action("open-folder"))
                    .map(|action| action.activate(None));
            }
        });
        signout.connect_clicked(|button| {
            if let Some(window) = button.root().and_downcast::<gtk::Window>() {
                let _ = window
                    .application()
                    .and_then(|app| app.lookup_action("sign-out"))
                    .map(|action| action.activate(None));
            }
        });

        let commands = self.commands.clone();
        let suppress = self.suppress.clone();
        self.pause_switch.connect_state_set(move |_, paused| {
            if !suppress.get() {
                let _ = commands.send_blocking(Command::SetPaused(paused));
            }
            glib::Propagation::Proceed
        });
    }

    /// The account the sign-out action should target, if any.
    pub fn current_account(&self) -> Rc<RefCell<Option<String>>> {
        self.account_id.clone()
    }

    /// True once the first daemon reply has painted the panel.
    pub fn is_ready(&self) -> Rc<Cell<bool>> {
        self.ready.clone()
    }

    pub fn apply(&self, event: UiEvent) {
        match event {
            UiEvent::Connected { accounts, status } => {
                // On the folders page the meaningful "painted" moment is the
                // listing arriving, not the initial connect; the snapshot hook
                // keys on `ready`, so leave it for FolderListing there.
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
                // Something finished; the activity list is probably stale.
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
                self.show_banner(&format!("{original} 在两端都被修改，本地版本已保留为 {copy}"));
            }
            UiEvent::ItemFailed { name, error } => {
                self.show_banner(&format!("{name} 同步失败：{error}"));
            }
            UiEvent::HydrationStarted(h) | UiEvent::HydrationProgress(h) => {
                self.transfer_row.set_visible(true);
                self.transfer_row.set_title(&h.name);
                self.transfer_row.set_subtitle(&if h.total > 0 {
                    format!("按需下载 · {} / {}", human(h.transferred), human(h.total))
                } else {
                    "按需下载 · 正在连接…".to_string()
                });
                let fraction = if h.total > 0 {
                    h.transferred as f64 / h.total as f64
                } else {
                    0.0
                };
                self.transfer_bar.set_fraction(fraction.clamp(0.0, 1.0));
            }
            UiEvent::HydrationFinished { .. } => {
                self.transfer_row.set_visible(false);
            }
        }
    }

    fn reset_login_button(&self) {
        self.login_button.set_sensitive(true);
        self.login_button.set_label("登录 Microsoft 账户");
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
        self.account_row.set_title(&name);
        self.account_row.set_subtitle(&account.address);
        self.folder_row.set_subtitle(&account.root_path);
        *self.root_path.borrow_mut() = account.root_path.clone();

        if account.quota_total > 0 {
            let used = account.quota_used as f64 / account.quota_total as f64;
            self.quota_bar.set_value(used.clamp(0.0, 1.0));
            self.quota_label.set_text(&format!(
                "已用 {} / {}",
                human(account.quota_used),
                human(account.quota_total)
            ));
        } else {
            self.quota_label.set_text("配额未知");
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
            "enumerating" => ("正在读取文件列表…".to_string(), "emblem-synchronizing-symbolic"),
            "syncing" => ("正在同步…".to_string(), "emblem-synchronizing-symbolic"),
            "downloading" => ("正在下载".to_string(), "emblem-synchronizing-symbolic"),
            "uploading" => ("正在上传".to_string(), "emblem-synchronizing-symbolic"),
            "paused" => ("已暂停".to_string(), "media-playback-pause-symbolic"),
            "offline" => ("离线，稍后重试".to_string(), "network-offline-symbolic"),
            "signed_out" => ("需要重新登录".to_string(), "dialog-warning-symbolic"),
            "error" => ("同步出错".to_string(), "dialog-error-symbolic"),
            _ if status.pending > 0 => (
                format!("等待处理 {} 项", status.pending),
                "emblem-synchronizing-symbolic",
            ),
            _ => ("已是最新".to_string(), "emblem-ok-symbolic"),
        };
        self.status_row.set_title(&title);
        self.status_icon.set_icon_name(Some(icon));
        self.status_row.set_subtitle(&status.last_error);

        if status.current_item.is_empty() || !busy {
            if status.current_item.is_empty() {
                self.transfer_row.set_visible(false);
            }
        } else {
            self.transfer_row.set_visible(true);
            self.transfer_row.set_title(&status.current_item);
            let fraction = if status.total > 0 {
                status.transferred as f64 / status.total as f64
            } else {
                0.0
            };
            self.transfer_bar.set_fraction(fraction.clamp(0.0, 1.0));
            self.transfer_row.set_subtitle(&format!(
                "{} / {}",
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

    fn show_folders(&self, path: &str, entries: &[FolderEntry]) {
        // Populating the list flips each switch to its stored state, and on an
        // already-mapped ListBox that fires `state-set`. Without a guard the
        // very first row's handler re-issues ListFolder, which rebuilds the
        // list mid-population — the observed result was a 20-item folder
        // rendering as 2 rows. The guard makes programmatic flips inert.
        self.suppress_folder_toggle.set(true);
        self.folders_loaded.set(true);
        *self.folders_current.borrow_mut() = path.to_string();
        self.folders_path
            .set_text(&if path.is_empty() { "/".into() } else { format!("/{path}") });
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

            let mut subtitle = if entry.is_folder {
                format!("{} · {} 个文件", human(entry.bytes), entry.files)
            } else {
                human(entry.bytes)
            };
            if entry.selected && !entry.explicit {
                subtitle.push_str(" · 随上级保留");
            }

            let row = adw::ActionRow::builder()
                .title(&entry.name)
                .title_lines(1)
                .subtitle(&subtitle)
                .activatable(entry.is_folder)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(if entry.is_folder {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));

            // Built with the state already set, and connected only afterwards,
            // so populating the list cannot fire spurious commands.
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
                // Re-list once the change lands; commands are handled in
                // order, so this reflects the new state and re-enables rows.
                let _ = commands.send_blocking(Command::ListFolder(refresh_path.clone()));
                glib::Propagation::Proceed
            });
            row.add_suffix(&keep);

            if entry.is_folder {
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
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
        while let Some(child) = self.activity_list.first_child() {
            self.activity_list.remove(&child);
        }
        self.activity_empty.set_visible(items.is_empty());
        self.activity_list.set_visible(!items.is_empty());

        for item in items.iter().take(50) {
            let (icon, verb) = match item.kind.as_str() {
                "downloaded" => ("folder-download-symbolic", "已下载"),
                "uploaded" => ("send-to-symbolic", "已上传"),
                "deleted" => ("user-trash-symbolic", "已删除"),
                "conflict" => ("dialog-warning-symbolic", "冲突已保留两份"),
                _ => ("dialog-error-symbolic", "失败"),
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
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            self.activity_list.append(&row);
        }
    }
}

/// "刚刚" / "5 分钟前" / "3 小时前" / "8月27日"
fn relative_time(when: i64) -> String {
    let now = glib::DateTime::now_utc().map(|t| t.to_unix()).unwrap_or(when);
    let elapsed = now.saturating_sub(when);
    match elapsed {
        i64::MIN..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", elapsed / 60),
        3600..=86_399 => format!("{} 小时前", elapsed / 3600),
        _ => glib::DateTime::from_unix_utc(when)
            .and_then(|t| t.to_local())
            .and_then(|t| t.format("%-m月%-d日"))
            .map(|s| s.to_string())
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
