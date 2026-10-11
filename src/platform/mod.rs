//! How pages reach the screen on each platform.
//!
//! - Wayland: our own `wl_subsurface` below GPUI's surface, fed by one GPU
//!   blit per frame ([`wayland`]); GPUI leaves a hole where the page is.
//! - macOS: a pooled `CVPixelBuffer` drawn by GPUI's `surface` element
//!   ([`macos`]).
//! - Windows: Chromium's own child window under GPUI's DirectComposition
//!   target ([`windows`]); GPUI leaves a hole.
//! - Elsewhere (X11, or when the above can't start): CPU frames drawn as a
//!   GPUI image ([`cpu`]).

use cef::{AcceleratedPaintInfo, PaintElementType, Rect, Settings};
use gpui::{App, Bounds, Pixels, Window};

use crate::runtime::Mode;

pub(crate) mod cpu;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "linux")]
pub(crate) mod wayland;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

/// Where the web view element is this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    /// Where the page is drawn, in window coordinates (logical pixels),
    /// snapped so its edges fall on device pixels.
    pub bounds: Bounds<Pixels>,
    /// The part of `bounds` inside the element's content mask.
    pub visible: Bounds<Pixels>,
}

pub(crate) trait Presenter {
    /// Whether the page is composited under the window, through a hole,
    /// rather than drawn by GPUI.
    fn punches_hole(&self) -> bool;

    /// Whether a frame is on screen (or would be, when placed).
    fn has_content(&self) -> bool;

    /// Device pixels per logical pixel where the page is shown, when the
    /// presenter knows better than GPUI.
    fn scale(&self) -> Option<f32> {
        None
    }

    /// Shows the page at `place`; called while painting the element.
    fn place(&mut self, place: &Placement, window: &mut Window, cx: &mut App);

    /// Takes the page off screen until it is placed again.
    fn hide(&mut self);

    /// A GPU frame; returns whether GPUI needs to draw a frame for it.
    fn on_accelerated_paint(&mut self, _kind: PaintElementType, _info: &AcceleratedPaintInfo) -> bool {
        false
    }

    /// A CPU frame (BGRA); returns whether GPUI needs to draw a frame.
    fn on_paint(&mut self, _kind: PaintElementType, _dirty: &[Rect], _pixels: &[u8], _width: i32, _height: i32) -> bool {
        false
    }

    fn on_popup_show(&mut self, _show: bool) {}

    fn on_popup_size(&mut self, _rect: Rect) {}

    /// The display's refresh rate, for the page's frame rate.
    fn refresh_rate(&self) -> Option<i32> {
        None
    }

    /// The page's browser exists (windowed mode adopts its window).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn set_browser(&mut self, _browser: &cef::Browser) {}
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn force_cpu() -> bool {
    env("GPUI_WEB_PRESENTER").as_deref() == Some("cpu")
}

/// How this process renders pages.
pub(crate) fn mode() -> Mode {
    if force_cpu() || env("GPUI_WEB_SHARED_TEXTURE").as_deref() == Some("0") {
        return Mode::Software;
    }
    #[cfg(target_os = "linux")]
    return if wayland::is_wayland_session() && wayland::chromium_render_node().is_some() {
        Mode::SharedTexture
    } else {
        Mode::Software
    };
    #[cfg(target_os = "macos")]
    return Mode::SharedTexture;
    #[cfg(target_os = "windows")]
    return if windows::direct_composition() { Mode::Windowed } else { Mode::Software };
    #[allow(unreachable_code)]
    Mode::Software
}

/// Whether GPUI's window needs to be transparent for holes.
pub(crate) fn hole_punch_expected() -> bool {
    match mode() {
        Mode::SharedTexture => cfg!(target_os = "linux"),
        // Wayland shows CPU frames through the subsurface too.
        Mode::Software => !force_cpu() && linux_wayland(),
        Mode::Windowed => true,
    }
}

fn linux_wayland() -> bool {
    #[cfg(target_os = "linux")]
    return wayland::is_wayland_session();
    #[allow(unreachable_code)]
    false
}

/// Whether pages render into a device-sized view at scale 1, with page zoom
/// (Linux's CEF ignores the device scale factor; see docs/webview-spikes.md).
pub(crate) fn device_rect_default() -> bool {
    match env("GPUI_WEB_DEVICE_RECT").as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => cfg!(target_os = "linux"),
    }
}

/// The browser process's command-line switches for `mode`.
pub(crate) fn switches(mode: Mode) -> Vec<(String, Option<String>)> {
    let mut switches: Vec<(String, Option<String>)> = vec![
        ("no-startup-window".into(), None),
        ("noerrdialogs".into(), None),
        ("hide-crash-restore-bubble".into(), None),
        ("disable-features".into(), Some("Translate,MediaRouter".into())),
    ];
    #[cfg(target_os = "linux")]
    {
        let wayland = wayland::is_wayland_session();
        let platform = if wayland { "wayland" } else { "x11" };
        switches.push(("ozone-platform".into(), Some(platform.into())));
        switches.push(("password-store".into(), Some("basic".into())));
        // ANGLE on GL, on a Mesa GPU: ANGLE on Vulkan leaves shared
        // textures empty, and NVIDIA's driver can't make them at all (see
        // docs/webview-spikes.md).
        let angle = env("GPUI_WEB_ANGLE").unwrap_or_else(|| "default".into());
        if angle != "default" {
            switches.push(("use-gl".into(), Some("angle".into())));
            switches.push(("use-angle".into(), Some(angle.clone())));
            if angle == "vulkan" {
                switches.push((
                    "enable-features".into(),
                    Some("Vulkan,DefaultANGLEVulkan,VulkanFromANGLE".into()),
                ));
            }
        }
        let node = match env("GPUI_WEB_RENDER_NODE") {
            Some(node) if node == "none" => None,
            Some(node) => Some(std::path::PathBuf::from(node)),
            None if mode == Mode::SharedTexture => wayland::chromium_render_node(),
            None => None,
        };
        if let Some(node) = node {
            switches.push(("render-node-override".into(), Some(node.to_string_lossy().into())));
        }
    }
    #[cfg(target_os = "macos")]
    switches.push(("use-mock-keychain".into(), None));
    let _ = mode;
    switches
}

/// Platform parts of CEF's settings.
pub(crate) fn settings() -> Settings {
    #[cfg(target_os = "macos")]
    return macos::settings();
    #[allow(unreachable_code)]
    Settings::default()
}

/// The presenter for a page shown in `window`.
pub(crate) fn create(window: &mut Window, mode: Mode) -> Box<dyn Presenter> {
    match mode {
        Mode::Windowed => {
            #[cfg(target_os = "windows")]
            return Box::new(windows::WindowedPresenter::new(window));
        }
        Mode::SharedTexture => {
            #[cfg(target_os = "linux")]
            match wayland::WaylandPresenter::new(window) {
                Ok(presenter) => return Box::new(presenter),
                Err(err) => tracing::warn!("web view: no Wayland presenter ({err:#}); drawing frames on the CPU"),
            }
            #[cfg(target_os = "macos")]
            match macos::SurfacePresenter::new() {
                Ok(presenter) => return Box::new(presenter),
                Err(err) => tracing::warn!("web view: no Metal presenter ({err:#}); drawing frames on the CPU"),
            }
        }
        Mode::Software => {
            #[cfg(target_os = "linux")]
            if !force_cpu() && wayland::is_wayland_session() {
                match wayland::WaylandPresenter::new(window) {
                    Ok(presenter) => return Box::new(presenter),
                    Err(err) => tracing::warn!("web view: no Wayland presenter ({err:#}); drawing frames on the CPU"),
                }
            }
        }
    }
    let _ = window;
    Box::new(cpu::CpuPresenter::default())
}
