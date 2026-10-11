//! Spike S4: CEF off-screen rendering with shared textures on Linux.
//!
//! Runs a windowless browser on an animated page and logs what
//! `OnAcceleratedPaint` delivers (planes, fds, strides, offsets, modifier,
//! format, coded size) and the achieved frame rate.
//!
//! Linux only. Run: cargo run -p gpui_web --example spike_cef_dmabuf -- [scale] [fps] [angle]
//! e.g. `-- 1.5 144 vulkan`. Seconds to run: SPIKE_SECONDS (default 6).

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("spike_cef_dmabuf runs on Linux only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use cef::args::Args;
    use cef::*;

    #[derive(Clone)]
    struct Config {
        angle: String,
    }

    wrap_app! {
        struct SpikeApp {
            config: Config,
        }

        impl App {
            fn on_before_command_line_processing(
                &self,
                _process_type: Option<&CefStringUtf16>,
                command_line: Option<&mut CommandLine>,
            ) {
                let Some(command_line) = command_line else { return };
                if std::env::var("SPIKE_OZONE").is_err() {
                    command_line.append_switch_with_value(Some(&"ozone-platform".into()), Some(&"wayland".into()));
                }
                if self.config.angle != "default" {
                    command_line.append_switch_with_value(Some(&"use-gl".into()), Some(&"angle".into()));
                    command_line.append_switch_with_value(
                        Some(&"use-angle".into()),
                        Some(&self.config.angle.as_str().into()),
                    );
                }
                if self.config.angle == "vulkan" {
                    command_line.append_switch_with_value(
                        Some(&"enable-features".into()),
                        Some(&"Vulkan,DefaultANGLEVulkan,VulkanFromANGLE".into()),
                    );
                }
                if let Ok(dsf) = std::env::var("SPIKE_FORCE_DSF") {
                    command_line.append_switch_with_value(Some(&"force-device-scale-factor".into()), Some(&dsf.as_str().into()));
                }
                if let Ok(platform) = std::env::var("SPIKE_OZONE") {
                    command_line.append_switch_with_value(Some(&"ozone-platform".into()), Some(&platform.as_str().into()));
                }
                if let Ok(node) = std::env::var("SPIKE_RENDER_NODE") {
                    command_line.append_switch_with_value(
                        Some(&"render-node-override".into()),
                        Some(&node.as_str().into()),
                    );
                }
                command_line.append_switch(Some(&"no-startup-window".into()));
                command_line.append_switch(Some(&"use-mock-keychain".into()));
            }
        }
    }

    #[derive(Clone, Default)]
    struct Stats {
        frames: Arc<AtomicU32>,
        software: Arc<AtomicU32>,
        last_size: Arc<AtomicU32>,
        asked: Arc<std::sync::atomic::AtomicBool>,
        first: Arc<Mutex<Option<String>>>,
    }

    #[derive(Clone)]
    struct Render {
        scale: f32,
        stats: Stats,
    }

    wrap_render_handler! {
        struct SpikeRender {
            render: Render,
        }

        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
                if let Some(rect) = rect {
                    let k = if device_rect() { self.render.scale } else { 1.0 };
                    rect.width = (800.0 * k) as i32;
                    rect.height = (600.0 * k) as i32;
                }
            }

            fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
                let Some(info) = info else { return 0 };
                if !self.render.stats.asked.swap(true, Ordering::Relaxed) { eprintln!("screen_info asked"); }
                info.device_scale_factor = if device_rect() { 1.0 } else { self.render.scale };
                info.depth = 24;
                info.depth_per_component = 8;
                info.rect = Rect { x: 0, y: 0, width: 800, height: 600 };
                info.available_rect = info.rect.clone();
                1
            }

            fn on_paint(
                &self,
                _browser: Option<&mut Browser>,
                _type_: PaintElementType,
                _dirty_rects: Option<&[Rect]>,
                _buffer: *const u8,
                width: ::std::os::raw::c_int,
                height: ::std::os::raw::c_int,
            ) {
                self.render.stats.frames.fetch_add(1, Ordering::Relaxed);
                if self.render.stats.software.fetch_add(1, Ordering::Relaxed) == 0 {
                    eprintln!("software OnPaint {width}x{height} (shared textures not in use)");
                }
            }

            fn on_accelerated_paint(
                &self,
                _browser: Option<&mut Browser>,
                type_: PaintElementType,
                dirty_rects: Option<&[Rect]>,
                info: Option<&AcceleratedPaintInfo>,
            ) {
                let Some(info) = info else { return };
                self.render.stats.frames.fetch_add(1, Ordering::Relaxed);
                let coded = (info.extra.coded_size.width as u32) << 16 | info.extra.coded_size.height as u32;
                if self.render.stats.last_size.swap(coded, Ordering::Relaxed) != coded {
                    eprintln!("buffer size now {}x{} (frame {})", coded >> 16, coded & 0xffff, self.render.stats.frames.load(Ordering::Relaxed));
                }
                let mut first = self.render.stats.first.lock().unwrap();
                if first.is_none() {
                    let planes: Vec<String> = info.planes[..info.plane_count as usize]
                        .iter()
                        .map(|p| format!("fd={} stride={} offset={} size={}", p.fd, p.stride, p.offset, p.size))
                        .collect();
                    let text = format!(
                        "type={:?} planes={} [{}] modifier={:#x} format={:?} coded={}x{} visible={:?} content={:?} dirty={:?}",
                        type_,
                        info.plane_count,
                        planes.join("; "),
                        info.modifier,
                        info.format,
                        info.extra.coded_size.width,
                        info.extra.coded_size.height,
                        (info.extra.visible_rect.x, info.extra.visible_rect.y, info.extra.visible_rect.width, info.extra.visible_rect.height),
                        (info.extra.content_rect.width, info.extra.content_rect.height),
                        dirty_rects.map(|d| d.len()),
                    );
                    eprintln!("first accelerated paint: {text}");
                    *first = Some(text);
                }
            }
        }
    }

    fn device_rect() -> bool {
        std::env::var("SPIKE_DEVICE_RECT").is_ok()
    }

    wrap_display_handler! {
        struct SpikeDisplay;

        impl DisplayHandler {
            fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
                if let Some(title) = title {
                    eprintln!("page reports {title}");
                }
            }
        }
    }

    wrap_client! {
        struct SpikeClient {
            render_handler: RenderHandler,
            display_handler: DisplayHandler,
        }

        impl Client {
            fn render_handler(&self) -> Option<RenderHandler> {
                Some(self.render_handler.clone())
            }
            fn display_handler(&self) -> Option<DisplayHandler> {
                Some(self.display_handler.clone())
            }
        }
    }

    const PAGE: &str = "data:text/html,<style>body{margin:0;background:%23fff;font:16px sans-serif}\
    %23b{width:100px;height:100px;background:%23c33;position:absolute;animation:m 1s linear infinite}\
    @keyframes m{from{left:0}to{left:600px}}</style><div id=b></div><p style='margin-top:120px'>The quick brown fox</p>\
    <script>setInterval(()=>document.title=innerWidth+'x'+innerHeight+'@'+devicePixelRatio,500)</script>";

    pub fn main() {
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let mut cli = std::env::args().skip(1);
        let scale: f32 = cli.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let fps: i32 = cli.next().and_then(|s| s.parse().ok()).unwrap_or(60);
        let angle = cli.next().unwrap_or_else(|| "vulkan".into());
        // Subprocesses get Chromium's own argv; don't parse it there.
        let config = Config { angle };

        let args = Args::new();
        let mut app = SpikeApp::new(config);
        let code = execute_process(
            Some(args.as_main_args()),
            Some(&mut app),
            std::ptr::null_mut(),
        );
        if code >= 0 {
            std::process::exit(code);
        }

        let cache = std::env::temp_dir().join("gpui_web_spike_cef");
        let settings = Settings {
            windowless_rendering_enabled: 1,
            external_message_pump: 1,
            no_sandbox: 1,
            root_cache_path: cache.to_string_lossy().as_ref().into(),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        };
        assert_eq!(
            initialize(
                Some(args.as_main_args()),
                Some(&settings),
                Some(&mut app),
                std::ptr::null_mut()
            ),
            1,
            "CefInitialize failed"
        );

        let stats = Stats::default();
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: std::env::var("SPIKE_SOFTWARE").is_err() as i32,
            ..Default::default()
        };
        let browser_settings = BrowserSettings {
            windowless_frame_rate: fps,
            ..Default::default()
        };
        let mut client = SpikeClient::new(
            SpikeRender::new(Render {
                scale,
                stats: stats.clone(),
            }),
            SpikeDisplay::new(),
        );
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&PAGE.into()),
            Some(&browser_settings),
            None,
            None,
        );
        assert!(browser.is_some(), "browser creation failed");

        if let Some(host) = browser.as_ref().and_then(|b| b.host()) {
            host.notify_screen_info_changed();
            host.was_resized();
            if device_rect() {
                host.set_zoom_level((scale as f64).ln() / 1.2f64.ln());
            }
        }
        let seconds: u64 = std::env::var("SPIKE_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6);
        let start = Instant::now();
        let mut measured_from = None;
        while start.elapsed() < Duration::from_secs(seconds) {
            do_message_loop_work();
            if device_rect()
                && measured_from.is_none()
                && start.elapsed() > Duration::from_secs(1)
                && let Some(host) = browser.as_ref().and_then(|b| b.host())
            {
                let level = (scale as f64).ln() / 1.2f64.ln();
                if (host.zoom_level() - level).abs() > 1e-6 {
                    host.set_zoom_level(level);
                    eprintln!("zoom set to {level}");
                }
            }
            if measured_from.is_none() && start.elapsed() > Duration::from_secs(2) {
                measured_from = Some((Instant::now(), stats.frames.load(Ordering::Relaxed)));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Some((at, frames)) = measured_from {
            let n = stats.frames.load(Ordering::Relaxed) - frames;
            eprintln!(
                "requested {fps} fps at scale {scale}: {:.1} accelerated frames/s, {} software paints",
                n as f64 / at.elapsed().as_secs_f64(),
                stats.software.load(Ordering::Relaxed)
            );
        }
        if let Some(host) = browser.as_ref().and_then(|b| b.host()) {
            host.close_browser(1);
        }
        for _ in 0..100 {
            do_message_loop_work();
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(browser);
        shutdown();
    }
}
