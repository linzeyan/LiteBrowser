//! Site icons: decode the PNG that WebView2 hands us, cache it per host in memory and on disk,
//! and expose it as a `slint::Image` for the tab strip, bookmarks and history.

use std::collections::HashMap;
use std::path::PathBuf;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

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
