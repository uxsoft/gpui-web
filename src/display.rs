//! Keeps the app's Wayland connection read while its main thread is busy.
//!
//! GPUI reads Wayland events on its main thread only. A compositor buffers
//! what a client hasn't read, but not forever: Smithay's (COSMIC's) drops
//! the client, without a protocol error, once its socket and a 4 KB buffer
//! are full, which a fast mouse fills in well under a second. Any stall then
//! closes the app: CEF's start, which runs on the main thread, or the
//! machine swapping. A thread of ours reads the shared connection instead;
//! libwayland moves each event to its own queue, so GPUI still dispatches
//! its events, and wakes up for them, on the main thread.

use gpui::Window;

/// Keeps reading `window`'s Wayland connection on a thread of its own, so
/// a blocked main thread can't get the app disconnected. Call when the
/// app's first window opens: CEF's start blocks the main thread. Once per
/// process is enough (later calls do nothing); a no-op off Wayland.
pub fn keep_reading_display(window: &Window) {
    #[cfg(target_os = "linux")]
    wayland::keep_reading(window);
    #[cfg(not(target_os = "linux"))]
    let _ = window;
}

#[cfg(target_os = "linux")]
mod wayland {
    use std::sync::OnceLock;
    use std::thread::JoinHandle;

    use anyhow::{Result, bail};
    use gpui::Window;
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    use wayland_backend::client::Backend;
    use wayland_client::Connection;

    pub(super) fn keep_reading(window: &Window) {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let display = match window.display_handle().map(|h| h.as_raw()) {
                Ok(RawDisplayHandle::Wayland(h)) => h.display.as_ptr(),
                _ => return,
            };
            if let Err(err) = unsafe { spawn_reader(display) } {
                tracing::warn!("Wayland reader: {err:#}");
            }
        });
    }

    /// Starts a thread reading the connection of `display` (a live
    /// `wl_display`) until it fails.
    pub(super) unsafe fn spawn_reader(display: *mut std::ffi::c_void) -> Result<JoinHandle<()>> {
        if display.is_null() {
            bail!("no wl_display");
        }
        // Its own (empty) queue: reading moves every event to the queue of
        // the object it is for, ours stays empty.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let mut queue = Connection::from_backend(backend).new_event_queue::<()>();
        Ok(std::thread::Builder::new()
            .name("gpui-web-wayland-reader".into())
            .spawn(move || {
                loop {
                    if let Err(err) = queue.blocking_dispatch(&mut ()) {
                        tracing::warn!("Wayland reader: {err}");
                        return;
                    }
                }
            })?)
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        use wayland_client::globals::{GlobalListContents, registry_queue_init};
        use wayland_client::protocol::{wl_output::WlOutput, wl_registry::WlRegistry};
        use wayland_client::{Dispatch, Proxy as _, QueueHandle, delegate_noop};
        use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
        use wayland_server::protocol::wl_output::{self, WlOutput as ServerOutput};
        use wayland_server::{DataInit, Display, GlobalDispatch, New};

        use super::*;

        /// Events the compositor sends a client that doesn't read them:
        /// 2.4 MB of `wl_output.mode`, far more than a socket holds.
        const BATCHES: usize = 1000;
        const PER_BATCH: usize = 100;

        #[derive(Default)]
        struct Server {
            output: Option<ServerOutput>,
        }

        impl GlobalDispatch<ServerOutput, ()> for Server {
            fn bind(
                state: &mut Self,
                _: &wayland_server::DisplayHandle,
                _: &wayland_server::Client,
                resource: New<ServerOutput>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                state.output = Some(init.init(resource, ()));
            }
        }

        impl wayland_server::Dispatch<ServerOutput, ()> for Server {
            fn request(
                _: &mut Self,
                _: &wayland_server::Client,
                _: &ServerOutput,
                _: wl_output::Request,
                _: &(),
                _: &wayland_server::DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }

        struct Watch(Arc<AtomicBool>);

        impl ClientData for Watch {
            fn disconnected(&self, _: ClientId, _: DisconnectReason) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        struct Client;

        impl Dispatch<WlRegistry, GlobalListContents> for Client {
            fn event(
                _: &mut Self,
                _: &WlRegistry,
                _: <WlRegistry as wayland_client::Proxy>::Event,
                _: &GlobalListContents,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
        delegate_noop!(Client: ignore WlOutput);

        /// Floods a client whose "main thread" never dispatches: whether the
        /// compositor dropped it, and whether it was connected and usable
        /// once the flood was over.
        fn flood(read_on_a_thread: bool) -> Option<(bool, bool)> {
            let (server_end, client_end) = UnixStream::pair().unwrap();
            // The smallest send buffer the kernel allows, so the test needs
            // little data to fill it.
            let size: libc::c_int = 1;
            unsafe {
                libc::setsockopt(
                    server_end.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            let Ok(backend) = Backend::connect(client_end) else {
                eprintln!("skipped: no libwayland-client");
                return None;
            };

            let killed = Arc::new(AtomicBool::new(false));
            let flooded = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let server = std::thread::spawn({
                let (killed, flooded, stop) = (killed.clone(), flooded.clone(), stop.clone());
                move || {
                    let mut display = Display::<Server>::new().unwrap();
                    let mut dh = display.handle();
                    dh.create_global::<Server, ServerOutput, ()>(4, ());
                    dh.insert_client(server_end, Arc::new(Watch(killed.clone())))
                        .unwrap();
                    let mut state = Server::default();
                    let mut batches = 0;
                    while !stop.load(Ordering::SeqCst) {
                        display.dispatch_clients(&mut state).ok();
                        if let Some(output) = &state.output
                            && batches < BATCHES
                            && !killed.load(Ordering::SeqCst)
                        {
                            for i in 0..PER_BATCH {
                                output.mode(wl_output::Mode::Current, i as i32, 600, 60_000);
                            }
                            batches += 1;
                            if batches == BATCHES {
                                flooded.store(true, Ordering::SeqCst);
                            }
                        }
                        display.flush_clients().ok();
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });

            let conn = Connection::from_backend(backend.clone());
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let output: WlOutput = globals.bind(&queue.handle(), 1..=4, ()).unwrap();
            conn.flush().unwrap();
            if read_on_a_thread {
                unsafe { spawn_reader(backend.display_ptr().cast()) }.unwrap();
            }
            // The main thread is "blocked": nothing dispatches `queue`.
            let deadline = Instant::now() + Duration::from_secs(20);
            while !flooded.load(Ordering::SeqCst)
                && !killed.load(Ordering::SeqCst)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let dropped = killed.load(Ordering::SeqCst);
            let alive = !dropped
                && flooded.load(Ordering::SeqCst)
                && queue.roundtrip(&mut Client).is_ok()
                && output.is_alive();
            stop.store(true, Ordering::SeqCst);
            server.join().unwrap();
            Some((dropped, alive))
        }

        #[test]
        fn a_client_that_stops_reading_is_disconnected() {
            // The failure the reader prevents, reproduced.
            if let Some((dropped, _)) = flood(false) {
                assert!(dropped, "the flood should have filled the socket");
            }
        }

        #[test]
        fn the_reader_keeps_a_blocked_client_connected() {
            if let Some((dropped, alive)) = flood(true) {
                assert!(!dropped, "the compositor dropped the client");
                assert!(alive, "the client didn't survive the flood");
            }
        }
    }
}
