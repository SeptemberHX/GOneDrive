//! GOnedrive's desktop front end.
//!
//! Deliberately a thin client: it owns no sync state, no credentials and no
//! database handle. Everything it shows comes from `gonedrived` over D-Bus, so
//! closing this window does not stop a transfer, and a crash here cannot
//! corrupt anything.

mod backend;
mod monitor;
mod tray;
mod trayicon;
mod window;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

use crate::backend::Command;
use crate::window::Ui;

const APP_ID: &str = "org.gonedrive.Gonedrive";
/// The progress window runs as its own application so it never activates — or
/// is activated by — the main window.
const MONITOR_APP_ID: &str = "org.gonedrive.Gonedrive.Monitor";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gonedrive_gtk=info".into()),
        )
        .init();

    // Spawned by the daemon while a placeholder is being filled.
    if std::env::args().any(|arg| arg == "--hydration-monitor") {
        let app = adw::Application::builder()
            .application_id(MONITOR_APP_ID)
            .build();
        app.connect_activate(build_monitor);
        // The flag is ours, not GTK's.
        return app.run_with_args::<&str>(&[]);
    }

    let mut builder = adw::Application::builder().application_id(APP_ID);
    // In snapshot mode, refuse to fold into an already-running instance: the
    // capture must render *this* process's window, not hand its request to a
    // stale one and exit.
    if std::env::var_os("GONEDRIVE_SNAPSHOT").is_some() {
        builder = builder.flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
    }
    let app = builder.build();
    app.connect_activate(build_ui);
    app.run_with_args::<&str>(&[])
}

fn build_monitor(app: &adw::Application) {
    // No tray: this window is transient and the main application owns the icon.
    let (commands, events, _) = backend::spawn(false);
    let monitor = monitor::Monitor::build(app, commands);
    let window = monitor.window.clone();

    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            monitor.apply(event);
        }
    });

    // Deliberately not presented here: the window appears when the first
    // transfer is reported, so a monitor started for a download that finished
    // in the meantime shows nothing at all.
    let _ = window;
}

fn build_ui(app: &adw::Application) {
    // Launching the app while it is already running means "bring it to the
    // front", the way every status-area application behaves — not "open a
    // second copy of the window". `windows()` rather than `active_window()`,
    // so a window kept hidden (a background/autostart launch) is still found
    // and presented instead of a second one being built.
    if let Some(window) = app.windows().into_iter().next() {
        window.present();
        return;
    }

    let (commands, events, actions) = backend::spawn(true);
    let ui = Ui::build(app, commands.clone());
    let window = ui.window.clone();
    let account = ui.current_account();
    let ready = ui.is_ready();

    register_actions(app, &window, &commands, account);

    // The receiving end runs on the GLib main context, which is what makes it
    // safe to touch widgets here: every `apply` call is on the main thread.
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            ui.apply(event);
        }
    });

    // Refresh on focus: the daemon may have been syncing while the window was
    // hidden, and its signals are not replayed.
    let refresh = commands.clone();
    window.connect_is_active_notify(move |window| {
        if window.is_active() {
            let _ = refresh.send_blocking(Command::Refresh);
        }
    });

    // Closing hides rather than quits, the way every sync client behaves: the
    // point of the tray icon is that the thing keeps working with no window.
    let hold = app.hold();
    window.connect_close_request(move |window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    // Tray requests arrive on the GTK thread here, which is the only place
    // allowed to touch the window.
    let tray_window = window.clone();
    let tray_commands = commands.clone();
    let tray_app = app.clone();
    glib::spawn_future_local(async move {
        let _hold = hold;
        while let Ok(action) = actions.recv().await {
            match action {
                tray::TrayAction::ShowPanel => {
                    tray_window.set_visible(true);
                    tray_window.present();
                    let _ = tray_commands.send_blocking(Command::Refresh);
                }
                tray::TrayAction::OpenFolder => open_sync_folder(&tray_window),
                tray::TrayAction::Quit => {
                    tray_app.quit();
                    return;
                }
            }
        }
    });

    // Start hidden when launched in the background (autostart): the tray is up,
    // but the window stays out of the way until the user opens it. Any other
    // launch presents it. A snapshot capture must always present, or it renders
    // nothing.
    let background = std::env::args().any(|arg| arg == "--background")
        && std::env::var_os("GONEDRIVE_SNAPSHOT").is_none();
    if background {
        window.set_visible(false);
    } else {
        window.present();
    }

    // Development hook: render the window into a PNG and exit. The capture
    // comes from GTK's own renderer, so it shows exactly what the user would
    // see — regardless of compositor, occlusion, or backing-store staleness,
    // all of which make external X11 screenshots of a GL-rendered window lie.
    if let Ok(path) = std::env::var("GONEDRIVE_SNAPSHOT") {
        let target = window.clone();
        let ready = ready.clone();
        // Poll for the first daemon reply, then wait one more frame so the
        // freshly-shown page has laid out; capture only then. Deadline-bounded
        // so a disconnected daemon still produces the offline page rather than
        // hanging the capture forever.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            if !ready.get() && std::time::Instant::now() < deadline {
                return glib::ControlFlow::Continue;
            }
            let target = target.clone();
            let path = path.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                match snapshot_window(&target, &path) {
                    Ok(()) => eprintln!("snapshot written to {path}"),
                    Err(e) => eprintln!("snapshot failed: {e}"),
                }
                std::process::exit(0);
            });
            glib::ControlFlow::Break
        });
    }
}

/// Render a window's current contents to a PNG file.
fn snapshot_window(window: &adw::ApplicationWindow, path: &str) -> Result<(), String> {
    use gtk::prelude::PaintableExt;

    // Aim at the toolbar view — the whole panel including its header — rather
    // than the inner GtkStack. The stack's scrolled pages report a fixed
    // viewport height, not their content's, so capturing the stack clipped
    // the folders and overview pages to a sliver; the toolbar view lays out
    // to its natural size.
    // Try the whole panel first (natural height), then the page stack, then
    // the window itself — the first that yields a non-empty render node wins.
    // A scrolled page occasionally renders nothing through a child paintable,
    // so the window fallback is what guarantees a capture always succeeds.
    let mut candidates: Vec<gtk::Widget> = Vec::new();
    if let Some(w) = find_role(window.upcast_ref(), "AdwToolbarView") {
        candidates.push(w);
    }
    candidates.push(window.clone().upcast());
    if let Some(w) = find_stack(window.upcast_ref()) {
        candidates.push(w);
    }

    let mut rendered = None;
    for subject in candidates {
        let (w, h) = (subject.width(), subject.height());
        if w == 0 || h == 0 {
            continue;
        }
        let paintable = gtk::WidgetPaintable::new(Some(&subject));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, w as f64, h as f64);
        if let Some(node) = snapshot.to_node() {
            rendered = Some((node, w, h));
            break;
        }
    }
    let (node, width, height) = rendered.ok_or_else(|| "nothing was rendered".to_string())?;

    let renderer = window
        .renderer()
        .ok_or_else(|| "the window has no renderer".to_string())?;
    // An explicit viewport rather than None: the node's own bounds can be
    // empty for a scrolled page whose content has not been measured, and a
    // None viewport then renders nothing. The subject's allocation is always
    // the size we want captured.
    let viewport = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(node, Some(&viewport));
    texture.save_to_png(path).map_err(|e| e.to_string())
}

/// First widget of the given type name, depth-first.
fn find_role(widget: &gtk::Widget, type_name: &str) -> Option<gtk::Widget> {
    if widget.type_().name() == type_name {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_role(&c, type_name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// First GtkStack in the tree — the page container the UI switches.
fn find_stack(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.type_().name() == "GtkStack" {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_stack(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// Open the sync folder in the user's file manager.
fn open_sync_folder(parent: &adw::ApplicationWindow) {
    let home = glib::home_dir().join("OneDrive");
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&home)));
    launcher.launch(Some(parent), None::<&gio::Cancellable>, |_| {});
}

fn register_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    commands: &async_channel::Sender<Command>,
    account: std::rc::Rc<std::cell::RefCell<Option<String>>>,
) {
    let about = gio::SimpleAction::new("about", None);
    let parent = window.clone();
    about.connect_activate(move |_, _| {
        adw::AboutDialog::builder()
            .application_name("GOnedrive")
            .application_icon("org.gonedrive.Gonedrive")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("GOnedrive")
            .comments("Linux 上的 OneDrive 客户端：双向同步，按需下载。")
            .license_type(gtk::License::Gpl30)
            .build()
            .present(Some(&parent));
    });
    app.add_action(&about);

    let open_folder = gio::SimpleAction::new("open-folder", None);
    let parent = window.clone();
    open_folder.connect_activate(move |_, _| open_sync_folder(&parent));
    app.add_action(&open_folder);

    let sign_out = gio::SimpleAction::new("sign-out", None);
    let commands = commands.clone();
    let parent = window.clone();
    sign_out.connect_activate(move |_, _| {
        // Signing out drops credentials, so it gets a confirmation; the local
        // files survive either way and the dialog says so.
        let dialog = adw::AlertDialog::builder()
            .heading("退出登录？")
            .body("将从密钥环中移除凭据并停止同步。本地文件会保留在磁盘上，不会被删除。")
            .build();
        dialog.add_response("cancel", "取消");
        dialog.add_response("sign-out", "退出登录");
        dialog.set_response_appearance("sign-out", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));

        let commands = commands.clone();
        let account = account.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "sign-out" {
                return;
            }
            // Only ever act on an id we actually hold. Sending an empty one
            // would ask the daemon to guess, and guessing becomes "signed out
            // of the wrong account" the moment multi-account support lands.
            if let Some(id) = account.borrow().clone() {
                let _ = commands.send_blocking(Command::RemoveAccount(id));
            }
        });
        dialog.present(Some(&parent));
    });
    app.add_action(&sign_out);
}
