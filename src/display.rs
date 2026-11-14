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
//!
//! Our threads use GPUI's `wl_display` without owning it, so they must end
//! before GPUI frees it: [`stop_reading_display`], when the app quits.

use gpui::Window;

/// Keeps reading `window`'s Wayland connection on a thread of its own, so
/// a blocked main thread can't get the app disconnected. Call when the
/// app's first window opens, before the window gets input: CEF's start
/// blocks the main thread. Once per process is enough (later calls do
/// nothing); a no-op off Wayland. Pair with [`stop_reading_display`].
pub fn keep_reading_display(window: &Window) {
    #[cfg(target_os = "linux")]
    wayland::keep_reading(window);
    #[cfg(not(target_os = "linux"))]
    let _ = window;
}

/// Stops the threads reading the app's Wayland connection, gpui-web's
/// presenters' among them; call when the app quits, while its windows
/// still exist. Returns why GPUI's own Wayland connection is lost, if it
/// is: a protocol error, or the compositor closing it. Errors of other
/// connections in the process (a clipboard's, say) don't count.
pub fn stop_reading_display() -> Option<String> {
    #[cfg(target_os = "linux")]
    return wayland::stop_all();
    #[cfg(not(target_os = "linux"))]
    None
}

#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "cef"), allow(unused_imports))]
pub(crate) use wayland::{Reader, register};

#[cfg(target_os = "linux")]
mod wayland {
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::JoinHandle;

    use anyhow::{Result, bail};
    use gpui::Window;
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    use wayland_backend::client::{Backend, WaylandError};
    use wayland_client::{Connection, EventQueue};

    /// How long a reader waits for events before it checks whether to stop.
    const POLL_MS: i32 = 100;

    /// The readers of GPUI's connection, for [`stop_all`].
    static READERS: Mutex<Vec<Reader>> = Mutex::new(Vec::new());

    pub(super) fn keep_reading(window: &Window) {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let display = match window.display_handle().map(|h| h.as_raw()) {
                Ok(RawDisplayHandle::Wayland(h)) => h.display.as_ptr(),
                _ => return,
            };
            match unsafe { spawn_reader(display) } {
                Ok(reader) => register(reader),
                Err(err) => tracing::warn!("Wayland reader: {err:#}"),
            }
        });
    }

    /// Has [`stop_reading_display`](super::stop_reading_display) stop
    /// `reader`.
    pub(crate) fn register(reader: Reader) {
        READERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(reader);
    }

    pub(super) fn stop_all() -> Option<String> {
        let readers = std::mem::take(&mut *READERS.lock().unwrap_or_else(|e| e.into_inner()));
        for reader in &readers {
            reader.stop.store(true, Ordering::SeqCst);
        }
        readers.into_iter().fold(None, |lost, r| lost.or(r.stop()))
    }

    /// Starts a thread reading the connection of `display` (a live
    /// `wl_display`) until it fails or is stopped.
    pub(super) unsafe fn spawn_reader(display: *mut std::ffi::c_void) -> Result<Reader> {
        if display.is_null() {
            bail!("no wl_display");
        }
        // Its own (empty) queue: reading moves every event to the queue of
        // the object it is for, ours stays empty.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let queue = conn.new_event_queue::<()>();
        Ok(Reader::spawn("gpui-web-wayland-reader", &conn, queue, ())?)
    }

    /// A thread dispatching a queue of a Wayland connection, which reads the
    /// connection for every queue on it.
    pub(crate) struct Reader {
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
        conn: Connection,
    }

    impl Reader {
        pub(crate) fn spawn<S: Send + 'static>(
            name: &str,
            conn: &Connection,
            mut queue: EventQueue<S>,
            mut state: S,
        ) -> std::io::Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let thread = std::thread::Builder::new().name(name.into()).spawn({
                let stop = stop.clone();
                move || {
                    if let Err(err) = read_until_stopped(&mut queue, &mut state, &stop) {
                        tracing::warn!("Wayland reader: {err}");
                    }
                }
            })?;
            Ok(Self {
                stop,
                thread,
                conn: conn.clone(),
            })
        }

        /// Ends the thread; returns why the connection is lost, if it is.
        pub(crate) fn stop(self) -> Option<String> {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.thread.join();
            // A socket the compositor closed shows only once read.
            if let Some(guard) = self.conn.prepare_read()
                && readable(guard.connection_fd(), 0)
            {
                let _ = guard.read();
            }
            // libwayland keeps a connection's fatal error, whichever queue
            // or thread hit it.
            match self.conn.flush() {
                Ok(()) => None,
                Err(WaylandError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => None,
                Err(err) => Some(err.to_string()),
            }
        }
    }

    fn read_until_stopped<S>(
        queue: &mut EventQueue<S>,
        state: &mut S,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        loop {
            queue.dispatch_pending(state).map_err(|e| e.to_string())?;
            ignore_would_block(queue.flush())?;
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            // None: events arrived for our queue; dispatch them first.
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            // Dropping the guard unprepared cancels the read.
            if readable(guard.connection_fd(), POLL_MS) {
                ignore_would_block(guard.read().map(drop))?;
            }
        }
    }

    /// Another reader may have read the events first.
    fn ignore_would_block(result: Result<(), WaylandError>) -> Result<(), String> {
        match result {
            Err(WaylandError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            other => other.map_err(|e| e.to_string()),
        }
    }

    /// Whether `fd` has something to read (or hung up) within `timeout_ms`.
    fn readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut poll, 1, timeout_ms) > 0 }
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use wayland_client::globals::{GlobalListContents, registry_queue_init};
        use wayland_client::protocol::{wl_output::WlOutput, wl_registry::WlRegistry};
        use wayland_client::{Dispatch, Proxy as _, QueueHandle, delegate_noop};
        use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
        use wayland_server::protocol::wl_output::{self, WlOutput as ServerOutput};
        use wayland_server::{DataInit, Display, GlobalDispatch, New, Resource as _};

        use super::*;

        /// Events the compositor sends a client that doesn't read them:
        /// 2.4 MB of `wl_output.mode`, far more than a socket holds.
        const BATCHES: usize = 1000;
        const PER_BATCH: usize = 100;

        #[derive(Default)]
        struct Server {
            outputs: Vec<ServerOutput>,
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
                state.outputs.push(init.init(resource, ()));
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

        /// A compositor in a thread, with one output global.
        struct TestServer {
            /// Per client: whether the compositor dropped it.
            dropped: Vec<Arc<AtomicBool>>,
            flooded: Arc<AtomicBool>,
            kill: mpsc::Sender<usize>,
            stop: Arc<AtomicBool>,
            thread: Option<JoinHandle<()>>,
        }

        impl TestServer {
            /// Serves `clients`; with `flood`, floods the first one's output.
            fn start(clients: Vec<UnixStream>, flood: bool) -> Self {
                let dropped: Vec<_> = clients
                    .iter()
                    .map(|_| Arc::new(AtomicBool::new(false)))
                    .collect();
                let flooded = Arc::new(AtomicBool::new(false));
                let stop = Arc::new(AtomicBool::new(false));
                let (kill, kills) = mpsc::channel::<usize>();
                // Closing these closes the connection as a compositor does.
                let sockets: Vec<UnixStream> =
                    clients.iter().map(|s| s.try_clone().unwrap()).collect();
                let thread = std::thread::spawn({
                    let (dropped, flooded, stop) = (dropped.clone(), flooded.clone(), stop.clone());
                    move || {
                        let mut display = Display::<Server>::new().unwrap();
                        let mut dh = display.handle();
                        dh.create_global::<Server, ServerOutput, ()>(4, ());
                        let ids: Vec<ClientId> = clients
                            .into_iter()
                            .zip(&dropped)
                            .map(|(stream, dropped)| {
                                dh.insert_client(stream, Arc::new(Watch(dropped.clone())))
                                    .unwrap()
                                    .id()
                            })
                            .collect();
                        let mut state = Server::default();
                        let mut batches = 0;
                        while !stop.load(Ordering::SeqCst) {
                            display.dispatch_clients(&mut state).ok();
                            while let Ok(i) = kills.try_recv() {
                                sockets[i].shutdown(std::net::Shutdown::Both).ok();
                            }
                            let first = state
                                .outputs
                                .iter()
                                .find(|o| o.client().is_some_and(|c| c.id() == ids[0]));
                            if flood
                                && let Some(output) = first
                                && batches < BATCHES
                                && !dropped[0].load(Ordering::SeqCst)
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
                Self {
                    dropped,
                    flooded,
                    kill,
                    stop,
                    thread: Some(thread),
                }
            }

            fn dropped(&self, client: usize) -> bool {
                self.dropped[client].load(Ordering::SeqCst)
            }

            /// Waits up to 20 s for `done`.
            fn wait(&self, done: impl Fn(&Self) -> bool) {
                let deadline = Instant::now() + Duration::from_secs(20);
                while !done(self) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }

        impl Drop for TestServer {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(thread) = self.thread.take() {
                    thread.join().unwrap();
                }
            }
        }

        /// A client socket pair; `None` without libwayland-client.
        fn pair() -> Option<(UnixStream, Backend)> {
            let (server_end, client_end) = UnixStream::pair().unwrap();
            match Backend::connect(client_end) {
                Ok(backend) => Some((server_end, backend)),
                Err(_) => {
                    eprintln!("skipped: no libwayland-client");
                    None
                }
            }
        }

        /// The app's side: `backend` with the output bound. Nothing
        /// dispatches the returned queue: the "main thread" is blocked.
        fn bind_output(backend: &Backend) -> (Connection, EventQueue<Client>, WlOutput) {
            let conn = Connection::from_backend(backend.clone());
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let output: WlOutput = globals.bind(&queue.handle(), 1..=4, ()).unwrap();
            queue.roundtrip(&mut Client).unwrap();
            (conn, queue, output)
        }

        fn reader_of(backend: &Backend) -> Reader {
            unsafe { spawn_reader(backend.display_ptr().cast()) }.unwrap()
        }

        /// Floods a client whose "main thread" never dispatches: whether the
        /// compositor dropped it, and whether it was connected and usable
        /// once the flood was over.
        fn flood(read_on_a_thread: bool) -> Option<(bool, bool)> {
            let (server_end, backend) = pair()?;
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
            let server = TestServer::start(vec![server_end], true);
            let (_conn, mut queue, output) = bind_output(&backend);
            let reader = read_on_a_thread.then(|| reader_of(&backend));
            server.wait(|s| s.flooded.load(Ordering::SeqCst) || s.dropped(0));
            let dropped = server.dropped(0);
            let alive = !dropped
                && server.flooded.load(Ordering::SeqCst)
                && queue.roundtrip(&mut Client).is_ok()
                && output.is_alive();
            if let Some(reader) = reader {
                assert_eq!(reader.stop(), None);
            }
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

        #[test]
        fn a_stopped_reader_ends_promptly_and_leaves_the_connection_working() {
            let Some((server_end, backend)) = pair() else {
                return;
            };
            let server = TestServer::start(vec![server_end], false);
            let (_conn, mut queue, _output) = bind_output(&backend);
            let reader = reader_of(&backend);
            std::thread::sleep(Duration::from_millis(50));
            let started = Instant::now();
            assert_eq!(reader.stop(), None);
            assert!(started.elapsed() < Duration::from_secs(1));
            // The app's own reading carries on without it.
            assert!(queue.roundtrip(&mut Client).is_ok());
            assert!(!server.dropped(0));
        }

        #[test]
        fn a_reader_reports_its_connection_closed() {
            let Some((server_end, backend)) = pair() else {
                return;
            };
            let server = TestServer::start(vec![server_end], false);
            let (_conn, _queue, _output) = bind_output(&backend);
            let reader = reader_of(&backend);
            server.kill.send(0).unwrap();
            server.wait(|s| s.dropped(0));
            std::thread::sleep(Duration::from_millis(50));
            let lost = reader.stop().expect("the connection was closed");
            assert!(lost.starts_with("Io error"), "{lost}");
        }

        #[test]
        fn another_connection_closing_is_not_reported() {
            // A clipboard's or portal's connection, next to GPUI's.
            let (Some((app_end, app)), Some((other_end, other))) = (pair(), pair()) else {
                return;
            };
            let server = TestServer::start(vec![app_end, other_end], false);
            let (_conn, _queue, _output) = bind_output(&app);
            let (_other_conn, mut other_queue, _other_output) = bind_output(&other);
            let reader = reader_of(&app);
            server.kill.send(1).unwrap();
            server.wait(|s| s.dropped(1));
            assert!(other_queue.roundtrip(&mut Client).is_err());
            assert_eq!(reader.stop(), None);
            assert!(!server.dropped(0));
        }
    }
}
