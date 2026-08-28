//! The OneDrive tray icon: a blue cloud with a per-state badge.
//!
//! Rendered to an ARGB pixmap at run time rather than referenced by theme name,
//! so the brand icon appears without anything being installed into the icon
//! theme first — and so each sync state can carry its own corner badge (a sync
//! arrow, a pause, an offline slash, an error mark) composited over the one
//! cloud. If the SVG loader is unavailable the caller falls back to a themed
//! symbolic name, so a missing librsvg degrades to a plain icon, never nothing.

use gtk::gdk_pixbuf::Pixbuf;
use gtk::gio;
use gtk::glib;

/// The cloud itself — a base ellipse and three bumps in one fill so the union
/// is seamless, under a top-lit OneDrive-blue gradient.
// The official OneDrive four-tone cloud, mapped from its own wide viewBox into
// a centred 48×48 square so it fills the icon.
const CLOUD: &str = r##"<g transform="translate(-1.54,7.90) scale(1.5) translate(1.132,-4.727)">
  <path d="M12.202 11.193v-.001l6.718 4.024 4.003-1.685A6.477 6.477 0 0 1 25.5 13c.148 0 .294.007.439.016a10 10 0 0 0-18.041-3.013L8 10a7.96 7.96 0 0 1 4.202 1.193z" fill="#0364b8"/>
  <path d="M12.203 11.192A7.96 7.96 0 0 0 8 10l-.102.003a7.997 7.997 0 0 0-6.46 12.57L7.36 20.08l2.634-1.108 5.863-2.468 3.062-1.288z" fill="#0078d4"/>
  <path d="M25.939 13.016A6.577 6.577 0 0 0 25.5 13a6.477 6.477 0 0 0-2.576.532l-4.004 1.684 1.161.695 3.805 2.279 1.66.994 5.677 3.4a6.5 6.5 0 0 0-5.284-9.568z" fill="#1490df"/>
  <path d="M25.546 19.184l-1.66-.994-3.805-2.28-1.16-.694-3.063 1.288-5.863 2.468L7.36 20.08l-5.924 2.493A7.989 7.989 0 0 0 8 26h17.5a6.498 6.498 0 0 0 5.723-3.416z" fill="#28a8ea"/>
</g>"##;

/// A badge is a white-ringed coloured disc at the bottom-right with a white
/// glyph. `glyph` is drawn centred on (38, 38).
fn badge(colour: &str, glyph: &str) -> String {
    format!(
        r##"<g><circle cx="38" cy="38" r="11" fill="#ffffff"/>
           <circle cx="38" cy="38" r="9.2" fill="{colour}"/>{glyph}</g>"##
    )
}

/// The badge overlay for a sync-state category, empty for the idle cloud.
fn overlay(category: &str) -> String {
    match category {
        "sync" => badge(
            "#0F7BDB",
            r##"<path d="M33.4 38a4.6 4.6 0 1 1 1.4 3.4" fill="none" stroke="#fff"
                 stroke-width="2.1" stroke-linecap="round"/>
               <path d="M32.3 34.5l1.2 3.7 3.6-1.2z" fill="#fff"/>"##,
        ),
        "paused" => badge(
            "#2C6F9B",
            r##"<rect x="34.4" y="33.6" width="2.6" height="8.8" rx="1.1" fill="#fff"/>
               <rect x="39" y="33.6" width="2.6" height="8.8" rx="1.1" fill="#fff"/>"##,
        ),
        "offline" => badge(
            "#9AA0A6",
            r##"<line x1="33.4" y1="42.6" x2="42.6" y2="33.4" stroke="#fff"
                 stroke-width="2.5" stroke-linecap="round"/>"##,
        ),
        "error" => badge(
            "#E8590C",
            r##"<rect x="36.8" y="32.5" width="2.4" height="6.8" rx="1.2" fill="#fff"/>
               <circle cx="38" cy="42" r="1.6" fill="#fff"/>"##,
        ),
        _ => String::new(),
    }
}

/// The full SVG for a category.
fn svg(category: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48">{CLOUD}{}</svg>"##,
        overlay(category)
    )
}

/// Render one category to a `ksni` icon at `size` px, or `None` if the SVG
/// loader is not available.
fn render(category: &str, size: i32) -> Option<ksni::Icon> {
    let svg = svg(category);
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(svg.as_bytes()));
    let pixbuf =
        Pixbuf::from_stream_at_scale(&stream, size, size, true, gio::Cancellable::NONE).ok()?;
    let (w, h) = (pixbuf.width(), pixbuf.height());
    let channels = pixbuf.n_channels() as usize;
    let stride = pixbuf.rowstride() as usize;
    let pixels = pixbuf.read_pixel_bytes();
    let src = pixels.as_ref();

    // gdk-pixbuf gives RGBA rows (possibly padded); ksni wants ARGB32 in network
    // byte order, i.e. [A, R, G, B] per pixel, tightly packed.
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = y * stride;
        for x in 0..w as usize {
            let i = row + x * channels;
            let (r, g, b) = (src[i], src[i + 1], src[i + 2]);
            let a = if channels == 4 { src[i + 3] } else { 255 };
            data.extend_from_slice(&[a, r, g, b]);
        }
    }
    Some(ksni::Icon { width: w, height: h, data })
}

/// The plain OneDrive cloud as a texture, for the login page and the avatar
/// badge. `None` if the SVG loader is unavailable.
pub fn cloud_texture(size: i32) -> Option<gtk::gdk::Texture> {
    let svg = svg("idle");
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(svg.as_bytes()));
    let pixbuf =
        Pixbuf::from_stream_at_scale(&stream, size, size, true, gio::Cancellable::NONE).ok()?;
    Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
}

/// The sync-state categories the tray distinguishes.
pub const CATEGORIES: &[&str] = &["idle", "sync", "paused", "offline", "error"];

/// Pre-render every category at a couple of panel sizes. Empty if the SVG
/// loader is unavailable, which tells the tray to fall back to a themed name.
pub fn render_all() -> std::collections::HashMap<&'static str, Vec<ksni::Icon>> {
    let mut map = std::collections::HashMap::new();
    for &category in CATEGORIES {
        // A spread of sizes, up to 64 px, so a HiDPI panel picks a crisp large
        // icon rather than scaling up a small one.
        let icons: Vec<ksni::Icon> = [24, 32, 48, 64]
            .into_iter()
            .filter_map(|size| render(category, size))
            .collect();
        if !icons.is_empty() {
            map.insert(category, icons);
        }
    }
    map
}
