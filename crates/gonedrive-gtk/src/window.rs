//! The main panel — what opens from the tray icon.
//!
//! A dashboard "home" answers "is my stuff synced" at a glance — account,
//! quota, status, the two actions people reach for, and a peek at what just
//! happened. Two more views hold the activity history and the files-on-demand
//! chooser; settings live behind the header menu. Three window states share the
//! shell: no daemon, signed out, and the panel itself.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gonedrive_ipc::{AccountInfo, Activity, FolderEntry, SyncStatus};
use gtk::{gio, glib};

pub struct Ui {
    pub window: adw::ApplicationWindow,
    stack: gtk::Stack,
    banner: adw::Banner,

    // -- home (overview) --
    avatar: adw::Avatar,
    account_name: gtk::Label,
    account_email: gtk::Label,
    quota_bar: gtk::ProgressBar,
    quota_used: gtk::Label,
    quota_free: gtk::Label,
    status_icon: gtk::Image,
    spinner: adw::Spinner,
    status_title: gtk::Label,
    status_sub: gtk::Label,
    transfer_box: gtk::Box,
    transfer_bar: gtk::ProgressBar,
    transfer_label: gtk::Label,
    peek_group: gtk::Box,
    peek_list: gtk::ListBox,

    // -- activity --
    activity_list: gtk::ListBox,
    activity_empty: adw::StatusPage,

    // -- files (on-demand chooser) --
    folders_list: gtk::ListBox,
    folders_path: gtk::Label,
    folders_back: gtk::Button,
    folders_current: Rc<RefCell<String>>,
    root_path: Rc<RefCell<String>>,
    folders_loaded: Cell<bool>,

    // -- settings (in the header menu) --
    pause_switch: gtk::Switch,
    folder_row: adw::ActionRow,

    login_button: gtk::Button,
    login_hint: gtk::Label,
    commands: async_channel::Sender<Command>,
    account_id: Rc<RefCell<Option<String>>>,
    suppress: Rc<Cell<bool>>,
    ready: Rc<Cell<bool>>,
    on_folders_page: Rc<Cell<bool>>,
    suppress_folder_toggle: Rc<Cell<bool>>,
}

use crate::backend::{Command, UiEvent};

/// A rounded card box with internal padding.
fn card() -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    b.add_css_class("card");
    b.add_css_class("od-card");
    b
}

impl Ui {
    pub fn build(app: &adw::Application, commands: async_channel::Sender<Command>) -> Self {
        // OneDrive blue as the accent, plus a little card padding. Loaded once,
        // application-wide, so switches/buttons/bars all pick up the brand.
        let css = gtk::CssProvider::new();
        css.load_from_string(
            // OneDrive blue as the accent. USER priority so it wins over
            // libadwaita's system-accent provider (which would otherwise leave
            // the buttons and switches on the desktop's accent, e.g. orange).
            // libadwaita 1.6 computes the accent from the desktop setting and
            // ignores @define-color, so the brand blue is applied directly to
            // the widgets that would otherwise carry the system accent.
            "@define-color accent_bg_color #0F80D8;\
             @define-color accent_color #6fb2ea;\
             .od-card{padding:14px;}\
             .od-quota trough,.od-quota progress{min-height:8px;border-radius:99px;}\
             .od-quota{min-height:8px;}\
             progressbar>trough>progress{background-color:#0F80D8;background-image:none;}\
             switch:checked{background-color:#0F80D8;}\
             .od-primary{background-color:#0F80D8;color:#ffffff;}\
             .od-primary:hover{background-color:#2a90e0;}\
             .od-primary:active{background-color:#0a63b4;}\
             .od-tile{border-radius:9px;}\
             .od-tile>image{color:#ffffff;}\
             .od-tile-blue{background:linear-gradient(160deg,#1E88D8,#0A63B4);}\
             .od-tile-green{background:linear-gradient(160deg,#2e7d51,#245f40);}\
             .od-tile-grey{background:linear-gradient(160deg,#4a4a4d,#37373a);}\
             .od-tile-folder{background:linear-gradient(160deg,#3a6ea8,#2b5486);}\
             .od-tile-red{background:linear-gradient(160deg,#a33a2b,#7e2b20);}\
             .od-tile-amber{background:linear-gradient(160deg,#9a7b2b,#6f5520);}",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
        }

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("GOnedrive")
            .default_width(440)
            .default_height(620)
            .build();

        let banner = adw::Banner::builder().revealed(false).build();

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
            .label("登录 OneDrive")
            .halign(gtk::Align::Center)
            .build();
        login_button.add_css_class("od-primary");
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

        let login_page = adw::StatusPage::builder()
            .title("连接你的 OneDrive")
            .description("登录后即可双向同步，并按需下载 —— 文件在磁盘上可见，打开才下载。")
            .child(&login_box)
            .build();
        match crate::trayicon::cloud_texture(96) {
            Some(texture) => login_page.set_paintable(Some(&texture)),
            None => login_page.set_icon_name(Some("folder-remote-symbolic")),
        }
        stack.add_named(&login_page, Some("login"));

        // ---- the panel: three views + a menu ------------------------------
        let views = adw::ViewStack::new();

        // == home ==
        let (overview, home_fields, sync_button, open_button) = Self::build_home();
        views
            .add_titled(&overview, Some("overview"), "主页")
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

        // == files (on-demand chooser) ==
        let (folders_scroll, folders_list, folders_path, folders_back) = Self::build_files();
        views
            .add_titled(&folders_scroll, Some("folders"), "文件")
            .set_icon_name(Some("folder-symbolic"));

        {
            let commands = commands.clone();
            views.connect_visible_child_name_notify(move |views| {
                if views.visible_child_name().as_deref() == Some("folders") {
                    let _ = commands.send_blocking(Command::ListFolder(String::new()));
                }
            });
        }
        if let Ok(page) = std::env::var("GONEDRIVE_PAGE") {
            views.set_visible_child_name(&page);
            if page == "folders" {
                let _ = commands.send_blocking(Command::ListFolder(String::new()));
            }
        }

        // "查看全部" on the home peek jumps to the activity view.
        {
            let views = views.clone();
            home_fields.peek_all.connect_clicked(move |_| {
                views.set_visible_child_name("activity");
            });
        }

        // == settings dialog (opened from the header menu) ==
        let (settings_dialog, pause_switch, folder_row, signout_button) = Self::build_settings();

        // Header: title + a primary menu. Settings/about/open-folder live here,
        // so the three views are all the tab bar carries.
        let menu = gio::Menu::new();
        menu.append(Some("设置"), Some("win.settings"));
        menu.append(Some("打开 OneDrive 文件夹"), Some("app.open-folder"));
        menu.append(Some("关于 GOnedrive"), Some("app.about"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("菜单")
            .build();

        let header = adw::HeaderBar::new();
        header.pack_end(&menu_button);

        // The settings action shows the dialog, reusing the one instance so the
        // pause switch it holds stays the one `apply_status` drives.
        let settings_action = gio::SimpleAction::new("settings", None);
        {
            let dialog = settings_dialog.clone();
            let parent = window.clone();
            settings_action.connect_activate(move |_, _| {
                dialog.present(Some(&parent));
            });
        }
        window.add_action(&settings_action);

        // Bottom switcher: the GNOME pattern for a narrow window — the header
        // stays clean and the three views never truncate.
        let switcher = adw::ViewSwitcherBar::builder().stack(&views).reveal(true).build();

        let panel = adw::ToolbarView::new();
        panel.add_top_bar(&header);
        panel.set_content(Some(&views));
        panel.add_bottom_bar(&switcher);
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
        let outer = outer_header.clone();
        stack.connect_visible_child_name_notify(move |stack| {
            outer.set_visible(stack.visible_child_name().as_deref() != Some("main"));
        });
        window.set_content(Some(&toolbar));

        let ui = Self {
            window,
            stack,
            banner,
            avatar: home_fields.avatar,
            account_name: home_fields.account_name,
            account_email: home_fields.account_email,
            quota_bar: home_fields.quota_bar,
            quota_used: home_fields.quota_used,
            quota_free: home_fields.quota_free,
            status_icon: home_fields.status_icon,
            spinner: home_fields.spinner,
            status_title: home_fields.status_title,
            status_sub: home_fields.status_sub,
            transfer_box: home_fields.transfer_box,
            transfer_bar: home_fields.transfer_bar,
            transfer_label: home_fields.transfer_label,
            peek_group: home_fields.peek_group,
            peek_list: home_fields.peek_list,
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

    /// The home dashboard and the widgets that need updating later.
    fn build_home() -> (gtk::ScrolledWindow, HomeFields, gtk::Button, gtk::Button) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .margin_start(14)
            .margin_end(14)
            .margin_top(14)
            .margin_bottom(14)
            .build();

        // --- account + quota card ---
        let account_card = card();
        let account_top = gtk::Box::builder().spacing(12).build();

        let avatar = adw::Avatar::builder().size(48).show_initials(true).build();
        // A little OneDrive cloud pinned to the avatar's corner.
        let avatar_overlay = gtk::Overlay::new();
        avatar_overlay.set_child(Some(&avatar));
        if let Some(texture) = crate::trayicon::cloud_texture(22) {
            let mark = gtk::Image::from_paintable(Some(&texture));
            mark.set_pixel_size(20);
            mark.set_halign(gtk::Align::End);
            mark.set_valign(gtk::Align::End);
            avatar_overlay.add_overlay(&mark);
        }
        account_top.append(&avatar_overlay);

        let names = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let account_name = gtk::Label::builder().xalign(0.0).build();
        account_name.set_markup("<b>未登录</b>");
        account_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let account_email = gtk::Label::builder().xalign(0.0).build();
        account_email.add_css_class("dim-label");
        account_email.add_css_class("caption");
        account_email.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        names.append(&account_name);
        names.append(&account_email);
        account_top.append(&names);
        account_card.append(&account_top);

        let quota_bar = gtk::ProgressBar::builder().margin_top(12).build();
        quota_bar.add_css_class("od-quota");
        account_card.append(&quota_bar);
        let quota_line = gtk::Box::builder().margin_top(6).build();
        let quota_used = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        quota_used.add_css_class("dim-label");
        quota_used.add_css_class("caption");
        let quota_free = gtk::Label::builder().xalign(1.0).build();
        quota_free.add_css_class("dim-label");
        quota_free.add_css_class("caption");
        quota_line.append(&quota_used);
        quota_line.append(&quota_free);
        account_card.append(&quota_line);
        root.append(&account_card);

        // --- status card ---
        let status_card = card();
        let status_top = gtk::Box::builder().spacing(12).build();
        let status_icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
        status_icon.set_pixel_size(24);
        let spinner = adw::Spinner::builder().visible(false).width_request(24).build();
        status_top.append(&status_icon);
        status_top.append(&spinner);
        let status_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let status_title = gtk::Label::builder().xalign(0.0).build();
        status_title.set_markup("<b>已是最新</b>");
        let status_sub = gtk::Label::builder().xalign(0.0).build();
        status_sub.add_css_class("dim-label");
        status_sub.add_css_class("caption");
        status_text.append(&status_title);
        status_text.append(&status_sub);
        status_top.append(&status_text);
        status_card.append(&status_top);

        // Transfer indicator, shown only while something is moving.
        let transfer_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(5)
            .margin_top(10)
            .visible(false)
            .build();
        let transfer_bar = gtk::ProgressBar::new();
        let transfer_label = gtk::Label::builder().xalign(0.0).build();
        transfer_label.add_css_class("dim-label");
        transfer_label.add_css_class("caption");
        transfer_box.append(&transfer_bar);
        transfer_box.append(&transfer_label);
        status_card.append(&transfer_box);
        root.append(&status_card);

        // --- quick actions ---
        let actions = gtk::Box::builder().spacing(10).homogeneous(true).build();
        let sync_button = gtk::Button::builder().build();
        sync_button.set_child(Some(&action_content("view-refresh-symbolic", "立即同步")));
        sync_button.add_css_class("od-primary");
        let open_button = gtk::Button::builder().build();
        open_button.set_child(Some(&action_content("folder-open-symbolic", "打开文件夹")));
        actions.append(&sync_button);
        actions.append(&open_button);
        root.append(&actions);

        // --- recent activity peek ---
        let peek_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(7)
            .margin_top(4)
            .visible(false)
            .build();
        let peek_head = gtk::Box::builder().build();
        let peek_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        peek_label.set_markup("<b>最近活动</b>");
        peek_label.add_css_class("caption-heading");
        let peek_all = gtk::Button::builder().label("查看全部").build();
        peek_all.add_css_class("flat");
        peek_head.append(&peek_label);
        peek_head.append(&peek_all);
        peek_group.append(&peek_head);
        let peek_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        peek_list.add_css_class("boxed-list");
        peek_group.append(&peek_list);
        root.append(&peek_group);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&root)
            .vexpand(true)
            .build();

        let fields = HomeFields {
            avatar,
            account_name,
            account_email,
            quota_bar,
            quota_used,
            quota_free,
            status_icon,
            spinner,
            status_title,
            status_sub,
            transfer_box,
            transfer_bar,
            transfer_label,
            peek_group,
            peek_list,
            peek_all: peek_all.clone(),
        };
        (scroll, fields, sync_button, open_button)
    }

    /// The files-on-demand chooser.
    fn build_files() -> (gtk::ScrolledWindow, gtk::ListBox, gtk::Label, gtk::Button) {
        let folders_back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("上一级")
            .sensitive(false)
            .build();
        folders_back.add_css_class("flat");
        let folders_path = gtk::Label::builder()
            .label("OneDrive")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        let crumb = gtk::Box::builder().spacing(6).build();
        crumb.append(&folders_back);
        crumb.append(&folders_path);

        let hint = gtk::Label::builder()
            .label("开关打开＝保留在本机；关闭＝释放空间，文件仍可随时打开。")
            .wrap(true)
            .xalign(0.0)
            .build();
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");

        let folders_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        folders_list.add_css_class("boxed-list");

        let folders_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        folders_box.append(&crumb);
        folders_box.append(&hint);
        folders_box.append(&folders_list);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&folders_box)
            .vexpand(true)
            .build();
        (scroll, folders_list, folders_path, folders_back)
    }

    /// The settings dialog, built once so the pause switch it owns persists.
    fn build_settings() -> (adw::PreferencesDialog, gtk::Switch, adw::ActionRow, gtk::Button) {
        let dialog = adw::PreferencesDialog::builder().title("设置").build();
        let page = adw::PreferencesPage::new();

        let sync_group = adw::PreferencesGroup::builder().title("同步").build();
        let pause_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let pause_row = adw::ActionRow::builder()
            .title("暂停同步")
            .subtitle("暂停期间本地改动会累积，恢复后一并处理")
            .build();
        pause_row.add_suffix(&pause_switch);
        pause_row.set_activatable_widget(Some(&pause_switch));
        sync_group.add(&pause_row);

        let folder_row = adw::ActionRow::builder()
            .title("同步文件夹")
            .subtitle("")
            .activatable(true)
            .build();
        folder_row.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        sync_group.add(&folder_row);
        page.add(&sync_group);

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
        page.add(&danger);

        dialog.add(&page);
        (dialog, pause_switch, folder_row, signout_button)
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

        open.connect_clicked(|button| run_app_action(button, "open-folder"));
        self.folder_row
            .connect_activated(|row| run_app_action(row, "open-folder"));
        signout.connect_clicked(|button| run_app_action(button, "sign-out"));

        let commands = self.commands.clone();
        let suppress = self.suppress.clone();
        self.pause_switch.connect_state_set(move |_, paused| {
            if !suppress.get() {
                let _ = commands.send_blocking(Command::SetPaused(paused));
            }
            glib::Propagation::Proceed
        });
    }

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
                self.show_banner(&format!("{original} 在两端都被修改，本地版本已保留为 {copy}"));
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

            // Local vs. online-only state, shown up front with a coloured dot
            // so the user can see at a glance what is taking up space. The
            // subtitle is our own string, so the markup is safe.
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
                format!("{dot} {word}{extra} · {} 项 · {}", entry.files, human(entry.bytes))
            } else {
                format!("{dot} {word}{extra} · {}", human(entry.bytes))
            };

            let row = adw::ActionRow::builder()
                .title(&entry.name)
                .title_lines(1)
                .subtitle(&size)
                .activatable(entry.is_folder)
                .build();
            row.add_prefix(&tile(
                if entry.is_folder { "folder-symbolic" } else { "text-x-generic-symbolic" },
                if entry.is_folder { "od-tile-folder" } else { "od-tile-grey" },
            ));

            // A navigate chevron for folders, then the keep switch. Files get an
            // empty chevron-width spacer instead, so every switch lines up in
            // one column down the right edge.
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

/// A coloured rounded tile with a white symbolic icon — the list-row leading
/// graphic from the design, replacing a bare symbolic prefix.
fn tile(icon: &str, variant: &str) -> gtk::Box {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    image.set_hexpand(true);
    image.set_vexpand(true);
    // A fixed 34×34 square, flush to the left of its slot rather than stretched
    // to fill it or floating in the middle.
    let b = gtk::Box::builder()
        .width_request(34)
        .height_request(34)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .build();
    b.add_css_class("od-tile");
    b.add_css_class(variant);
    b.append(&image);
    b
}

/// A button child: a symbolic icon above nothing — icon + label in a row.
fn action_content(icon: &str, label: &str) -> gtk::Box {
    let b = gtk::Box::builder()
        .spacing(7)
        .halign(gtk::Align::Center)
        .build();
    b.append(&gtk::Image::from_icon_name(icon));
    b.append(&gtk::Label::new(Some(label)));
    b
}

/// Activate an `app.` action from any widget in the window.
fn run_app_action(widget: &impl IsA<gtk::Widget>, name: &str) {
    if let Some(window) = widget.root().and_downcast::<gtk::Window>() {
        let _ = window
            .application()
            .and_then(|app| app.lookup_action(name))
            .map(|action| action.activate(None));
    }
}

/// One row of the activity / peek lists — a coloured directional tile, the
/// name, and what happened with when and how big.
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

/// The widgets `build_home` hands back for `apply` to drive.
struct HomeFields {
    avatar: adw::Avatar,
    account_name: gtk::Label,
    account_email: gtk::Label,
    quota_bar: gtk::ProgressBar,
    quota_used: gtk::Label,
    quota_free: gtk::Label,
    status_icon: gtk::Image,
    spinner: adw::Spinner,
    status_title: gtk::Label,
    status_sub: gtk::Label,
    transfer_box: gtk::Box,
    transfer_bar: gtk::ProgressBar,
    transfer_label: gtk::Label,
    peek_group: gtk::Box,
    peek_list: gtk::ListBox,
    peek_all: gtk::Button,
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
