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
//!
//! Without the `cef` feature (on by default) the crate builds without
//! downloading Chromium: the API stays, and every page reports
//! [`WebView::error`].

#[cfg(feature = "cef")]
mod browser;
mod devtools;
#[cfg(feature = "cef")]
mod element;
#[cfg(feature = "cef")]
mod input;
#[cfg(feature = "cef")]
mod platform;
#[cfg(feature = "cef")]
mod runtime;
#[cfg(not(feature = "cef"))]
mod stub;
mod types;
#[cfg(feature = "cef")]
mod webview;

pub use devtools::{devtools_url, take_devtools_pipe};
#[cfg(feature = "cef")]
pub use element::HoleBackground;
#[cfg(feature = "cef")]
pub use runtime::{
    Profile, handle_subprocess, init, reserve_devtools_pipe, shutdown, window_background,
};
#[cfg(not(feature = "cef"))]
pub use stub::{
    HoleBackground, Profile, WebView, chromium_version, handle_subprocess, init,
    reserve_devtools_pipe, shutdown, window_background,
};
pub use types::{
    ContextMenu, DialogKind, Disposition, FileChooser, JsDialog, LogLevel, RemoteDebugging,
    RuntimeOptions, WebViewEvent,
};
#[cfg(feature = "cef")]
pub use webview::WebView;

/// The Chromium version pages run in, e.g. `154.0.8037.98`.
#[cfg(feature = "cef")]
pub fn chromium_version() -> &'static str {
    let full = std::str::from_utf8(cef::sys::CEF_VERSION)
        .unwrap_or_default()
        .trim_end_matches('\0');
    full.rsplit_once("chromium-").map_or(full, |(_, v)| v)
}
