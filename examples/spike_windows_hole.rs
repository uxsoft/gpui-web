//! Spike S2: a child window under a transparent GPUI window (Windows).
//!
//! GPUI draws through DirectComposition with a topmost target, over child
//! windows. The window paints its background everywhere except a hole; a
//! solid red child window sits in the hole, and a GPUI popover is drawn over
//! it. Check that:
//! - the red shows through the hole, and never the desktop;
//! - the popover and its translucent strip draw over the red;
//! - clicking the popover's strip counts clicks (the child window must not
//!   take them: it is made transparent to the mouse, as pages are).
//!
//! Run: cargo run -p gpui_web --example spike_windows_hole (Windows only)

#[cfg(target_os = "windows")]
fn main() {
    windows_spike::main();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("spike_windows_hole runs on Windows only");
}

#[cfg(target_os = "windows")]
mod windows_spike {
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, HTTRANSPARENT, RegisterClassW, SWP_NOACTIVATE,
        SWP_NOZORDER, SetWindowPos, WINDOW_EX_STYLE, WM_NCHITTEST, WNDCLASSW, WS_CHILD, WS_VISIBLE,
    };
    use windows::core::w;

    unsafe extern "system" fn red_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_NCHITTEST {
            return LRESULT(HTTRANSPARENT as isize);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    struct Spike {
        child: Option<HWND>,
        popover: bool,
        clicks: usize,
    }

    impl Spike {
        fn child(&mut self, window: &Window) -> Option<HWND> {
            if self.child.is_none() {
                let parent = match HasWindowHandle::window_handle(window).ok()?.as_raw() {
                    RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
                    _ => return None,
                };
                unsafe {
                    let instance = GetModuleHandleW(None).ok()?;
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(red_proc),
                        hInstance: instance.into(),
                        lpszClassName: w!("gpui_web_spike_red"),
                        hbrBackground: CreateSolidBrush(COLORREF(0x0030_30e0)),
                        ..Default::default()
                    };
                    RegisterClassW(&class);
                    self.child = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("gpui_web_spike_red"),
                        w!(""),
                        WS_CHILD | WS_VISIBLE,
                        0,
                        0,
                        10,
                        10,
                        Some(parent),
                        None,
                        Some(instance.into()),
                        None,
                    )
                    .ok();
                }
            }
            self.child
        }
    }

    impl Render for Spike {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let child = self.child(window);
            let bg = rgb(0x2b2d31);
            let hole: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>> = Default::default();
            div()
                .size_full()
                .relative()
                .text_color(rgb(0xffffff))
                .child(
                    // The background: everything but the hole.
                    canvas(|_, _, _| {}, {
                        let hole = hole.clone();
                        move |bounds, _, window, _| match hole.get() {
                            None => window.paint_quad(fill(bounds, bg)),
                            Some(h) => {
                                let corners = |x0, y0, x1, y1| {
                                    Bounds::from_corners(point(x0, y0), point(x1, y1))
                                };
                                for r in [
                                    corners(bounds.left(), bounds.top(), bounds.right(), h.top()),
                                    corners(
                                        bounds.left(),
                                        h.bottom(),
                                        bounds.right(),
                                        bounds.bottom(),
                                    ),
                                    corners(bounds.left(), h.top(), h.left(), h.bottom()),
                                    corners(h.right(), h.top(), bounds.right(), h.bottom()),
                                ] {
                                    window.paint_quad(fill(r, bg));
                                }
                            }
                        }
                    })
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .size_full()
                        .p_4()
                        .gap_2()
                        .child(
                            div()
                                .id("toggle")
                                .px_3()
                                .py_1()
                                .bg(rgb(0x5865f2))
                                .rounded_md()
                                .cursor_pointer()
                                .child(format!("Popover ({} clicks)", self.clicks))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.popover = !this.popover;
                                    cx.notify();
                                })),
                        )
                        .child(
                            canvas(
                                {
                                    let hole = hole.clone();
                                    move |bounds, _, _| hole.set(Some(bounds))
                                },
                                move |bounds, _, window, _| {
                                    // The hole is the child's area.
                                    let Some(child) = child else { return };
                                    let scale = window.scale_factor();
                                    let d = |v: Pixels| (f32::from(v) * scale).round() as i32;
                                    unsafe {
                                        let _ = SetWindowPos(
                                            child,
                                            None,
                                            d(bounds.origin.x),
                                            d(bounds.origin.y),
                                            d(bounds.size.width),
                                            d(bounds.size.height),
                                            SWP_NOZORDER | SWP_NOACTIVATE,
                                        );
                                    }
                                },
                            )
                            .flex_1()
                            .w_full(),
                        ),
                )
                .when(self.popover, |this| {
                    this.child(
                        div()
                            .absolute()
                            .top(px(60.))
                            .left(px(40.))
                            .w(px(320.))
                            .p_4()
                            .rounded_lg()
                            .shadow_lg()
                            .bg(rgb(0x1e1f22))
                            .child("GPUI popover over the child window")
                            .child(
                                div()
                                    .id("strip")
                                    .mt_2()
                                    .p_2()
                                    .bg(rgba(0x5865f280))
                                    .child("50% alpha: click me")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.clicks += 1;
                                        cx.notify();
                                    })),
                            ),
                    )
                })
        }
    }

    pub fn main() {
        gpui_kit::application().run(|cx: &mut App| {
            gpui_kit::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(900.), px(600.)),
                    cx,
                ))),
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            };
            cx.open_window(options, |_, cx| {
                cx.new(|_| Spike {
                    child: None,
                    popover: true,
                    clicks: 0,
                })
            })
            .unwrap();
        });
    }
}
