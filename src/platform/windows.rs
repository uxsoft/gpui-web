//! Windows: Chromium's own child window under GPUI's window.
//!
//! GPUI draws through DirectComposition with a topmost target, so its
//! content covers child windows; with a transparent background it leaves a
//! hole where the page's window shows. Chromium composites the page itself
//! (no copies). Its windows are transparent to the mouse: GPUI gets every
//! click, so its menus and dialogs over the page work, and passes those on
//! the page to Chromium. The keyboard goes to Chromium's window while the
//! page has focus (native input and IME); the app's shortcuts are handed
//! back to GPUI (see `WebKeyboard`).

use std::collections::HashSet;
use std::sync::OnceLock;

use cef::{Browser, ImplBrowser as _, ImplBrowserHost as _, Rect, WindowInfo};
use gpui::{App, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, EnumChildWindows, GetParent, HTTRANSPARENT, SW_HIDE, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetParent, SetWindowPos, ShowWindow, WINDOW_EX_STYLE,
    WM_NCHITTEST, WS_POPUP,
};
use windows::core::{BOOL, w};

use super::{Placement, Presenter};

/// Whether GPUI draws through DirectComposition, which the hole needs.
pub(crate) fn direct_composition() -> bool {
    std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_none()
}

/// A hidden window pages live in until they're shown in a GPUI window.
fn parking_window() -> HWND {
    static PARKING: OnceLock<isize> = OnceLock::new();
    let hwnd = *PARKING.get_or_init(|| unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("Static"),
            w!("gpui_web"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .map(|h| h.0 as isize)
        .unwrap_or(0)
    });
    HWND(hwnd as *mut _)
}

/// Window info for a page created as a child of the parking window.
pub(crate) fn child_window_info(info: WindowInfo) -> WindowInfo {
    info.set_as_child(
        cef::sys::HWND(parking_window().0 as *mut _),
        &Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
    )
}

/// Lets mouse events through Chromium's windows to GPUI's.
unsafe extern "system" fn transparent_to_mouse(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn collect_child(hwnd: HWND, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    BOOL(1)
}

pub(crate) struct WindowedPresenter {
    gpui: HWND,
    child: Option<HWND>,
    /// Chromium's windows already transparent to the mouse.
    subclassed: HashSet<isize>,
    shown: bool,
    position: Option<(i32, i32, i32, i32)>,
}

impl WindowedPresenter {
    pub fn new(window: &mut Window) -> Self {
        let gpui = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut _),
            _ => HWND(std::ptr::null_mut()),
        };
        Self {
            gpui,
            child: None,
            subclassed: HashSet::new(),
            shown: false,
            position: None,
        }
    }

    /// Makes Chromium's windows (they come and go with navigations)
    /// transparent to the mouse.
    fn subclass_children(&mut self) {
        let Some(child) = self.child else { return };
        let mut windows = vec![child];
        unsafe {
            let _ = EnumChildWindows(
                Some(child),
                Some(collect_child),
                LPARAM(&mut windows as *mut _ as isize),
            );
        }
        for hwnd in windows {
            if self.subclassed.insert(hwnd.0 as isize) {
                unsafe {
                    let _ = SetWindowSubclass(hwnd, Some(transparent_to_mouse), 1, 0);
                }
            }
        }
    }
}

impl Presenter for WindowedPresenter {
    fn punches_hole(&self) -> bool {
        true
    }

    fn has_content(&self) -> bool {
        self.child.is_some()
    }

    fn place(&mut self, place: &Placement, window: &mut Window, _cx: &mut App) {
        let Some(child) = self.child else { return };
        let scale = window.scale_factor();
        let device = |v: gpui::Pixels| (f32::from(v) * scale).round() as i32;
        let bounds = place.visible;
        let position = (
            device(bounds.origin.x),
            device(bounds.origin.y),
            device(bounds.size.width),
            device(bounds.size.height),
        );
        unsafe {
            if GetParent(child).ok() != Some(self.gpui) && !self.gpui.0.is_null() {
                let _ = SetParent(child, Some(self.gpui));
                self.position = None;
            }
            if self.position != Some(position) || !self.shown {
                let _ = SetWindowPos(
                    child,
                    None,
                    position.0,
                    position.1,
                    position.2,
                    position.3,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                self.position = Some(position);
            }
        }
        self.shown = true;
        self.subclass_children();
    }

    fn hide(&mut self) {
        if let (true, Some(child)) = (self.shown, self.child) {
            unsafe {
                let _ = ShowWindow(child, SW_HIDE);
            }
        }
        self.shown = false;
    }

    fn set_browser(&mut self, browser: &Browser) {
        if self.child.is_some() {
            return;
        }
        if let Some(host) = browser.host() {
            let hwnd = host.window_handle();
            if !hwnd.0.is_null() {
                self.child = Some(HWND(hwnd.0 as *mut _));
            }
        }
    }

    fn focus_changed(&mut self, focused: bool) {
        // The keyboard follows Win32 focus: back to GPUI when the page
        // loses GPUI's focus.
        if !focused && !self.gpui.0.is_null() {
            unsafe {
                let _ = SetFocus(Some(self.gpui));
            }
        }
    }
}
