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
const CLOUD: &str = r##"
  <g fill="#28A8EA">
    <ellipse cx="31" cy="32" rx="17" ry="12"/>
    <circle cx="37" cy="24" r="11"/>
    <circle cx="24" cy="18.5" r="12.5"/>
  </g>
  <g fill="#0A78CE">
    <ellipse cx="15" cy="35" rx="14.5" ry="10"/>
    <circle cx="9" cy="30" r="9"/>
    <circle cx="19" cy="27" r="10"/>
  </g>"##;

/// A badge is a white-ringed coloured disc at the bottom-right with a white
/// glyph. `glyph` is drawn centred on (37, 37).
fn badge(colour: &str, glyph: &str) -> String {
    format!(
        r##"<g><circle cx="37" cy="37" r="11.5" fill="#ffffff"/>
           <circle cx="37" cy="37" r="9.6" fill="{colour}"/>{glyph}</g>"##
    )
}

/// The badge overlay for a sync-state category, empty for the idle cloud.
fn overlay(category: &str) -> String {
    match category {
        "sync" => badge(
            "#0F7BDB",
            r##"<path d="M32.2 37a4.8 4.8 0 1 1 1.5 3.5" fill="none" stroke="#fff"
                 stroke-width="2.2" stroke-linecap="round"/>
               <path d="M31.1 33.3l1.3 3.9 3.8-1.3z" fill="#fff"/>"##,
        ),
        "paused" => badge(
            "#2C6F9B",
            r##"<rect x="33.2" y="32.3" width="2.8" height="9.6" rx="1.2" fill="#fff"/>
               <rect x="38.1" y="32.3" width="2.8" height="9.6" rx="1.2" fill="#fff"/>"##,
        ),
        "offline" => badge(
            "#9AA0A6",
            r##"<line x1="32.1" y1="41.9" x2="41.9" y2="32.1" stroke="#fff"
                 stroke-width="2.7" stroke-linecap="round"/>"##,
        ),
        "error" => badge(
            "#E8590C",
            r##"<rect x="35.7" y="31.2" width="2.6" height="7.4" rx="1.3" fill="#fff"/>
               <circle cx="37" cy="41.5" r="1.7" fill="#fff"/>"##,
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
