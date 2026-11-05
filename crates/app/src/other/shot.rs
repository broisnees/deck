//! `deck window shot` off macOS: not wired up yet.

use gpui::{Bounds, Pixels, Window};
use std::path::Path;

pub fn capture(_window: &Window, _out: &Path) -> Result<(usize, usize), String> {
    Err("window pictures are not supported on this platform yet".into())
}

pub fn capture_area(
    _window: &Window,
    _area: Bounds<Pixels>,
    _out: &Path,
) -> Result<(usize, usize), String> {
    Err("window pictures are not supported on this platform yet".into())
}
