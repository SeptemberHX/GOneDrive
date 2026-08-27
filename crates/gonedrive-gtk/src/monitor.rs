//! The window that appears while a placeholder is being filled.
//!
//! Double-clicking a file that is only in the cloud freezes the application
//! that opened it until the content arrives. Without any indication of what is
//! happening, that reads as a hang — so this window says which file, how far
//! along it is, and offers a way out.
//!
//! It is spawned by the daemon on demand and closes itself once nothing is in
//! flight, so there is no tray icon to manage and nothing running when nothing
//! is downloading.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gonedrive_ipc::Hydration;
use gtk::glib;

use crate::backend::{Command, UiEvent};
use crate::window::human;

/// How long the window lingers once the last transfer finishes.
///
/// Not zero: a burst of small reads would otherwise open and close it
/// repeatedly, which is worse than a window that waits a moment.
const LINGER: Duration = Duration::from_secs(2);

/// Longer after a failure: the message is the only explanation the user gets
/// for the error their application just showed them.
const FAILURE_LINGER: Duration = Duration::from_secs(8);

/// Rows shown at once. Beyond this the window would grow past anything useful,
/// and the count in the status line carries the rest.
const MAX_ROWS: usize = 6;

/// A row for one file being filled.
///
/// Hand-built rather than an `AdwActionRow` so the progress bar can span the
/// full width: squeezed into a suffix it leaves the file name truncated, and
/// the file name is the one thing the user needs to read.
struct Row {
    container: gtk::Box,
    name: String,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
}

pub struct Monitor {
    pub window: adw::ApplicationWindow,
    list: gtk::Box,
    rows: RefCell<HashMap<String, Row>>,
    status: gtk::Label,
    commands: async_channel::Sender<Command>,
    /// Pending close, cancelled if another transfer starts first.
    closing: RefCell<Option<glib::SourceId>>,
}

impl Monitor {
    pub fn build(app: &adw::Application, commands: async_channel::Sender<Command>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("正在从 OneDrive 下载")
            .resizable(false)
            .build();
        // A height of -1 means "as tall as the content needs". Leaving it at
        // the toolkit default gives a window with a slab of empty space under
        // a single row, and `resizable(false)` alone does not shrink it.
        window.set_default_size(440, -1);

        let header = adw::HeaderBar::builder()
            .show_end_title_buttons(true)
            .build();
        header.add_css_class("flat");

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .build();

        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .margin_top(10)
            .build();
        status.add_css_class("dim-label");
        status.add_css_class("caption");

        // No scroller: it reports a fixed height rather than its content's, so
        // the window ends up with a slab of empty space under one row. The
        // list is short by construction — see `MAX_ROWS`.
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(14)
            .spacing(4)
            .build();
        content.append(&list);
        content.append(&status);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        Rc::new(Self {
            window,
            list,
            rows: RefCell::new(HashMap::new()),
            status,
            commands,
            closing: RefCell::new(None),
        })
    }

    pub fn apply(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::HydrationStarted(hydration) => self.upsert(hydration),
            UiEvent::HydrationProgress(hydration) => self.upsert(hydration),
            UiEvent::HydrationFinished { path, ok } => self.remove(&path, ok),
            UiEvent::Disconnected(_) => {
                // The daemon is what feeds this window; without it there is
                // nothing left to report.
                self.window.close();
            }
            _ => {}
        }
    }

    fn upsert(self: &Rc<Self>, hydration: Hydration) {
        self.cancel_close();

        let mut rows = self.rows.borrow_mut();
        if !rows.contains_key(&hydration.path) && rows.len() < MAX_ROWS {
            rows.insert(hydration.path.clone(), self.build_row(&hydration));
        }

        if let Some(row) = rows.get(&hydration.path) {
            if hydration.total > 0 {
                let fraction = hydration.transferred as f64 / hydration.total as f64;
                row.progress.set_fraction(fraction.clamp(0.0, 1.0));
                row.detail.set_text(&format!(
                    "{} / {}",
                    human(hydration.transferred),
                    human(hydration.total)
                ));
            } else {
                // The size is unknown until the first response header lands. A
                // pulsing bar is honest about that; a 0% bar looks stalled.
                row.progress.pulse();
                row.detail.set_text("正在连接…");
            }
        }

        let count = rows.len();
        drop(rows);
        self.status.set_text(&match count {
            0 | 1 => "打开的文件正在下载，稍候即可使用".to_string(),
            n => format!("{n} 个文件正在下载"),
        });
        self.window.present();
    }

    fn build_row(self: &Rc<Self>, hydration: &Hydration) -> Row {
        let name = gtk::Label::builder()
            .label(&hydration.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .hexpand(true)
            .build();

        let cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("停止下载")
            .build();
        cancel.add_css_class("flat");
        cancel.add_css_class("circular");

        let commands = self.commands.clone();
        let path = hydration.path.clone();
        cancel.connect_clicked(move |button| {
            // Disabled at once: cancelling twice is meaningless, and the reply
            // takes a moment to come back.
            button.set_sensitive(false);
            let _ = commands.send_blocking(Command::CancelHydration(path.clone()));
        });

        let detail = gtk::Label::builder().xalign(1.0).build();
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");

        let top = gtk::Box::builder().spacing(8).build();
        top.append(&name);
        top.append(&detail);
        top.append(&cancel);

        let progress = gtk::ProgressBar::new();

        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        container.append(&top);
        container.append(&progress);
        self.list.append(&container);

        Row {
            container,
            name: hydration.name.clone(),
            progress,
            detail,
        }
    }

    fn remove(self: &Rc<Self>, path: &str, ok: bool) {
        let name = self
            .rows
            .borrow_mut()
            .remove(path)
            .map(|row| {
                self.list.remove(&row.container);
                row.name
            })
            .unwrap_or_default();

        if self.rows.borrow().is_empty() {
            self.schedule_close(ok, &name);
        }
    }

    /// Linger, then close — but stay longer on a failure, because the
    /// application that was waiting has just been handed an I/O error and the
    /// user deserves to know why.
    fn schedule_close(self: &Rc<Self>, ok: bool, name: &str) {
        self.cancel_close();
        if ok {
            self.status.set_text("完成");
        } else {
            self.status.set_text(&if name.is_empty() {
                "下载未完成，打开该文件的程序会收到读取错误".to_string()
            } else {
                format!("{name} 下载未完成，打开它的程序会收到读取错误")
            });
        }
        let linger = if ok { LINGER } else { FAILURE_LINGER };
        let this = self.clone();
        let id = glib::timeout_add_local_once(linger, move || {
            this.closing.replace(None);
            if this.rows.borrow().is_empty() {
                this.window.close();
            }
        });
        self.closing.replace(Some(id));
    }

    fn cancel_close(&self) {
        if let Some(id) = self.closing.replace(None) {
            id.remove();
        }
    }
}
