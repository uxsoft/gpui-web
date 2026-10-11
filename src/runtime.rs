//! CEF's process-wide state: the subprocess entry point, lazy
//! initialization, the external message pump on GPUI's executor, profiles
//! and shutdown.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use cef::args::Args;
use cef::*;
use futures::FutureExt as _;
use gpui::{App, WindowBackgroundAppearance};

/// How often CEF gets to run while browsers exist, whatever it asked for.
const SAFETY_TICK: Duration = Duration::from_millis(30);
/// And while it runs without browsers: CEF doesn't always ask for work
/// while it creates the first one.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// How the app's DevTools clients reach the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteDebugging {
    Off,
    /// `--remote-debugging-pipe` on fds 3 and 4 (Unix). Needs
    /// [`handle_subprocess`] to reserve them; see [`take_devtools_pipe`].
    Pipe,
    /// `--remote-debugging-port=0` on localhost; see [`devtools_url`].
    Port,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevel {
    Verbose,
    Info,
    #[default]
    Warning,
    Error,
    Disabled,
}

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    /// Where profiles, caches and CEF's own state live. Profile directories
    /// must be inside it.
    pub root_cache_dir: PathBuf,
    /// Where downloads are saved, without asking.
    pub download_dir: Option<PathBuf>,
    pub remote_debugging: RemoteDebugging,
    pub log_level: LogLevel,
    /// CEF's log; its standard error when `None`.
    pub log_file: Option<PathBuf>,
    /// Appended to the user agent, e.g. `Wisp/1.0`.
    pub user_agent_product: Option<String>,
}

/// How pages are shown, chosen once per process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Off-screen rendering into shared GPU textures.
    SharedTexture,
    /// Off-screen rendering into CPU buffers.
    Software,
    /// Chromium's own child window (Windows).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Windowed,
}

/// Runs a CEF subprocess (renderer, GPU, utility…) when this process is one,
/// returning its exit code. Call it first thing in `main`, before anything
/// else starts threads or opens files: in the browser process it reserves
/// fds 3 and 4 for the DevTools pipe when `devtools_pipe` is set.
pub fn handle_subprocess(devtools_pipe: bool) -> Option<i32> {
    #[cfg(target_os = "macos")]
    if !crate::platform::macos::load_framework() {
        eprintln!("gpui_web: couldn't load the Chromium Embedded Framework");
        return Some(1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = Args::new();
    let mut app = crate::browser::app();
    let code = execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
    if code >= 0 {
        return Some(code);
    }
    #[cfg(unix)]
    if devtools_pipe {
        crate::devtools::reserve_pipe();
    }
    #[cfg(not(unix))]
    let _ = devtools_pipe;
    None
}

/// The window background the presenters need: transparent where the page
/// is composited under the window.
pub fn window_background() -> WindowBackgroundAppearance {
    if crate::platform::hole_punch_expected() {
        WindowBackgroundAppearance::Transparent
    } else {
        WindowBackgroundAppearance::Opaque
    }
}

enum State {
    Idle,
    Running,
    Failed(String),
    Stopped,
}

struct Runtime {
    options: RuntimeOptions,
    state: State,
    /// Browsers alive (created and not yet closed).
    live: Rc<Cell<usize>>,
    mode: Mode,
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Where CEF's "do work" requests go, from any thread.
static PUMP: OnceLock<async_channel::Sender<i64>> = OnceLock::new();

pub(crate) fn schedule_pump(delay_ms: i64) {
    if let Some(pump) = PUMP.get() {
        pump.try_send(delay_ms).ok();
    }
}

/// Sets CEF up to start with the first web view, and starts the message
/// pump. Call once, from the main thread, at app start.
pub fn init(options: RuntimeOptions, cx: &mut App) {
    let (tx, rx) = async_channel::unbounded::<i64>();
    if PUMP.set(tx).is_err() {
        tracing::warn!("gpui_web::init called twice");
        return;
    }
    let live = Rc::new(Cell::new(0));
    RUNTIME.with_borrow_mut(|runtime| {
        *runtime = Some(Runtime {
            options,
            state: State::Idle,
            live: live.clone(),
            mode: crate::platform::mode(),
        })
    });
    cx.spawn(async move |cx| {
        let mut due: Option<Instant> = None;
        loop {
            let wait = match due {
                Some(due) => Some(due.saturating_duration_since(Instant::now())),
                None if live.get() > 0 => Some(SAFETY_TICK),
                None if is_running() => Some(IDLE_TICK),
                None => None,
            };
            let request = match wait {
                Some(wait) if wait.is_zero() => None,
                Some(wait) => {
                    let mut timer = std::pin::pin!(cx.background_executor().timer(wait).fuse());
                    let mut recv = std::pin::pin!(rx.recv().fuse());
                    futures::select! {
                        request = recv => match request {
                            Ok(delay) => Some(delay),
                            Err(_) => return,
                        },
                        _ = timer => None,
                    }
                }
                None => match rx.recv().await {
                    Ok(delay) => Some(delay),
                    Err(_) => return,
                },
            };
            if let Some(delay) = request {
                let at = Instant::now() + Duration::from_millis(delay.max(0) as u64);
                due = Some(due.map_or(at, |due| due.min(at)));
                // Coalesce whatever else arrived.
                while let Ok(delay) = rx.try_recv() {
                    let at = Instant::now() + Duration::from_millis(delay.max(0) as u64);
                    due = Some(due.map_or(at, |due| due.min(at)));
                }
                if due.is_some_and(|due| due > Instant::now()) {
                    continue;
                }
            }
            due = None;
            if is_running() {
                do_message_loop_work();
            }
        }
    })
    .detach();
}

pub(crate) fn is_running() -> bool {
    RUNTIME.with_borrow(|r| matches!(r.as_ref().map(|r| &r.state), Some(State::Running)))
}

pub(crate) fn mode() -> Mode {
    RUNTIME.with_borrow(|r| r.as_ref().map_or(crate::platform::mode(), |r| r.mode))
}

pub(crate) fn download_dir() -> Option<PathBuf> {
    RUNTIME.with_borrow(|r| r.as_ref().and_then(|r| r.options.download_dir.clone()))
}

pub(crate) fn browser_created() {
    RUNTIME.with_borrow(|r| {
        if let Some(r) = r {
            r.live.set(r.live.get() + 1);
        }
    });
    schedule_pump(0);
}

pub(crate) fn browser_closed() {
    RUNTIME.with_borrow(|r| {
        if let Some(r) = r {
            r.live.set(r.live.get().saturating_sub(1));
        }
    });
}

/// Starts CEF if it isn't running yet.
pub(crate) fn ensure_started() -> Result<(), String> {
    let options = RUNTIME.with_borrow(|r| match r {
        None => Err("gpui_web::init wasn't called".to_string()),
        Some(r) => match &r.state {
            State::Running => Ok(None),
            State::Failed(err) => Err(err.clone()),
            State::Stopped => Err("the browser engine has shut down".to_string()),
            State::Idle => Ok(Some((r.options.clone(), r.mode))),
        },
    })?;
    let Some((options, mode)) = options else {
        return Ok(());
    };
    let result = start(&options, mode);
    RUNTIME.with_borrow_mut(|r| {
        if let Some(r) = r {
            r.state = match &result {
                Ok(()) => State::Running,
                Err(err) => State::Failed(err.clone()),
            }
        }
    });
    if result.is_ok() {
        schedule_pump(0);
    }
    result
}

fn start(options: &RuntimeOptions, mode: Mode) -> Result<(), String> {
    std::fs::create_dir_all(&options.root_cache_dir)
        .map_err(|e| format!("creating {}: {e}", options.root_cache_dir.display()))?;
    let mut switches = crate::platform::switches(mode);
    match options.remote_debugging {
        RemoteDebugging::Off => {}
        RemoteDebugging::Pipe if crate::devtools::pipe_reserved() => {
            switches.push(("remote-debugging-pipe".into(), None));
        }
        RemoteDebugging::Pipe | RemoteDebugging::Port => {
            switches.push(("remote-debugging-port".into(), Some("0".into())));
        }
    }
    crate::browser::set_switches(switches);

    let args = Args::new();
    let mut app = crate::browser::app();
    let settings = Settings {
        no_sandbox: 1,
        external_message_pump: 1,
        windowless_rendering_enabled: 1,
        root_cache_path: path_string(&options.root_cache_dir),
        cache_path: path_string(&options.root_cache_dir.join("Default")),
        persist_session_cookies: 1,
        user_agent_product: options
            .user_agent_product
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        log_file: options.log_file.as_deref().map(path_string).unwrap_or_default(),
        log_severity: match options.log_level {
            LogLevel::Verbose => LogSeverity::VERBOSE,
            LogLevel::Info => LogSeverity::INFO,
            LogLevel::Warning => LogSeverity::WARNING,
            LogLevel::Error => LogSeverity::ERROR,
            LogLevel::Disabled => LogSeverity::DISABLE,
        },
        background_color: 0xffff_ffff,
        // Chromium's crash handlers would fight the host app's.
        disable_signal_handlers: 1,
        ..crate::platform::settings()
    };
    #[cfg(target_os = "macos")]
    crate::platform::macos::install_app_protocol();
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        return Err("Chromium failed to start (CefInitialize)".into());
    }
    tracing::info!("CEF started ({mode:?})");
    Ok(())
}

fn path_string(path: &Path) -> CefString {
    CefString::from(path.to_string_lossy().as_ref())
}

/// Closes every browser, letting each save its state, then stops CEF.
/// Blocks for up to `timeout`; call it when the app quits.
pub fn shutdown(timeout: Duration) {
    if !is_running() {
        return;
    }
    crate::browser::close_all();
    let deadline = Instant::now() + timeout;
    let live = RUNTIME.with_borrow(|r| r.as_ref().map(|r| r.live.clone()));
    while live.as_ref().is_some_and(|l| l.get() > 0) && Instant::now() < deadline {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    // A few more turns let CEF release what the closed browsers held.
    for _ in 0..10 {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(2));
    }
    crate::browser::forget_all();
    RUNTIME.with_borrow_mut(|r| {
        if let Some(r) = r {
            r.state = State::Stopped;
        }
    });
    cef::shutdown();
}

/// A browsing profile: cookies, storage and cache, kept in its own
/// directory under the runtime's `root_cache_dir`, or the runtime's default
/// profile.
#[derive(Clone)]
pub struct Profile(Rc<ProfileInner>);

struct ProfileInner {
    dir: Option<PathBuf>,
    context: RefCell<Option<RequestContext>>,
}

impl Profile {
    /// A profile kept in `dir`, which must be inside `root_cache_dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(Rc::new(ProfileInner {
            dir: Some(dir.into()),
            context: RefCell::new(None),
        }))
    }

    /// The runtime's default profile.
    pub fn default_profile() -> Self {
        Self(Rc::new(ProfileInner {
            dir: None,
            context: RefCell::new(None),
        }))
    }

    pub fn dir(&self) -> Option<&Path> {
        self.0.dir.as_deref()
    }

    pub(crate) fn context(&self) -> Option<RequestContext> {
        let dir = self.0.dir.as_ref()?;
        let mut context = self.0.context.borrow_mut();
        if context.is_none() {
            std::fs::create_dir_all(dir).ok();
            let settings = RequestContextSettings {
                cache_path: path_string(dir),
                persist_session_cookies: 1,
                ..Default::default()
            };
            *context = request_context_create_context(Some(&settings), None);
        }
        context.clone()
    }
}

impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
