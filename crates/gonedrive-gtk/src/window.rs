//! The main panel — what opens from the tray icon.
//!
//! A dashboard "home" answers "is my stuff synced" at a glance — account,
//! quota, status, the two actions people reach for, and a peek at what just
//! happened. Two more views hold the activity history and the files-on-demand
//! chooser; settings live behind the header menu. Three window states share the
//! shell: no daemon, signed out, and the panel itself.

mod activity;
mod builders;
mod render;

pub use render::human;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
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
             .od-tile-amber{background:linear-gradient(160deg,#9a7b2b,#6f5520);}\
             .od-reveal{color:#4a90d9;}",
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
        let switcher = adw::ViewSwitcherBar::builder()
            .stack(&views)
            .reveal(true)
            .build();

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
