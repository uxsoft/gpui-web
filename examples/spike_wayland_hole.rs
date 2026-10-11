//! Spike S1: a Wayland subsurface under a transparent GPUI window.
//!
//! The window paints an opaque background everywhere except a hole. A
//! `wl_subsurface` of our own, placed below GPUI's surface, shows a 1-pixel
//! checker pattern (at physical resolution) through the hole, and a GPUI
//! popover is drawn over it. Click "Popover" to toggle the overlay; resize
//! the window to see the subsurface follow the hole.
//!
//! Run: cargo run -p gpui_web --example spike_wayland_hole

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("spike_wayland_hole runs on Linux/Wayland only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::AsFd;
    use std::sync::{Arc, Mutex};

    use gpui_kit::*;
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_region::WlRegion,
        wl_registry::WlRegistry, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    };
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::wp::fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use wayland_protocols::wp::viewporter::client::{
        wp_viewport::WpViewport, wp_viewporter::WpViewporter,
    };

    struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore WlCompositor);
    delegate_noop!(State: ignore WlSubcompositor);
    delegate_noop!(State: ignore WlSubsurface);
    delegate_noop!(State: ignore WlSurface);
    delegate_noop!(State: ignore WlShm);
    delegate_noop!(State: ignore WlShmPool);
    delegate_noop!(State: ignore WlBuffer);
    delegate_noop!(State: ignore WlRegion);
    delegate_noop!(State: ignore WpViewporter);
    delegate_noop!(State: ignore WpViewport);
    delegate_noop!(State: ignore WpFractionalScaleManagerV1);

    impl Dispatch<WpFractionalScaleV1, Arc<AtomicU32>> for State {
        fn event(
            _: &mut Self,
            _: &WpFractionalScaleV1,
            event: wp_fractional_scale_v1::Event,
            scale: &Arc<AtomicU32>,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wp_fractional_scale_v1::Event::PreferredScale { scale: s } = event {
                eprintln!("subsurface preferred scale: {}", s as f32 / 120.0);
                scale.store(s, Ordering::Relaxed);
            }
        }
    }

    /// Our objects on GPUI's connection, on a queue of our own.
    struct Sub {
        conn: Connection,
        qh: QueueHandle<State>,
        shm: WlShm,
        surface: WlSurface,
        subsurface: WlSubsurface,
        viewport: WpViewport,
        size: (i32, i32),
        pos: (i32, i32),
        scale: Arc<AtomicU32>,
    }

    impl Sub {
        fn new(window: &Window) -> anyhow::Result<Self> {
            let display = match window.display_handle()?.as_raw() {
                RawDisplayHandle::Wayland(h) => h.display.as_ptr(),
                other => anyhow::bail!("not a Wayland window: {other:?}"),
            };
            let parent_ptr = match HasWindowHandle::window_handle(window)?.as_raw() {
                RawWindowHandle::Wayland(h) => h.surface.as_ptr(),
                other => anyhow::bail!("not a Wayland window: {other:?}"),
            };
            let backend = unsafe { Backend::from_foreign_display(display.cast()) };
            let conn = Connection::from_backend(backend);
            let parent_id =
                unsafe { ObjectId::from_ptr(WlSurface::interface(), parent_ptr.cast())? };
            let parent = WlSurface::from_id(&conn, parent_id)?;

            let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
            let qh = queue.handle();
            let compositor: WlCompositor = globals.bind(&qh, 4..=6, ())?;
            let subcompositor: WlSubcompositor = globals.bind(&qh, 1..=1, ())?;
            let shm: WlShm = globals.bind(&qh, 1..=1, ())?;
            let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ())?;

            let surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&surface, &parent, &qh, ());
            subsurface.place_below(&parent);
            subsurface.set_desync();
            // Input goes to GPUI's surface, which covers ours.
            let empty = compositor.create_region(&qh, ());
            surface.set_input_region(Some(&empty));
            empty.destroy();
            let viewport = viewporter.get_viewport(&surface, &qh, ());
            let scale = Arc::new(AtomicU32::new(0));
            let fractional: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, ())?;
            let _fs = fractional.get_fractional_scale(&surface, &qh, scale.clone());
            // GPUI commits the parent; this dispatches our queue's events.
            std::thread::spawn(move || {
                let mut state = State;
                loop {
                    if let Err(error) = queue.blocking_dispatch(&mut state) {
                        eprintln!("subsurface queue: {error}");
                        return;
                    }
                }
            });
            Ok(Self { conn, qh, shm, surface, subsurface, viewport, size: (0, 0), pos: (i32::MIN, 0), scale })
        }

        /// Places the subsurface at `bounds` (logical) with a buffer of
        /// `bounds * scale` pixels. Position applies on GPUI's next commit.
        fn place(&mut self, bounds: Bounds<Pixels>, gpui_scale: f32) {
            let ours = self.scale.load(Ordering::Relaxed);
            let scale = if ours > 0 { ours as f32 / 120.0 } else { gpui_scale };
            let pos = (f32::from(bounds.origin.x) as i32, f32::from(bounds.origin.y) as i32);
            let logical = (f32::from(bounds.size.width) as i32, f32::from(bounds.size.height) as i32);
            let physical = (
                (logical.0 as f32 * scale).round() as i32,
                (logical.1 as f32 * scale).round() as i32,
            );
            if pos != self.pos {
                self.subsurface.set_position(pos.0, pos.1);
                self.pos = pos;
            }
            if physical != self.size && physical.0 > 0 && physical.1 > 0 {
                eprintln!("place bounds={bounds:?} gpui_scale={gpui_scale} scale={scale} logical={logical:?} physical={physical:?}");
                self.size = physical;
                let buffer = self.checker(physical.0, physical.1);
                self.surface.attach(Some(&buffer), 0, 0);
                self.viewport.set_destination(logical.0, logical.1);
                self.surface.damage_buffer(0, 0, physical.0, physical.1);
                buffer.destroy();
            }
            self.surface.commit();
            let _ = self.conn.flush();
        }

        /// A 1-pixel checker on the left half (blurs if scaled), color bars on the right.
        fn checker(&self, w: i32, h: i32) -> WlBuffer {
            let stride = w * 4;
            let len = (stride * h) as usize;
            let fd = unsafe { libc::memfd_create(c"spike".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            file.set_len(len as u64).unwrap();
            let map = unsafe {
                libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
            };
            let pixels = unsafe { std::slice::from_raw_parts_mut(map as *mut u32, (w * h) as usize) };
            for y in 0..h {
                for x in 0..w {
                    pixels[(y * w + x) as usize] = if x < w / 2 {
                        if (x + y) % 2 == 0 { 0xff000000 } else { 0xffffffff }
                    } else {
                        [0xffe04040, 0xff40c040, 0xff4060e0][((y * 3) / h.max(1)) as usize % 3]
                    };
                }
            }
            unsafe { libc::munmap(map, len) };
            let pool = self.shm.create_pool(file.as_fd(), len as i32, &self.qh, ());
            let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, &self.qh, ());
            pool.destroy();
            buffer
        }
    }

    impl Drop for Sub {
        fn drop(&mut self) {
            self.viewport.destroy();
            self.subsurface.destroy();
            self.surface.destroy();
        }
    }

    struct Spike {
        sub: Option<Arc<Mutex<Sub>>>,
        popover: bool,
        error: Option<String>,
    }

    impl Render for Spike {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.sub.is_none() && self.error.is_none() {
                match Sub::new(window) {
                    Ok(sub) => self.sub = Some(Arc::new(Mutex::new(sub))),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            let hole: Arc<Mutex<Option<Bounds<Pixels>>>> = Default::default();
            let sub = self.sub.clone();
            let bg = rgb(0x2b2d31);
            div()
                .size_full()
                .relative()
                .text_color(rgb(0xffffff))
                .child(
                    // The background: everything but the hole.
                    canvas(|_, _, _| {}, {
                        let hole = hole.clone();
                        move |bounds, _, window, _| {
                            let Some(h) = *hole.lock().unwrap() else {
                                window.paint_quad(fill(bounds, bg));
                                return;
                            };
                            for r in around(bounds, h) {
                                window.paint_quad(fill(r, bg));
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
                                .flex()
                                .gap_2()
                                .child(
                                    div()
                                        .id("toggle")
                                        .px_3()
                                        .py_1()
                                        .bg(rgb(0x5865f2))
                                        .rounded_md()
                                        .cursor_pointer()
                                        .child("Popover")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.popover = !this.popover;
                                            cx.notify();
                                        })),
                                )
                                .child(self.error.clone().unwrap_or_else(|| {
                                    "Left half: 1px checker (must be crisp). Right: color bars.".into()
                                })),
                        )
                        .child(
                            // The page area: records the hole and places the subsurface.
                            canvas(
                                {
                                    let hole = hole.clone();
                                    move |bounds, _, _| {
                                        *hole.lock().unwrap() = Some(bounds);
                                    }
                                },
                                move |bounds, _, window, _| {
                                    if let Some(sub) = &sub {
                                        sub.lock().unwrap().place(bounds, window.scale_factor());
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
                            .border_1()
                            .border_color(rgb(0x5865f2))
                            .child("GPUI popover over the subsurface")
                            .child(
                                div()
                                    .mt_2()
                                    .p_2()
                                    .bg(rgba(0x5865f280))
                                    .child("50% alpha strip: must blend with the page"),
                            ),
                    )
                })
        }
    }

    use gpui_kit::prelude::FluentBuilder as _;

    /// Up to four rects covering `outer` minus `hole`.
    fn around(outer: Bounds<Pixels>, hole: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
        let (ox0, oy0, ox1, oy1) = (outer.left(), outer.top(), outer.right(), outer.bottom());
        let (hx0, hy0, hx1, hy1) = (hole.left(), hole.top(), hole.right(), hole.bottom());
        let rect = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| {
            Bounds::from_corners(point(x0, y0), point(x1, y1))
        };
        vec![
            rect(ox0, oy0, ox1, hy0),
            rect(ox0, hy1, ox1, oy1),
            rect(ox0, hy0, hx0, hy1),
            rect(hx1, hy0, ox1, hy1),
        ]
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
                cx.new(|_| Spike { sub: None, popover: true, error: None })
            })
            .unwrap();
        });
    }
}
