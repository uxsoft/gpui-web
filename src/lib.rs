//! A Chromium web view for GPUI, built on CEF.
//!
//! GPUI never touches the page's pixels on Wayland and Windows: the page
//! lives in a native layer under GPUI's window, and GPUI leaves a hole in
//! its background where the page shows through ([`HoleBackground`]), so
//! GPUI's menus and popovers still draw over it. On macOS the page is a
//! GPU surface GPUI composites; on X11 it is a CPU image.
//!
//! Setup:
//! 1. First thing in `main`: `if let Some(code) = gpui_web::handle_subprocess(true) { exit(code) }`
//!    (CEF runs its renderer and GPU processes from the same executable).
//! 2. At app start: [`init`], and [`window_background`] for the window.
//! 3. [`HoleBackground`] as the window root's background.
//! 4. [`WebView::new`] for each page; [`shutdown`] when the app quits.
//!
//! See `docs/webview-spikes.md` for what each platform path relies on.

mod browser;
mod devtools;
mod element;
mod input;
mod platform;
mod runtime;
mod webview;

pub use browser::{ContextMenu, DialogKind, Disposition, FileChooser, JsDialog};
pub use devtools::{devtools_url, take_devtools_pipe};
pub use element::HoleBackground;
pub use runtime::{
    LogLevel, Profile, RemoteDebugging, RuntimeOptions, handle_subprocess, init,
    reserve_devtools_pipe, shutdown, window_background,
};
pub use webview::{WebView, WebViewEvent};

/// The Chromium version pages run in, e.g. `154.0.8037.98`.
pub fn chromium_version() -> &'static str {
    let full = std::str::from_utf8(cef::sys::CEF_VERSION)
        .unwrap_or_default()
        .trim_end_matches('\0');
    full.rsplit_once("chromium-").map_or(full, |(_, v)| v)
}
