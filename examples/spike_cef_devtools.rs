//! Spike S5: DevTools for agents over an in-process pipe.
//!
//! Before CEF starts, the browser process puts a pipe on fds 3 and 4 and
//! passes `--remote-debugging-pipe`, the transport Wisp already speaks to
//! headless Chrome. The spike then checks the browser-level `Target.*`
//! methods the CDP proxy relies on, and whether a browser can tell its own
//! DevTools target id through `SendDevToolsMessage`.
//!
//! Run: cargo run -p gpui_web --example spike_cef_devtools (Unix only)

#[cfg(unix)]
fn main() {
    unix::main();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("spike_cef_devtools uses fds 3/4 and runs on Unix only");
}

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::FromRawFd;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use cef::args::Args;
    use cef::*;

    wrap_app! {
        struct SpikeApp;

        impl App {
            fn on_before_command_line_processing(
                &self,
                _process_type: Option<&CefStringUtf16>,
                command_line: Option<&mut CommandLine>,
            ) {
                let Some(command_line) = command_line else { return };
                command_line.append_switch_with_value(Some(&"ozone-platform".into()), Some(&"wayland".into()));
                command_line.append_switch(Some(&"remote-debugging-pipe".into()));
                command_line.append_switch(Some(&"no-startup-window".into()));
            }
        }
    }

    wrap_render_handler! {
        struct SpikeRender;

        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
                if let Some(rect) = rect {
                    rect.width = 800;
                    rect.height = 600;
                }
            }
        }
    }

    wrap_dev_tools_message_observer! {
        struct SpikeObserver {
            tx: mpsc::Sender<String>,
        }

        impl DevToolsMessageObserver {
            fn on_dev_tools_method_result(
                &self,
                _browser: Option<&mut Browser>,
                message_id: ::std::os::raw::c_int,
                success: ::std::os::raw::c_int,
                result: Option<&[u8]>,
            ) {
                let text = String::from_utf8_lossy(result.unwrap_or_default()).to_string();
                let _ = self.tx.send(format!("SendDevToolsMessage #{message_id} ok={success}: {text}"));
            }
        }
    }

    wrap_client! {
        struct SpikeClient {
            render_handler: RenderHandler,
        }

        impl Client {
            fn render_handler(&self) -> Option<RenderHandler> {
                Some(self.render_handler.clone())
            }
        }
    }

    /// Puts a pipe pair on fds 3 (Chromium reads) and 4 (Chromium writes);
    /// returns our ends.
    fn reserve_pipe() -> (std::fs::File, std::fs::File) {
        // Moves an fd out of the 3..=4 range (pipe2 may return exactly those).
        fn high(fd: i32) -> i32 {
            let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
            assert!(moved >= 10);
            unsafe { libc::close(fd) };
            moved
        }
        let mut to_chrome = [0; 2];
        let mut from_chrome = [0; 2];
        unsafe {
            assert_eq!(libc::pipe2(to_chrome.as_mut_ptr(), libc::O_CLOEXEC), 0);
            assert_eq!(libc::pipe2(from_chrome.as_mut_ptr(), libc::O_CLOEXEC), 0);
            let fds = [high(to_chrome[0]), high(to_chrome[1]), high(from_chrome[0]), high(from_chrome[1])];
            assert_eq!(libc::dup2(fds[0], 3), 3);
            assert_eq!(libc::dup2(fds[3], 4), 4);
            libc::close(fds[0]);
            libc::close(fds[3]);
            (std::fs::File::from_raw_fd(fds[1]), std::fs::File::from_raw_fd(fds[2]))
        }
    }

    pub fn main() {
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = Args::new();
        let mut app = SpikeApp::new();
        let code = execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
        if code >= 0 {
            std::process::exit(code);
        }
        let show = |at: &str| {
            for fd in [3, 4] {
                eprintln!("{at}: fd {fd} -> {:?}", std::fs::read_link(format!("/proc/self/fd/{fd}")));
            }
        };
        show("before reserve");
        let (mut to_chrome, from_chrome) = reserve_pipe();
        show("after reserve");
        let (tx, rx) = mpsc::channel::<String>();
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(from_chrome);
                loop {
                    let mut message = Vec::new();
                    match reader.read_until(0, &mut message) {
                        Ok(0) | Err(_) => { let _ = tx.send("pipe closed".into()); return }
                        Ok(_) => {
                            message.pop();
                            let _ = tx.send(format!("pipe: {}", String::from_utf8_lossy(&message)));
                        }
                    }
                }
            });
        }

        let cache = std::env::temp_dir().join("gpui_web_spike_devtools");
        let settings = Settings {
            windowless_rendering_enabled: 1,
            external_message_pump: 1,
            no_sandbox: 1,
            root_cache_path: cache.to_string_lossy().as_ref().into(),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        };
        assert_eq!(
            initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()),
            1
        );
        show("after initialize");
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: 1,
            ..Default::default()
        };
        let mut client = SpikeClient::new(SpikeRender::new());
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&"data:text/html,<title>spike page</title>hello".into()),
            Some(&BrowserSettings::default()),
            None,
            None,
        )
        .expect("browser");
        let host = browser.host().unwrap();
        let mut observer = SpikeObserver::new(tx.clone());
        let _registration = host.add_dev_tools_message_observer(Some(&mut observer));

        let pump = |for_ms: u64| {
            let until = Instant::now() + Duration::from_millis(for_ms);
            while Instant::now() < until {
                do_message_loop_work();
                std::thread::sleep(Duration::from_millis(5));
                while let Ok(line) = rx.try_recv() {
                    println!("{}", truncate(&line));
                }
            }
        };
        let mut send = |id: u32, method: &str, params: &str| {
            let message = format!(r#"{{"id":{id},"method":"{method}","params":{params}}}"#);
            println!("-> {message}");
            to_chrome.write_all(message.as_bytes()).unwrap();
            to_chrome.write_all(&[0]).unwrap();
        };
        pump(1500);
        host.send_dev_tools_message(Some(br#"{"id":900,"method":"Target.getTargetInfo"}"#));
        pump(500);
        send(1, "Browser.getVersion", "{}");
        send(2, "Target.setDiscoverTargets", r#"{"discover":true}"#);
        send(3, "Target.getTargets", "{}");
        send(4, "Target.attachToBrowserTarget", "{}");
        pump(1000);
        send(5, "Target.createTarget", r#"{"url":"about:blank"}"#);
        pump(2000);
        send(6, "Target.getTargets", "{}");
        send(7, "Browser.setDownloadBehavior", r#"{"behavior":"default"}"#);
        pump(1000);

        host.close_browser(1);
        pump(1000);
        drop(host);
        drop(browser);
        shutdown();
    }

    fn truncate(line: &str) -> String {
        if line.len() > 600 { format!("{}…", &line[..600]) } else { line.to_string() }
    }
}
