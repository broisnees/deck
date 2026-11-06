//! Site icons for browser tabs, the address bar and the new tab page.
//!
//! The page says where its icon is; deck fetches it natively (no CORS, so a
//! CDN-hosted icon works) and converts whatever it is (ICO with several
//! sizes, PNG, JPEG, WebP) to one PNG through ImageIO. SVG goes to gpui as
//! it is. Icons are kept per host on disk, so the new tab page has them for
//! sites that are not open.

use gpui::{Image, ImageFormat};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSData, NSDictionary};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

/// `None` once a host is known to have no icon on disk, so it is not read
/// again on every frame.
static CACHE: LazyLock<Mutex<HashMap<String, Option<Arc<Image>>>>> =
    LazyLock::new(Default::default);

/// Hosts whose icon was fetched this session.
static REFRESHED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// True the first time a host comes up this session, so its icon is fetched
/// once more in case the site changed it; after that the kept one serves.
pub fn stale(host: &str) -> bool {
    REFRESHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.to_string())
}

fn dir() -> PathBuf {
    crate::control::socket_dir().join("favicons")
}

fn file(host: &str, ext: &str) -> PathBuf {
    let safe: String = host
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir().join(format!("{safe}.{ext}"))
}

/// The icon for `host`, from memory or disk.
pub fn get(host: &str) -> Option<Arc<Image>> {
    if host.is_empty() {
        return None;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(host) {
        return hit.clone();
    }
    let found = [("png", ImageFormat::Png), ("svg", ImageFormat::Svg)]
        .into_iter()
        .find_map(|(ext, format)| {
            std::fs::read(file(host, ext))
                .ok()
                .map(|b| Arc::new(Image::from_bytes(format, b)))
        });
    cache.insert(host.to_string(), found.clone());
    found
}

/// Fetch `icon_url` for `host`, keep it, and hand it back.
pub async fn fetch(host: String, icon_url: String) -> Option<Arc<Image>> {
    let bytes = crate::net::get(&icon_url, 8.).await?;
    let (data, format, ext) = decode(&bytes)?;
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(file(&host, ext), &data);
    let image = Arc::new(Image::from_bytes(format, data));
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host, Some(image.clone()));
    Some(image)
}

fn decode(bytes: &[u8]) -> Option<(Vec<u8>, ImageFormat, &'static str)> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_lowercase();
    if head.contains("<svg") || head.starts_with("<?xml") {
        return Some((bytes.to_vec(), ImageFormat::Svg, "svg"));
    }
    raster(bytes)
}

#[cfg(target_os = "macos")]
fn raster(bytes: &[u8]) -> Option<(Vec<u8>, ImageFormat, &'static str)> {
    let reps = NSBitmapImageRep::imageRepsWithData(&NSData::with_bytes(bytes));
    // The sharpest size up to 256 px; past that only cost.
    let best = reps
        .iter()
        .filter_map(|r| r.downcast::<NSBitmapImageRep>().ok())
        .max_by_key(|r| {
            let w = r.pixelsWide();
            if w <= 256 {
                w
            } else {
                256 - w
            }
        })?;
    let png = unsafe {
        best.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some((png.to_vec(), ImageFormat::Png, "png"))
}

/// The image crate reads PNG, ICO, JPEG, GIF and WebP; an ICO comes out
/// as its largest picture.
#[cfg(not(target_os = "macos"))]
fn raster(bytes: &[u8]) -> Option<(Vec<u8>, ImageFormat, &'static str)> {
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > 256 { image.thumbnail(256, 256) } else { image };
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some((png.into_inner(), ImageFormat::Png, "png"))
}
