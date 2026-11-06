//! What ⌘V means in the composer when the clipboard holds more than text:
//! files copied in Finder, or an image (a screenshot, "Copy Image" in a
//! browser). gpui's own clipboard reads text first and never sees file URLs,
//! so this asks NSPasteboard directly.

#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeFileURL,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDictionary, NSString};
use std::path::PathBuf;

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub enum Pasted {
    Files(Vec<PathBuf>),
    /// Encoded image bytes and the extension they should be saved with.
    Image(Vec<u8>, &'static str),
}

/// Files or an image on the general pasteboard, or `None` when ⌘V should
/// paste text. Text wins over an image unless it is a single word (a URL or
/// a file name riding along with the picture), so copying a table out of
/// Numbers, which also carries a picture of it, still pastes the text.
pub fn read() -> Option<Pasted> {
    read_with(|t| t.trim().contains(char::is_whitespace))
}

/// The terminal's ⌘V: any text wins, so a single word copied with a picture
/// of it still pastes as the word, except a lone link riding along with an
/// image ("Copy Image" in a browser).
pub fn read_for_terminal() -> Option<Pasted> {
    read_with(|t| {
        let t = t.trim();
        !t.is_empty() && !t.starts_with("http://") && !t.starts_with("https://")
    })
}

/// On Linux only text is read yet; gpui's clipboard pastes it.
#[cfg(not(any(target_os = "macos", windows)))]
fn read_with(_text_wins: impl Fn(&str) -> bool) -> Option<Pasted> {
    None
}

/// The Windows clipboard, as the Mac's: files copied in Explorer
/// (CF_HDROP), then text when it wins, then an image: "PNG" as browsers and
/// the Snipping Tool put it, or the bitmap (CF_DIB) made a PNG.
#[cfg(windows)]
fn read_with(text_wins: impl Fn(&str) -> bool) -> Option<Pasted> {
    use windows::core::w;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// A global memory block's bytes.
    unsafe fn bytes(h: HANDLE) -> Option<Vec<u8>> {
        let g = HGLOBAL(h.0);
        let p = unsafe { GlobalLock(g) } as *const u8;
        if p.is_null() {
            return None;
        }
        let out = unsafe { std::slice::from_raw_parts(p, GlobalSize(g)) }.to_vec();
        let _ = unsafe { GlobalUnlock(g) };
        Some(out)
    }

    // Another app may hold the clipboard for a moment.
    // SAFETY: opened here and closed before returning.
    let opened = (0..5).any(|i| {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { OpenClipboard(None) }.is_ok()
    });
    if !opened {
        return None;
    }
    // SAFETY: the clipboard is open; every handle is the clipboard's, read
    // under a lock and never kept.
    let out = (|| unsafe {
        if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            let files: Vec<PathBuf> = (0..n)
                .filter_map(|i| {
                    let len = DragQueryFileW(drop, i, None) as usize;
                    let mut buf = vec![0u16; len + 1];
                    let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                    (got > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..got])))
                })
                .collect();
            if !files.is_empty() {
                return Some(Pasted::Files(files));
            }
        }
        let text = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| bytes(h)).map(|b| {
            let wide: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
            String::from_utf16_lossy(&wide)
        });
        if text.as_deref().is_some_and(&text_wins) {
            return None;
        }
        let png = RegisterClipboardFormatW(w!("PNG"));
        if png != 0 {
            if let Some(b) = GetClipboardData(png).ok().and_then(|h| bytes(h)).filter(|b| !b.is_empty()) {
                return Some(Pasted::Image(b, "png"));
            }
        }
        let dib = GetClipboardData(CF_DIB.0 as u32).ok().and_then(|h| bytes(h))?;
        Some(Pasted::Image(dib_to_png(&dib)?, "png"))
    })();
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    out
}

/// A clipboard bitmap (a BITMAPINFO and its pixels) as PNG: the file
/// header a .bmp would start with, then the image crate's decoder.
#[cfg(windows)]
fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(dib.get(o..o + 4)?.try_into().ok()?)) };
    let header = u32_at(0)?;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?) as u32;
    let compression = u32_at(16)?;
    let used = u32_at(32)?;
    // BI_BITFIELDS after a plain BITMAPINFOHEADER: three masks follow it.
    let masks = if header == 40 && compression == 3 { 12 } else { 0 };
    let palette = if used > 0 { used } else if bits <= 8 { 1 << bits } else { 0 };
    let offset = 14 + header + masks + palette * 4;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&offset.to_le_bytes());
    bmp.extend_from_slice(dib);
    let image = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

#[cfg(target_os = "macos")]
fn read_with(text_wins: impl Fn(&str) -> bool) -> Option<Pasted> {
    let pb = NSPasteboard::generalPasteboard();
    let files: Vec<PathBuf> = pb
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
                .filter_map(|url| file_url_to_path(&url.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !files.is_empty() {
        return Some(Pasted::Files(files));
    }
    let text = pb
        .stringForType(unsafe { NSPasteboardTypeString })
        .map(|s| s.to_string());
    if text.as_deref().is_some_and(text_wins) {
        return None;
    }
    // Formats the model accepts as they are; anything else (TIFF from a
    // screenshot or Preview) becomes PNG.
    for (uti, ext) in [
        ("public.png", "png"),
        ("public.jpeg", "jpg"),
        ("com.compuserve.gif", "gif"),
        ("org.webmproject.webp", "webp"),
    ] {
        if let Some(data) = pb.dataForType(&NSString::from_str(uti)) {
            if data.length() > 0 {
                return Some(Pasted::Image(data.to_vec(), ext));
            }
        }
    }
    let tiff = pb.dataForType(unsafe { NSPasteboardTypeTIFF })?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(Pasted::Image(png.to_vec(), "png"))
}

/// Longest edge a pasted image keeps. The model scales anything larger down
/// to about this anyway, so the extra pixels would only cost upload time.
const PASTE_MAX_EDGE: u32 = 1568;

/// Keep a pasted image as a file, so every agent can be handed a path.
/// Big ones are scaled down with `sips`; if that fails the original stays.
pub fn save_image(bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
    let dir = crate::threads::attachments_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("pasted-{stamp}.{ext}"));
    std::fs::write(&path, bytes)?;
    if cfg!(target_os = "macos") && bytes.len() > 512 * 1024 && ext != "gif" {
        let edge = PASTE_MAX_EDGE.to_string();
        let _ = std::process::Command::new("/usr/bin/sips")
            .args(["-Z", &edge])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(path)
}

/// Paths as a terminal types them when files are dropped on it: each one
/// backslash-escaped, joined by spaces. Agent CLIs (Claude Code, Codex) turn
/// a pasted image path into an attachment.
pub fn shell_paths(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in p.to_string_lossy().chars() {
            if !(c.is_alphanumeric() || "/._-+,:@%~".contains(c)) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// `file:///Users/me/My%20Shot.png` → `/Users/me/My Shot.png`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    // Finder hands folders over with a trailing slash.
    let s = if s.len() > 1 {
        s.trim_end_matches('/')
    } else {
        &s
    };
    Some(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_decode_to_paths() {
        assert_eq!(
            file_url_to_path("file:///Users/me/My%20Shot.png"),
            Some("/Users/me/My Shot.png".into())
        );
        assert_eq!(
            file_url_to_path("file:///tmp/dir/"),
            Some("/tmp/dir".into())
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/a%C3%A9.txt"),
            Some("/tmp/aé.txt".into())
        );
        assert_eq!(
            file_url_to_path("file:///tmp/100%"),
            Some("/tmp/100%".into())
        );
        assert_eq!(file_url_to_path("https://example.com"), None);
    }

    #[test]
    fn shell_paths_escape_like_a_drop() {
        assert_eq!(
            shell_paths(&["/Users/me/My Shot (2).png".into(), "/tmp/aé.txt".into()]),
            r"/Users/me/My\ Shot\ \(2\).png /tmp/aé.txt"
        );
        assert_eq!(shell_paths(&["/tmp/it's".into()]), r"/tmp/it\'s");
    }
}
