//! `deck window shot` off macOS. Windows draws Deck's window into a bitmap
//! with PrintWindow, which also takes in what DirectX and WebView2 draw.
//! Linux has no capture wired up yet.

use gpui::{Bounds, Pixels, Window};
use std::path::Path;

/// Write the window as a PNG at `out`. Returns its size in pixels.
#[cfg(windows)]
pub fn capture(window: &Window, out: &Path) -> Result<(usize, usize), String> {
    write_png(window_image(window)?, out)
}

/// Write just `area` of the window (gpui's coordinates: logical pixels from
/// the top left of its view) as a PNG at `out`: one pane, say.
#[cfg(windows)]
pub fn capture_area(
    window: &Window,
    area: Bounds<Pixels>,
    out: &Path,
) -> Result<(usize, usize), String> {
    let image = window_image(window)?;
    let scale = window.scale_factor();
    let px = |v: Pixels| (f32::from(v) * scale).round().max(0.) as u32;
    let (x, y) = (px(area.origin.x), px(area.origin.y));
    let (w, h) = (px(area.size.width), px(area.size.height));
    if w < 1 || h < 1 {
        return Err("the pane has no size on screen".into());
    }
    if x >= image.width() || y >= image.height() {
        return Err("the pane is outside the window's picture".into());
    }
    let (w, h) = (w.min(image.width() - x), h.min(image.height() - y));
    write_png(image::imageops::crop_imm(&image, x, y, w, h).to_image(), out)
}

/// The window's client area, which is all of gpui's view.
#[cfg(windows)]
fn window_image(window: &Window) -> Result<image::RgbaImage, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS, PW_CLIENTONLY};
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, PW_RENDERFULLCONTENT};

    let handle = HasWindowHandle::window_handle(window).map_err(|_| "deck's window is not on screen")?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return Err("deck's window is not on screen".into());
    };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    let mut rect = RECT::default();
    // SAFETY: gpui's own window, alive for this call.
    unsafe { GetClientRect(hwnd, &mut rect) }.map_err(|e| e.to_string())?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w < 1 || h < 1 {
        return Err("deck's window is not on screen".into());
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: a top-down 32 bpp section of w x h, read only while selected
    // into a DC we own; everything made here is freed before returning.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(e) => {
                let _ = DeleteDC(dc);
                return Err(e.to_string());
            }
        };
        let old = SelectObject(dc, bitmap.into());
        let ok = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT)).as_bool();
        let mut rgba = Vec::new();
        if ok {
            let len = (w * h * 4) as usize;
            rgba = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
            // BGRA to RGBA; GDI leaves alpha undefined.
            for p in rgba.chunks_exact_mut(4) {
                p.swap(0, 2);
                p[3] = 255;
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        if !ok {
            return Err("the window could not be captured".into());
        }
        image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or_else(|| "the window could not be captured".into())
    }
}

/// Write `image` as a PNG at `out`. Returns its size in pixels.
#[cfg(windows)]
fn write_png(image: image::RgbaImage, out: &Path) -> Result<(usize, usize), String> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let size = (image.width() as usize, image.height() as usize);
    image.save_with_format(out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(size)
}

#[cfg(not(windows))]
pub fn capture(_window: &Window, _out: &Path) -> Result<(usize, usize), String> {
    Err("window pictures are not supported on this platform yet".into())
}

#[cfg(not(windows))]
pub fn capture_area(
    _window: &Window,
    _area: Bounds<Pixels>,
    _out: &Path,
) -> Result<(usize, usize), String> {
    Err("window pictures are not supported on this platform yet".into())
}
