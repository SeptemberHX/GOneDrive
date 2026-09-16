//! Recent-activity rows and safe file-manager navigation.

use std::path::{Component, Path, PathBuf};

use adw::prelude::*;
use gonedrive_ipc::Activity;
use gtk::glib::variant::ToVariant;
use gtk::{gio, glib};

struct Visual {
    icon: &'static str,
    variant: &'static str,
    verb: &'static str,
    consumes_detail: bool,
}

fn visual(kind: &str, detail: &str) -> Visual {
    match kind {
        "downloaded" if detail == "按需" => Visual {
            icon: "document-open-symbolic",
            variant: "od-tile-blue",
            verb: "按需下载",
            consumes_detail: true,
        },
        "downloaded" => Visual {
            icon: "go-down-symbolic",
            variant: "od-tile-blue",
            verb: "同步下载",
            consumes_detail: false,
        },
        "uploaded" => Visual {
            icon: "go-up-symbolic",
            variant: "od-tile-green",
            verb: "已上传",
            consumes_detail: false,
        },
        "deleted" => Visual {
            icon: "user-trash-symbolic",
            variant: "od-tile-grey",
            verb: "已删除",
            consumes_detail: false,
        },
        "conflict" => Visual {
            icon: "dialog-warning-symbolic",
            variant: "od-tile-amber",
            verb: "冲突已保留两份",
            consumes_detail: false,
        },
        _ => Visual {
            icon: "dialog-error-symbolic",
            variant: "od-tile-red",
            verb: "失败",
            consumes_detail: false,
        },
    }
}

pub(super) fn row(item: &Activity, root: &Path) -> adw::ActionRow {
    let visual = visual(&item.kind, &item.detail);
    let detail = if visual.consumes_detail {
        ""
    } else {
        &item.detail
    };
    let event = if detail.is_empty() {
        format!("{} · {}", visual.verb, relative_time(item.when))
    } else {
        format!("{} · {} · {detail}", visual.verb, relative_time(item.when))
    };
    let path = if item.path.is_empty() {
        "OneDrive/（旧记录未保存路径）"
    } else {
        &item.path
    };
    let target = activity_target(root, &item.path);
    let subtitle = format!(
        "<span foreground=\"#4a90d9\" weight=\"semibold\">{}</span>\n{}",
        glib::markup_escape_text(path),
        glib::markup_escape_text(&event)
    );
    // Build without untrusted strings, then escape every value received from
    // Graph before enabling markup for the highlighted path.
    let row = adw::ActionRow::builder()
        .title_lines(1)
        .subtitle_lines(2)
        .activatable(target.is_some())
        .build();
    row.set_use_markup(true);
    row.set_title(&glib::markup_escape_text(&item.name));
    row.set_subtitle(&subtitle);
    row.set_tooltip_text(Some(if target.is_some() {
        "在文件中显示"
    } else {
        path
    }));
    row.add_prefix(&tile(visual.icon, visual.variant));
    if let Some(target) = target {
        let reveal = gtk::Image::from_icon_name("folder-open-symbolic");
        reveal.add_css_class("od-reveal");
        reveal.set_tooltip_text(Some("在文件中显示"));
        row.add_suffix(&reveal);
        let root = root.to_path_buf();
        row.connect_activated(move |_| reveal_activity_target(&root, &target));
    }
    row
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

/// Resolve a durable display path beneath the current local root. Refuse
/// absolute paths and `..` so an old or corrupt record cannot navigate outside
/// OneDrive when clicked.
fn activity_target(root: &Path, display_path: &str) -> Option<PathBuf> {
    if !root.is_absolute() {
        return None;
    }
    if display_path == "OneDrive" {
        return Some(root.to_path_buf());
    }
    let relative = Path::new(display_path.strip_prefix("OneDrive/")?);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

/// Reveal without opening the file. Missing/deleted targets fall back to the
/// closest existing parent folder instead of triggering a document launch.
fn reveal_activity_target(root: &Path, target: &Path) {
    let (path, method) = if target.exists() {
        (target.to_path_buf(), "ShowItems")
    } else {
        let parent = target
            .ancestors()
            .skip(1)
            .take_while(|candidate| candidate.starts_with(root))
            .find(|candidate| candidate.is_dir())
            .unwrap_or(root)
            .to_path_buf();
        (parent, "ShowFolders")
    };
    let uri = gio::File::for_path(&path).uri().to_string();
    let fallback = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(root).to_path_buf()
    };
    let fallback_uri = gio::File::for_path(fallback).uri().to_string();

    gio::bus_get(
        gio::BusType::Session,
        None::<&gio::Cancellable>,
        move |connection| match connection {
            Ok(connection) => {
                let parameters = (vec![uri], String::new()).to_variant();
                connection.call(
                    Some("org.freedesktop.FileManager1"),
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1",
                    method,
                    Some(&parameters),
                    None,
                    gio::DBusCallFlags::NONE,
                    5_000,
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Err(error) = result {
                            tracing::warn!(%error, "file manager could not reveal activity item");
                            open_folder_fallback(&fallback_uri);
                        }
                    },
                );
            }
            Err(error) => {
                tracing::warn!(%error, "could not connect to the session bus");
                open_folder_fallback(&fallback_uri);
            }
        },
    );
}

fn open_folder_fallback(uri: &str) {
    gio::AppInfo::launch_default_for_uri_async(
        uri,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        |result| {
            if let Err(error) = result {
                tracing::warn!(%error, "could not open the activity item's folder");
            }
        },
    );
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

#[cfg(test)]
mod tests {
    use gtk::glib::{self, LogLevels};

    use super::*;

    #[test]
    fn activity_rows_are_safe_to_refresh_in_realized_lists() {
        if gtk::init().is_err() {
            // Locally this is an ordinary skip: a headless shell has no
            // display and that is not a failure. On CI it is one. This test
            // used to return early there too, so the job went green having
            // never executed a single line of it.
            assert!(
                std::env::var_os("CI").is_none(),
                "GTK could not open a display; run the suite under xvfb-run"
            );
            eprintln!("skipping: no display available");
            return;
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let window = gtk::Window::new();
        window.set_child(Some(&list));
        window.present();
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }

        let previous_fatal =
            glib::log_set_always_fatal(LogLevels::LEVEL_WARNING | LogLevels::LEVEL_CRITICAL);
        let detail = "graph error 411: <!DOCTYPE HTML><HTML><HEAD><META></HEAD></HTML>";
        let row = row(
            &Activity {
                kind: "failed".into(),
                name: "remote-error.html".into(),
                path: "OneDrive/Test/remote-error.html".into(),
                detail: detail.into(),
                when: 0,
            },
            Path::new("/home/u/OneDrive"),
        );
        list.append(&row);
        while context.pending() {
            context.iteration(false);
        }
        list.remove(&row);
        list.append(&row);
        while context.pending() {
            context.iteration(false);
        }
        glib::log_set_always_fatal(previous_fatal);

        assert!(row.uses_markup());
        assert!(row.is_activatable());
        assert!(!row.is_subtitle_selectable());
        assert_eq!(row.title(), "remote-error.html");
        let subtitle = row.subtitle().expect("activity subtitle");
        assert!(subtitle.contains("OneDrive/Test/remote-error.html"));
        assert!(!subtitle.contains(detail));
        assert!(subtitle.contains("&lt;!DOCTYPE HTML&gt;"));
        window.close();
    }

    #[test]
    fn activity_paths_are_confined_to_the_current_sync_root() {
        let root = Path::new("/home/u/OneDrive");
        assert_eq!(
            activity_target(root, "OneDrive/Notes/todo.md"),
            Some(root.join("Notes/todo.md"))
        );
        assert_eq!(activity_target(root, "OneDrive"), Some(root.to_path_buf()));
        assert_eq!(activity_target(root, "OneDrive/../outside"), None);
        assert_eq!(activity_target(root, "/etc/shadow"), None);
        assert_eq!(activity_target(root, "OneDrive-old/file"), None);
        assert_eq!(activity_target(Path::new(""), "OneDrive/file"), None);
        assert_eq!(
            activity_target(Path::new("relative"), "OneDrive/file"),
            None
        );
    }

    #[test]
    fn download_activity_names_its_source_without_a_display() {
        let synced = visual("downloaded", "");
        assert_eq!(synced.verb, "同步下载");
        assert!(!synced.consumes_detail);

        let on_demand = visual("downloaded", "按需");
        assert_eq!(on_demand.verb, "按需下载");
        assert!(on_demand.consumes_detail);
    }
}
