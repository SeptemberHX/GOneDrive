//! Builders for the dashboard, files-on-demand, and settings views.

use adw::prelude::*;

use super::Ui;

/// A rounded card box with internal padding.
fn card() -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    card.add_css_class("card");
    card.add_css_class("od-card");
    card
}

impl Ui {
    /// The home dashboard and the widgets that need updating later.
    pub(super) fn build_home() -> (gtk::ScrolledWindow, HomeFields, gtk::Button, gtk::Button) {
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
        let spinner = adw::Spinner::builder()
            .visible(false)
            .width_request(24)
            .build();
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
    pub(super) fn build_files() -> (gtk::ScrolledWindow, gtk::ListBox, gtk::Label, gtk::Button) {
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
    pub(super) fn build_settings() -> (
        adw::PreferencesDialog,
        gtk::Switch,
        adw::ActionRow,
        gtk::Button,
    ) {
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
}

fn action_content(icon: &str, label: &str) -> gtk::Box {
    let content = gtk::Box::builder()
        .spacing(7)
        .halign(gtk::Align::Center)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    content
}

/// Widgets created with the home page that runtime events update later.
pub(super) struct HomeFields {
    pub(super) avatar: adw::Avatar,
    pub(super) account_name: gtk::Label,
    pub(super) account_email: gtk::Label,
    pub(super) quota_bar: gtk::ProgressBar,
    pub(super) quota_used: gtk::Label,
    pub(super) quota_free: gtk::Label,
    pub(super) status_icon: gtk::Image,
    pub(super) spinner: adw::Spinner,
    pub(super) status_title: gtk::Label,
    pub(super) status_sub: gtk::Label,
    pub(super) transfer_box: gtk::Box,
    pub(super) transfer_bar: gtk::ProgressBar,
    pub(super) transfer_label: gtk::Label,
    pub(super) peek_group: gtk::Box,
    pub(super) peek_list: gtk::ListBox,
    pub(super) peek_all: gtk::Button,
}
