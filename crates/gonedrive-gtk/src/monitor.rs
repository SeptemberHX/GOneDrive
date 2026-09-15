//! The window that appears while a placeholder is being filled.
//!
//! Double-clicking a file that is only in the cloud freezes the application
//! that opened it until the content arrives. Without any indication of what is
//! happening, that reads as a hang — so this window says which file, how far
//! along it is, roughly how long is left, and offers a way out.
//!
//! It is spawned by the daemon on demand and closes itself once nothing is in
//! flight, so there is no tray icon to manage and nothing running when nothing
//! is downloading. On success it simply vanishes; only a failure lingers, to
//! explain the I/O error the waiting application just showed the user.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gonedrive_ipc::Hydration;
use gtk::{gio, glib};

use crate::backend::{Command, UiEvent};
use crate::window::human;

/// How long the window lingers, hidden, once the last transfer finishes.
///
/// Not zero: a burst of small reads would otherwise spawn and close the window
/// repeatedly. It hides immediately (nothing to celebrate) but stays alive this
/// long so a follow-up read re-presents it instead of flickering a new one.
const LINGER: Duration = Duration::from_secs(2);

/// Longer, and visible, after a failure: the message is the only explanation
/// the user gets for the error their application just showed them.
const FAILURE_LINGER: Duration = Duration::from_secs(8);

/// Rows shown at once. Beyond this the window would grow past anything useful,
/// and the count in the header carries the rest.
const MAX_ROWS: usize = 6;

/// A row for one file being filled.
///
/// Hand-built rather than an `AdwActionRow` so the progress bar can span the
/// full width under the name — the compact "download popover" look — and so
/// the detail line can carry a live time estimate.
struct Row {
    container: gtk::Box,
    name: String,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
    /// Smoothed transfer rate in bytes/sec, `0.0` until enough has arrived to
    /// estimate one. Kept here because the time-remaining figure is derived
    /// from it on every progress tick.
    rate: f64,
    /// Last `(when, transferred)` sample, for the next rate calculation.
    sample: Option<(Instant, u64)>,
}

pub struct Monitor {
    pub window: adw::ApplicationWindow,
    list: gtk::Box,
    rows: RefCell<HashMap<String, Row>>,
    /// The header title: "下载中" for one file, "N 个文件下载中" for several.
    title: gtk::Label,
    /// Shown only on failure — the error explanation. Hidden otherwise.
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
        // A height of -1 means "as tall as the content needs". Leaving it at the
        // toolkit default gives a slab of empty space under a single row.
        window.set_default_size(400, -1);

        let header = adw::HeaderBar::builder().build();
        header.add_css_class("flat");
        // Close only. A transient window that closes itself in a moment has no
        // business offering minimize — it only invites a mis-click.
        header.set_decoration_layout(Some(":close"));

        // A flat "downloading" title on the left: a glyph plus a live count.
        let glyph = gtk::Image::from_icon_name("folder-download-symbolic");
        glyph.add_css_class("dim-label");
        let title = gtk::Label::new(Some("OneDrive 下载中"));
        title.add_css_class("heading");
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        title_box.append(&glyph);
        title_box.append(&title);
        header.pack_start(&title_box);
        // Suppress the centred window title so it does not duplicate the above.
        header.set_title_widget(Some(&gtk::Label::new(None)));

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .build();

        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .margin_top(10)
            .build();
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        status.set_visible(false);

        // No scroller: it reports a fixed height rather than its content's, so
        // the window ends up with empty space under one row. The list is short
        // by construction — see `MAX_ROWS`.
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(15)
            .margin_end(15)
            .margin_top(4)
            .margin_bottom(15)
            .spacing(0)
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
            title,
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
        // Any prior failure message is stale the moment a new transfer starts.
        self.status.set_visible(false);
        let newly_shown = !self.window.is_visible();
        self.window.set_visible(true);

        let mut rows = self.rows.borrow_mut();
        if !rows.contains_key(&hydration.path) && rows.len() < MAX_ROWS {
            rows.insert(hydration.path.clone(), self.build_row(&hydration));
        }

        if let Some(row) = rows.get_mut(&hydration.path) {
            if hydration.total > 0 {
                let fraction = hydration.transferred as f64 / hydration.total as f64;
                row.progress.set_fraction(fraction.clamp(0.0, 1.0));

                // Update the smoothed rate from the change since the last tick,
                // then estimate the time left from it.
                let now = Instant::now();
                match row.sample {
                    Some((t0, b0)) => {
                        let dt = now.duration_since(t0).as_secs_f64();
                        if dt >= 0.2 && hydration.transferred > b0 {
                            let instant = (hydration.transferred - b0) as f64 / dt;
                            row.rate = if row.rate <= 0.0 {
                                instant
                            } else {
                                row.rate * 0.7 + instant * 0.3
                            };
                            row.sample = Some((now, hydration.transferred));
                        }
                    }
                    None => row.sample = Some((now, hydration.transferred)),
                }

                let pct = (fraction * 100.0).round() as u32;
                let remaining = hydration.total.saturating_sub(hydration.transferred);
                let eta = if row.rate > 1.0 && remaining > 0 {
                    format!(" · 剩余约 {}", human_duration(remaining as f64 / row.rate))
                } else {
                    String::new()
                };
                row.detail.set_text(&format!(
                    "{pct}% · {} / {}{eta}",
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
        self.title.set_text(&match count {
            0 | 1 => "OneDrive 下载中".to_string(),
            n => format!("OneDrive · {n} 个文件下载中"),
        });
        // Present only when the window first appears. Every progress tick
        // used to re-present it, which on X11 steals focus from whatever the
        // user is typing into for the whole download.
        if newly_shown {
            self.window.present();
        }
    }

    fn build_row(self: &Rc<Self>, hydration: &Hydration) -> Row {
        // The file-type icon from the desktop's own theme, so it matches what
        // the file manager shows for the same file.
        let (content_type, _) =
            gio::content_type_guess(Some(std::path::Path::new(&hydration.name)), None);
        let icon = gtk::Image::from_gicon(&gio::content_type_get_icon(&content_type));
        icon.set_pixel_size(28);
        icon.set_valign(gtk::Align::Center);

        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .hexpand(true)
            .build();
        // Bold, body-sized: the file name is the one thing the user must read.
        name.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&hydration.name)
        ));

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

        let top = gtk::Box::builder().spacing(10).build();
        top.append(&icon);
        top.append(&name);
        top.append(&cancel);

        let progress = gtk::ProgressBar::new();

        let detail = gtk::Label::builder().xalign(0.0).build();
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");

        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(7)
            .build();
        container.append(&top);
        container.append(&progress);
        container.append(&detail);
        self.list.append(&container);

        Row {
            container,
            name: hydration.name.clone(),
            progress,
            detail,
            rate: 0.0,
            sample: None,
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

    /// Linger, then close. On success the window vanishes at once and is only
    /// kept alive, hidden, long enough that a burst of reads re-presents it
    /// rather than flickering a new one. On failure it stays visible and
    /// explains the error, because the application that was waiting has just
    /// been handed an I/O error and the user deserves to know why.
    fn schedule_close(self: &Rc<Self>, ok: bool, name: &str) {
        self.cancel_close();
        if ok {
            // No "完成": nothing to announce, just get out of the way.
            self.window.set_visible(false);
        } else {
            self.status.set_visible(true);
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

/// A rough, human duration: "8 秒", "2 分 10 秒", "1 小时 5 分".
fn human_duration(secs: f64) -> String {
    let s = secs.round().max(1.0) as u64;
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        let (m, r) = (s / 60, s % 60);
        if r == 0 {
            format!("{m} 分")
        } else {
            format!("{m} 分 {r} 秒")
        }
    } else {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 {
            format!("{h} 小时")
        } else {
            format!("{h} 小时 {m} 分")
        }
    }
}
