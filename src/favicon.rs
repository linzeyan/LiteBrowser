//! Site icons: decode the PNG that WebView2 hands us, cache it per host in memory and on disk,
//! and expose it as a `slint::Image` for the tab strip, bookmarks and history.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use resvg::tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::platform::MenuIcon;
use crate::url_input;

pub struct FaviconCache {
    dir: PathBuf,
    by_host: HashMap<String, Image>,
    /// Hosts we already tried and failed, so we don't retry decoding every frame.
    missing: HashMap<String, ()>,
}

impl FaviconCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, by_host: HashMap::new(), missing: HashMap::new() }
    }

    fn file(&self, host: &str) -> PathBuf {
        // Hashed so odd host characters never break the path.
        let mut hash: u64 = 1469598103934665603;
        for b in host.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
        self.dir.join(format!("{hash:016x}.png"))
    }

    /// The icon for a URL's host, loading from disk on first use. `None` until one is known.
    pub fn get(&mut self, url: &str) -> Option<Image> {
        let host = url_input::host_of(url)?;
        if let Some(img) = self.by_host.get(&host) {
            return Some(img.clone());
        }
        if self.missing.contains_key(&host) {
            return None;
        }
        match std::fs::read(self.file(&host)).ok().and_then(|bytes| decode_png(&bytes)) {
            Some(img) => {
                self.by_host.insert(host, img.clone());
                Some(img)
            }
            None => {
                self.missing.insert(host, ());
                None
            }
        }
    }

    /// Stores a freshly fetched PNG for `url`'s host. Returns the decoded image on success.
    pub fn store(&mut self, url: &str, png_bytes: &[u8]) -> Option<Image> {
        let host = url_input::host_of(url)?;
        let img = decode_png(png_bytes)?;
        let _ = crate::paths::write_atomic(&self.file(&host), png_bytes);
        self.missing.remove(&host);
        self.by_host.insert(host, img.clone());
        Some(img)
    }

    /// Takes icons imported from another browser, best size first: the first one per host that
    /// decodes wins. Only for `hosts`, and never over an icon we already have, which is fresher.
    pub fn import(&mut self, icons: &[(String, Vec<u8>)], hosts: &HashSet<String>) -> usize {
        let mut added = 0;
        for (url, bytes) in icons {
            let Some(host) = url_input::host_of(url) else { continue };
            if hosts.contains(&host) && self.get(url).is_none() && self.store(url, bytes).is_some() {
                added += 1;
            }
        }
        added
    }
}

/// Decodes PNG bytes into a Slint RGBA image. Handles RGBA, RGB and grayscale 8-bit PNGs.
pub fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(bytes);
    // Expand palettes and low-bit grayscale to 8-bit channels, and 16-bit down to 8-bit, so the
    // output is always 8-bit RGB(A) or gray(+alpha).
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let src = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => src.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None, // next_frame expands palettes only with a transform
    };
    if rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
    Some(Image::from_rgba8(buffer))
}

/// A site icon redrawn `px` square for a native menu.
pub fn menu_icon(image: &Image, px: u32) -> Option<MenuIcon> {
    let src = image.to_rgba8_premultiplied()?;
    let (w, h) = (src.width(), src.height());
    let src = Pixmap::from_vec(src.as_bytes().to_vec(), IntSize::from_wh(w, h)?)?;
    let mut out = Pixmap::new(px, px)?;
    let scale = px as f32 / w.max(h) as f32;
    let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..PixmapPaint::default() };
    out.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_scale(scale, scale), None);
    Some(MenuIcon { size: px, rgba: out.take() })
}

/// One of the UI's outline glyphs (a path in a 24×24 box, like `Icons` in app.slint) drawn `px`
/// square for a native menu, with the bookmarks bar's stroke: 1.4 px at 16 px.
pub fn menu_glyph(path: &str, px: u32) -> Option<MenuIcon> {
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{px}" height="{px}" viewBox="0 0 24 24"><path d="{path}" fill="none" stroke="#474747" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"/></svg>"##
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = Pixmap::new(px, px)?;
    resvg::render(&tree, Transform::default(), &mut pixmap.as_mut());
    Some(MenuIcon { size: px, rgba: pixmap.take() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, size, size);
        encoder.set_color(png::ColorType::Rgba);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&vec![255; (size * size * 4) as usize]).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn import_takes_the_best_drawable_icon_per_bookmarked_host() {
        let dir = std::env::temp_dir().join(format!("litebrowser-favicon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cache = FaviconCache::new(dir.clone());
        cache.store("https://visited.example/", &png(8));
        let hosts: HashSet<String> = ["a.example", "visited.example"].map(String::from).into();
        let icons = vec![
            // Firefox keeps SVG icons too; we cannot draw them, so the next one must be used.
            ("https://a.example/page".to_string(), b"<svg/>".to_vec()),
            ("https://a.example/".to_string(), png(32)),
            ("https://a.example/other".to_string(), png(16)),
            ("https://visited.example/".to_string(), png(32)),
            ("https://not-bookmarked.example/".to_string(), png(32)),
        ];
        assert_eq!(cache.import(&icons, &hosts), 1);
        assert_eq!(cache.get("https://a.example/x").unwrap().size().width, 32);
        // An icon LiteBrowser fetched itself is newer than anything in another browser's cache.
        assert_eq!(cache.get("https://visited.example/").unwrap().size().width, 8);
        assert!(cache.get("https://not-bookmarked.example/").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn menu_icons_are_drawn_at_the_menu_size() {
        // The menu bitmap must be exactly px² (platform rejects anything else): a 16 px favicon
        // on a 175 % screen becomes 28 px, opaque where the icon is.
        let icon = menu_icon(&decode_png(&png(16)).unwrap(), 28).unwrap();
        assert_eq!((icon.size, icon.rgba.len()), (28, 28 * 28 * 4));
        assert_eq!(icon.rgba[(14 * 28 + 14) * 4 + 3], 255);
        // A glyph is actually inked, not left blank, and stays inside its stroke.
        let glyph = menu_glyph("M 3.5 12 L 20.5 12", 28).unwrap();
        assert_eq!(glyph.rgba.len(), 28 * 28 * 4);
        assert!(glyph.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
        assert_eq!(glyph.rgba[3], 0);
    }
}
