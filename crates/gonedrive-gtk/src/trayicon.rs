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
  <defs><linearGradient id="od" x1="0" y1="0" x2="0" y2="1">
    <stop offset="0" stop-color="#35B8EC"/><stop offset="1" stop-color="#0264C0"/>
  </linearGradient></defs>
  <g fill="url(#od)">
    <ellipse cx="24" cy="31.5" rx="16" ry="9.5"/>
    <circle cx="14.5" cy="27" r="7.8"/>
    <circle cx="24" cy="20.5" r="10"/>
    <circle cx="33.5" cy="26" r="8.4"/>
  </g>"##;

/// A badge is a white-ringed coloured disc at the bottom-right with a white
/// glyph. `glyph` is drawn centred on (36.5, 36.5).
fn badge(colour: &str, glyph: &str) -> String {
    format!(
        r##"<g><circle cx="36.5" cy="36.5" r="11" fill="#ffffff"/>
           <circle cx="36.5" cy="36.5" r="9.2" fill="{colour}"/>{glyph}</g>"##
    )
}

/// The badge overlay for a sync-state category, empty for the idle cloud.
fn overlay(category: &str) -> String {
    match category {
        "sync" => badge(
            "#0F7BDB",
            r##"<path d="M31.7 36.5a4.8 4.8 0 1 1 1.5 3.5" fill="none" stroke="#fff"
                 stroke-width="2.1" stroke-linecap="round"/>
               <path d="M30.6 32.8l1.3 3.9 3.8-1.3z" fill="#fff"/>"##,
        ),
        "paused" => badge(
            "#2C6F9B",
            r##"<rect x="32.7" y="31.8" width="2.7" height="9.4" rx="1.2" fill="#fff"/>
               <rect x="37.6" y="31.8" width="2.7" height="9.4" rx="1.2" fill="#fff"/>"##,
        ),
        "offline" => badge(
            "#9AA0A6",
            r##"<line x1="31.6" y1="41.4" x2="41.4" y2="31.6" stroke="#fff"
                 stroke-width="2.6" stroke-linecap="round"/>"##,
        ),
        "error" => badge(
            "#E8590C",
            r##"<rect x="35.2" y="30.7" width="2.6" height="7.2" rx="1.3" fill="#fff"/>
               <circle cx="36.5" cy="41" r="1.7" fill="#fff"/>"##,
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

/// The sync-state categories the tray distinguishes.
pub const CATEGORIES: &[&str] = &["idle", "sync", "paused", "offline", "error"];

/// Pre-render every category at a couple of panel sizes. Empty if the SVG
/// loader is unavailable, which tells the tray to fall back to a themed name.
pub fn render_all() -> std::collections::HashMap<&'static str, Vec<ksni::Icon>> {
    let mut map = std::collections::HashMap::new();
    for &category in CATEGORIES {
        let icons: Vec<ksni::Icon> = [22, 32, 44]
            .into_iter()
            .filter_map(|size| render(category, size))
            .collect();
        if !icons.is_empty() {
            map.insert(category, icons);
        }
    }
    map
}
