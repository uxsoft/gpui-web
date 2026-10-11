//! One page's state, shared between its [`crate::WebView`] and the CEF
//! handlers that report on it, and the handlers themselves.
//!
//! With the external message pump, CEF calls these handlers on the main
//! thread from inside `do_message_loop_work`, outside any GPUI update. They
//! record what happened in the [`Page`] and wake the web view, which turns
//! it into events and redraws. Painting is the exception: a presenter puts
//! frames on screen from inside the paint callback, the only time a frame's
//! buffer is valid.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Mutex;
use std::thread::ThreadId;

use cef::*;
use gpui::{CursorStyle, Point};

use crate::platform::Presenter;
use crate::runtime::{self, Mode, Profile};

/// An `Rc` that CEF may carry to other threads: it may only be used on the
/// thread that made it, and is leaked rather than freed elsewhere.
pub(crate) struct Local<T> {
    rc: ManuallyDrop<Rc<T>>,
    thread: ThreadId,
}

// SAFETY: every access and every clone or drop of the inner `Rc` happens on
// `thread` (checked); a `Local` dropped elsewhere leaks its reference.
unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

impl<T> Local<T> {
    pub fn new(value: T) -> Self {
        Self {
            rc: ManuallyDrop::new(Rc::new(value)),
            thread: std::thread::current().id(),
        }
    }

    fn on_own_thread(&self) -> bool {
        std::thread::current().id() == self.thread
    }

    pub fn downgrade(&self) -> Weak<T> {
        assert!(self.on_own_thread(), "gpui_web page used off the main thread");
        Rc::downgrade(&self.rc)
    }

}

impl<T> Clone for Local<T> {
    fn clone(&self) -> Self {
        assert!(self.on_own_thread(), "gpui_web page cloned off the main thread");
        Self {
            rc: self.rc.clone(),
            thread: self.thread,
        }
    }
}

impl<T> Deref for Local<T> {
    type Target = T;
    fn deref(&self) -> &T {
        assert!(self.on_own_thread(), "gpui_web page used off the main thread");
        &self.rc
    }
}

impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if self.on_own_thread() {
            unsafe { ManuallyDrop::drop(&mut self.rc) }
        } else {
            tracing::warn!("gpui_web: page reference released off the main thread; leaking it");
        }
    }
}

pub(crate) type PageRef = Local<RefCell<Page>>;

/// The page's size and the scale it renders at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    /// Logical (GPUI) pixels.
    pub width: f32,
    pub height: f32,
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// Work around CEF ignoring `device_scale_factor` (see
    /// docs/webview-spikes.md): the view is sized in device pixels at scale
    /// 1, and page zoom brings CSS pixels back to logical size.
    pub device_rect: bool,
}

impl Geometry {
    /// The view rect CEF lays the page out in.
    pub fn view_size(&self) -> (i32, i32) {
        let k = if self.device_rect { self.scale } else { 1. };
        (
            ((self.width * k).round() as i32).max(1),
            ((self.height * k).round() as i32).max(1),
        )
    }

    pub fn device_scale_factor(&self) -> f32 {
        if self.device_rect { 1. } else { self.scale }
    }

    /// The page zoom level that keeps one CSS pixel at one logical pixel.
    pub fn zoom_level(&self) -> f64 {
        if self.device_rect && self.scale > 0. {
            (self.scale as f64).ln() / 1.2f64.ln()
        } else {
            0.
        }
    }

    /// View coordinates of a point in logical pixels relative to the page.
    pub fn to_view(&self, x: f32, y: f32) -> (i32, i32) {
        let k = if self.device_rect { self.scale } else { 1. };
        ((x * k).round() as i32, (y * k).round() as i32)
    }

    /// Logical pixels relative to the page of a point in view coordinates.
    pub fn from_view(&self, x: i32, y: i32) -> (f32, f32) {
        let k = if self.device_rect { self.scale } else { 1. };
        (x as f32 / k, y as f32 / k)
    }
}

/// A JavaScript dialog the page opened: `alert`, `confirm`, `prompt` or the
/// "leave this page?" check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
}

#[derive(Clone, Debug)]
pub struct JsDialog {
    pub kind: DialogKind,
    pub message: String,
    pub default_text: String,
}

/// A file input asking for files.
#[derive(Clone, Debug)]
pub struct FileChooser {
    pub multiple: bool,
    pub folder: bool,
    pub save: bool,
    pub default_path: Option<PathBuf>,
}

/// What was under the pointer when the page asked for a context menu.
#[derive(Clone, Debug, Default)]
pub struct ContextMenu {
    /// Where, in logical pixels relative to the page.
    pub position: Point<gpui::Pixels>,
    pub link_url: Option<String>,
    pub selection_text: String,
    pub editable: bool,
    pub can_cut: bool,
    pub can_copy: bool,
    pub can_paste: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub can_select_all: bool,
    pub media_url: Option<String>,
}

/// How a page asked to open another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// `window.open` with features, or a popup.
    Popup,
    /// A link or `window.open` meant for a new tab (Ctrl-click, `target=_blank`).
    Tab,
    BackgroundTab,
    Window,
    Other,
}

pub(crate) enum PageEvent {
    Title,
    Url,
    Loading,
    Favicon,
    Popup {
        page: PageRef,
        url: String,
        disposition: Disposition,
        user_gesture: bool,
    },
    JsDialog(JsDialog),
    JsDialogClosed,
    FileChooser(FileChooser),
    ContextMenu(ContextMenu),
    DownloadStarted { name: String, path: PathBuf },
    DownloadFinished { name: String, path: PathBuf, ok: bool },
    Crashed,
    Ready,
    Closed,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Keystroke(gpui::Keystroke),
}

#[derive(Default)]
pub(crate) struct Ime {
    /// Bounds of the composition's characters, in view coordinates.
    pub character_bounds: Vec<Rect>,
    pub composing: Option<std::ops::Range<usize>>,
}

pub(crate) struct Page {
    pub browser: Option<Browser>,
    pub profile: Profile,
    pub geometry: Geometry,
    pub presenter: Option<Box<dyn Presenter>>,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub favicon: Option<String>,
    pub cursor: CursorStyle,
    pub ime: Ime,
    pub js_dialog: Option<JsdialogCallback>,
    pub file_dialog: Option<FileDialogCallback>,
    pub events: Vec<PageEvent>,
    /// Wakes the web view; `None` until it has one (popups).
    pub notify: Option<async_channel::Sender<()>>,
    pub notify_pending: bool,
    /// The DevTools target id, once known.
    pub target_id: Option<String>,
    pub closed: bool,
    /// The page that opened this one, until it learns of it.
    pub opener: Option<Weak<RefCell<Page>>>,
    pub popup_info: Option<(String, Disposition, bool)>,
    pub downloads: HashSet<u32>,
    /// Keystrokes the app binds, which the page shouldn't swallow (windowed
    /// mode, where Chromium's window has keyboard focus).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub app_keystrokes: Vec<gpui::Keystroke>,
    /// Kept alive while the page is: the DevTools observer's registration.
    pub registrations: Vec<Registration>,
    /// Pending DevTools calls: message id → reply.
    pub devtools_calls: Vec<(i32, futures::channel::oneshot::Sender<Result<String, String>>)>,
    pub next_devtools_id: i32,
    pub frame_rate: i32,
}

impl Page {
    pub fn new(profile: Profile) -> Self {
        Self {
            browser: None,
            profile,
            geometry: Geometry {
                width: 1280.,
                height: 800.,
                scale: 1.,
                device_rect: crate::platform::device_rect_default(),
            },
            presenter: None,
            url: String::new(),
            title: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            favicon: None,
            cursor: CursorStyle::Arrow,
            ime: Ime::default(),
            js_dialog: None,
            file_dialog: None,
            events: Vec::new(),
            notify: None,
            notify_pending: false,
            target_id: None,
            closed: false,
            opener: None,
            popup_info: None,
            downloads: HashSet::new(),
            app_keystrokes: Vec::new(),
            registrations: Vec::new(),
            devtools_calls: Vec::new(),
            next_devtools_id: 1000,
            frame_rate: 60,
        }
    }

    pub fn host(&self) -> Option<BrowserHost> {
        self.browser.as_ref()?.host()
    }

    /// Queues `event` and wakes the web view.
    pub fn push(&mut self, event: PageEvent) {
        self.events.push(event);
        self.wake();
    }

    /// Wakes the web view to redraw or read the page's state.
    pub fn wake(&mut self) {
        if self.notify_pending {
            return;
        }
        if let Some(notify) = &self.notify {
            self.notify_pending = notify.try_send(()).is_ok();
        }
    }
}

thread_local! {
    /// Every page, for closing them all at shutdown.
    static PAGES: RefCell<Vec<Weak<RefCell<Page>>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn new_page(profile: Profile) -> PageRef {
    let page = Local::new(RefCell::new(Page::new(profile)));
    PAGES.with_borrow_mut(|pages| {
        pages.retain(|p| p.strong_count() > 0);
        pages.push(page.downgrade());
    });
    page
}

/// Closes every open browser.
pub(crate) fn close_all() {
    let hosts: Vec<BrowserHost> = PAGES.with_borrow(|pages| {
        pages
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|p| p.try_borrow().ok().and_then(|p| p.host()))
            .collect()
    });
    for host in hosts {
        host.close_browser(1);
    }
}

/// Drops every reference to CEF objects, before CEF shuts down.
pub(crate) fn forget_all() {
    let pages: Vec<Rc<RefCell<Page>>> =
        PAGES.with_borrow_mut(|pages| pages.drain(..).filter_map(|p| p.upgrade()).collect());
    for page in pages {
        if let Ok(mut page) = page.try_borrow_mut() {
            page.browser = None;
            page.presenter = None;
            page.js_dialog = None;
            page.file_dialog = None;
            page.registrations.clear();
        }
    }
}

/// Command-line switches for the browser process, set before CEF starts.
static SWITCHES: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

pub(crate) fn set_switches(switches: Vec<(String, Option<String>)>) {
    *SWITCHES.lock().unwrap_or_else(|e| e.into_inner()) = switches;
}

wrap_app! {
    struct WebApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Only the browser process; its children inherit what they need.
            if process_type.is_some_and(|t| !t.to_string().is_empty()) {
                return;
            }
            let Some(command_line) = command_line else { return };
            for (name, value) in SWITCHES.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                match value {
                    Some(value) => command_line.append_switch_with_value(
                        Some(&name.as_str().into()),
                        Some(&value.as_str().into()),
                    ),
                    None => command_line.append_switch(Some(&name.as_str().into())),
                }
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(WebBrowserProcess::new())
        }
    }
}

pub(crate) fn app() -> cef::App {
    WebApp::new()
}

wrap_browser_process_handler! {
    struct WebBrowserProcess;

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            runtime::schedule_pump(delay_ms);
        }
    }
}

/// Creates the browser for `page`, showing `url`.
pub(crate) fn create_browser(page: &PageRef, url: &str) -> Result<(), String> {
    runtime::ensure_started()?;
    let profile = page.borrow().profile.clone();
    let mut context = profile.context();
    let mut window_info = window_info(runtime::mode());
    let settings = browser_settings(page.borrow().frame_rate);
    let mut client = client(page);
    // Asynchronous creation: `on_after_created` hands the browser over.
    #[cfg(target_os = "windows")]
    if runtime::mode() == Mode::Windowed {
        window_info = crate::platform::windows::child_window_info(window_info);
    }
    let created = browser_host_create_browser(
        Some(&mut window_info),
        Some(&mut client),
        Some(&url.into()),
        Some(&settings),
        None,
        context.as_mut(),
    );
    if created != 1 {
        return Err("Chromium couldn't create the page".into());
    }
    Ok(())
}

fn window_info(mode: Mode) -> WindowInfo {
    let info = WindowInfo::default();
    match mode {
        Mode::SharedTexture | Mode::Software => WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: (mode == Mode::SharedTexture) as i32,
            runtime_style: RuntimeStyle::ALLOY,
            ..info
        },
        Mode::Windowed => WindowInfo {
            runtime_style: RuntimeStyle::ALLOY,
            ..info
        },
    }
}

fn browser_settings(frame_rate: i32) -> BrowserSettings {
    BrowserSettings {
        windowless_frame_rate: frame_rate,
        background_color: 0xffff_ffff,
        ..Default::default()
    }
}

fn client(page: &PageRef) -> Client {
    WebClient::new(
        WebRender::new(page.clone()),
        WebDisplay::new(page.clone()),
        WebLoad::new(page.clone()),
        WebLifeSpan::new(page.clone()),
        WebRequest::new(page.clone()),
        WebContextMenu::new(page.clone()),
        WebJsDialog::new(page.clone()),
        WebDialog::new(page.clone()),
        WebDownload::new(page.clone()),
        keyboard_handler(page),
    )
}

wrap_client! {
    struct WebClient {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        request: RequestHandler,
        context_menu: ContextMenuHandler,
        js_dialog: JsdialogHandler,
        dialog: DialogHandler,
        download: DownloadHandler,
        keyboard: Option<KeyboardHandler>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu.clone())
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.js_dialog.clone())
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.dialog.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download.clone())
        }
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            self.keyboard.clone()
        }
    }
}

/// Runs `f` on the page unless something already holds it (a handler called
/// back from inside a call into CEF that holds the page).
fn with_page<R>(page: &PageRef, f: impl FnOnce(&mut Page) -> R) -> Option<R> {
    match page.try_borrow_mut() {
        Ok(mut page) => Some(f(&mut page)),
        Err(_) => {
            tracing::warn!("gpui_web: CEF called back while the page was busy");
            None
        }
    }
}

fn cef_string(s: Option<&CefString>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

fn userfree(s: &CefStringUserfree) -> String {
    CefString::from(s).to_string()
}

wrap_render_handler! {
    struct WebRender {
        page: PageRef,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (width, height) = with_page(&self.page, |p| p.geometry.view_size()).unwrap_or((1, 1));
            *rect = Rect { x: 0, y: 0, width, height };
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            let Some(info) = info else { return 0 };
            let Some(geometry) = with_page(&self.page, |p| p.geometry) else { return 0 };
            let (width, height) = geometry.view_size();
            info.device_scale_factor = geometry.device_scale_factor();
            info.depth = 24;
            info.depth_per_component = 8;
            info.rect = Rect { x: 0, y: 0, width, height };
            info.available_rect = info.rect.clone();
            1
        }

        fn on_popup_show(&self, browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            with_page(&self.page, |p| {
                if let Some(presenter) = &mut p.presenter {
                    presenter.on_popup_show(show != 0);
                }
            });
            if show == 0
                && let Some(host) = browser.and_then(|b| b.host())
            {
                host.invalidate(PaintElementType::VIEW);
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            with_page(&self.page, |p| {
                if let Some(presenter) = &mut p.presenter {
                    presenter.on_popup_size(rect.clone());
                }
            });
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            // SAFETY: CEF hands over a BGRA buffer of width × height pixels,
            // valid for the duration of the call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, width as usize * height as usize * 4) };
            with_page(&self.page, |p| {
                let redraw = p.presenter.as_mut().is_some_and(|presenter| {
                    presenter.on_paint(type_, dirty_rects.unwrap_or_default(), pixels, width, height)
                });
                if redraw {
                    p.wake();
                }
            });
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            with_page(&self.page, |p| {
                let redraw = p
                    .presenter
                    .as_mut()
                    .is_some_and(|presenter| presenter.on_accelerated_paint(type_, info));
                if redraw {
                    p.wake();
                }
            });
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            selected_range: Option<&cef::Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            with_page(&self.page, |p| {
                p.ime.character_bounds = character_bounds.map(<[Rect]>::to_vec).unwrap_or_default();
                if let (Some(composing), Some(range)) = (&mut p.ime.composing, selected_range) {
                    composing.end = composing.start.max(range.to as usize);
                }
            });
        }

        fn start_dragging(
            &self,
            _browser: Option<&mut Browser>,
            _drag_data: Option<&mut DragData>,
            _allowed_ops: DragOperationsMask,
            _x: ::std::os::raw::c_int,
            _y: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // Dragging out of the page isn't supported yet; CEF cancels it.
            0
        }
    }
}

wrap_display_handler! {
    struct WebDisplay {
        page: PageRef,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if !frame.is_some_and(|f| f.is_main() != 0) {
                return;
            }
            let url = cef_string(url);
            with_page(&self.page, |p| {
                if p.url != url {
                    p.url = url;
                    p.push(PageEvent::Url);
                }
            });
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = cef_string(title);
            with_page(&self.page, |p| {
                if p.title != title {
                    p.title = title;
                    p.push(PageEvent::Title);
                }
            });
        }

        fn on_favicon_urlchange(&self, _browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            let favicon = icon_urls.and_then(|urls| urls.clone().into_iter().next());
            with_page(&self.page, |p| {
                if p.favicon != favicon {
                    p.favicon = favicon;
                    p.push(PageEvent::Favicon);
                }
            });
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: ::std::os::raw::c_ulong,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            let cursor = crate::input::cursor_style(type_);
            with_page(&self.page, |p| {
                if p.cursor != cursor {
                    p.cursor = cursor;
                    p.wake();
                }
            });
            1
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            tracing::trace!("page console: {} ({}:{line})", cef_string(message), cef_string(source));
            1
        }
    }
}

wrap_load_handler! {
    struct WebLoad {
        page: PageRef,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            with_page(&self.page, |p| {
                p.loading = is_loading != 0;
                p.can_go_back = can_go_back != 0;
                p.can_go_forward = can_go_forward != 0;
                p.push(PageEvent::Loading);
            });
        }

        fn on_load_start(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            if frame.is_some_and(|f| f.is_main() != 0) {
                // Page zoom is per site; each navigation may reset it.
                let zoom = with_page(&self.page, |p| p.geometry.zoom_level());
                if let (Some(zoom), Some(host)) = (zoom, browser.and_then(|b| b.host())) {
                    apply_zoom(&host, zoom);
                }
            }
        }
    }
}

/// Sets the page zoom when it differs.
pub(crate) fn apply_zoom(host: &BrowserHost, zoom: f64) {
    if (host.zoom_level() - zoom).abs() > 1e-4 {
        host.set_zoom_level(zoom);
    }
}

wrap_life_span_handler! {
    struct WebLifeSpan {
        page: PageRef,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // The popup becomes a page of its own, in the same profile; the
            // app learns of it once Chromium has created it.
            let Some(profile) = with_page(&self.page, |p| p.profile.clone()) else { return 1 };
            let popup = new_page(profile);
            {
                let mut p = popup.borrow_mut();
                p.opener = Some(self.page.downgrade());
                p.popup_info = Some((
                    cef_string(target_url),
                    disposition(target_disposition),
                    user_gesture != 0,
                ));
                if let Some(geometry) = with_page(&self.page, |p| p.geometry) {
                    p.geometry = geometry;
                }
            }
            if let Some(window_info) = window_info {
                let mode = runtime::mode();
                *window_info = self::window_info(mode);
                #[cfg(target_os = "windows")]
                if mode == Mode::Windowed {
                    *window_info = crate::platform::windows::child_window_info(window_info.clone());
                }
            }
            if let Some(client) = client {
                *client = Some(self::client(&popup));
            }
            if let Some(settings) = settings {
                *settings = browser_settings(60);
            }
            0
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let browser = browser.clone();
            let opener = with_page(&self.page, |p| {
                p.browser = Some(browser.clone());
                p.opener.take()
            })
            .flatten();
            runtime::browser_created();
            if let Some(host) = browser.host() {
                let mut observer = WebDevTools::new(self.page.clone());
                if let Some(registration) = host.add_dev_tools_message_observer(Some(&mut observer)) {
                    with_page(&self.page, |p| p.registrations.push(registration));
                }
                host.send_dev_tools_message(Some(TARGET_INFO_REQUEST.as_bytes()));
                let zoom = with_page(&self.page, |p| p.geometry.zoom_level()).unwrap_or(0.);
                apply_zoom(&host, zoom);
            }
            match opener.and_then(|o| o.upgrade()) {
                Some(opener) => {
                    let info = with_page(&self.page, |p| p.popup_info.take()).flatten();
                    let (url, disposition, user_gesture) =
                        info.unwrap_or((String::new(), Disposition::Other, false));
                    if let Ok(mut opener) = opener.try_borrow_mut() {
                        opener.push(PageEvent::Popup {
                            page: self.page.clone(),
                            url,
                            disposition,
                            user_gesture,
                        });
                    }
                }
                None => {
                    with_page(&self.page, |p| p.wake());
                }
            }
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            0
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            runtime::browser_closed();
            with_page(&self.page, |p| {
                p.browser = None;
                p.closed = true;
                p.presenter = None;
                p.registrations.clear();
                p.js_dialog = None;
                p.file_dialog = None;
                for (_, reply) in p.devtools_calls.drain(..) {
                    reply.send(Err("the page closed".into())).ok();
                }
                p.push(PageEvent::Closed);
            });
        }
    }
}

fn disposition(d: WindowOpenDisposition) -> Disposition {
    use cef::sys::cef_window_open_disposition_t as D;
    match d.into() {
        D::CEF_WOD_NEW_POPUP => Disposition::Popup,
        D::CEF_WOD_NEW_FOREGROUND_TAB => Disposition::Tab,
        D::CEF_WOD_NEW_BACKGROUND_TAB => Disposition::BackgroundTab,
        D::CEF_WOD_NEW_WINDOW => Disposition::Window,
        _ => Disposition::Other,
    }
}

/// Asks a page for its own DevTools target; see [`WebDevTools`].
const TARGET_INFO_ID: i32 = 1;
const TARGET_INFO_REQUEST: &str = r#"{"id":1,"method":"Target.getTargetInfo"}"#;

wrap_dev_tools_message_observer! {
    struct WebDevTools {
        page: PageRef,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            let text = String::from_utf8_lossy(result.unwrap_or_default()).into_owned();
            with_page(&self.page, |p| {
                if message_id == TARGET_INFO_ID {
                    if let Some(id) = target_id_of(&text) {
                        p.target_id = Some(id);
                        p.push(PageEvent::Ready);
                    }
                    return;
                }
                if let Some(ix) = p.devtools_calls.iter().position(|(id, _)| *id == message_id) {
                    let (_, reply) = p.devtools_calls.remove(ix);
                    reply.send(if success != 0 { Ok(text) } else { Err(text) }).ok();
                }
            });
        }
    }
}

/// The `targetId` in a `Target.getTargetInfo` result.
fn target_id_of(result: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(result).ok()?;
    Some(value.pointer("/targetInfo/targetId")?.as_str()?.to_string())
}

wrap_request_handler! {
    struct WebRequest {
        page: PageRef,
    }

    impl RequestHandler {
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            tracing::warn!("page's renderer ended: {status:?}");
            with_page(&self.page, |p| p.push(PageEvent::Crashed));
        }
    }
}

wrap_context_menu_handler! {
    struct WebContextMenu {
        page: PageRef,
    }

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            // The app shows its own menu, drawn by GPUI over the page.
            if let Some(model) = model {
                model.clear();
            }
            let Some(params) = params else { return };
            use cef::sys::cef_context_menu_edit_state_flags_t as E;
            let flags: E = params.edit_state_flags().into();
            let has = |flag: E| flags.0 & flag.0 != 0;
            let link = userfree(&params.link_url());
            let media = userfree(&params.source_url());
            let (x, y) = (params.xcoord(), params.ycoord());
            with_page(&self.page, |p| {
                let (x, y) = p.geometry.from_view(x, y);
                p.push(PageEvent::ContextMenu(ContextMenu {
                    position: gpui::point(gpui::px(x), gpui::px(y)),
                    link_url: (!link.is_empty()).then_some(link),
                    selection_text: userfree(&params.selection_text()),
                    editable: params.is_editable() != 0,
                    can_cut: has(E::CM_EDITFLAG_CAN_CUT),
                    can_copy: has(E::CM_EDITFLAG_CAN_COPY),
                    can_paste: has(E::CM_EDITFLAG_CAN_PASTE),
                    can_undo: has(E::CM_EDITFLAG_CAN_UNDO),
                    can_redo: has(E::CM_EDITFLAG_CAN_REDO),
                    can_select_all: has(E::CM_EDITFLAG_CAN_SELECT_ALL),
                    media_url: (!media.is_empty()).then_some(media),
                }));
            });
        }
    }
}

wrap_jsdialog_handler! {
    struct WebJsDialog {
        page: PageRef,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_jsdialog_type_t as T;
            let Some(callback) = callback else { return 0 };
            let kind = match dialog_type.into() {
                T::JSDIALOGTYPE_CONFIRM => DialogKind::Confirm,
                T::JSDIALOGTYPE_PROMPT => DialogKind::Prompt,
                _ => DialogKind::Alert,
            };
            let dialog = JsDialog {
                kind,
                message: cef_string(message_text),
                default_text: cef_string(default_prompt_text),
            };
            with_page(&self.page, |p| {
                p.js_dialog = Some(callback.clone());
                p.push(PageEvent::JsDialog(dialog));
            })
            .map_or(0, |()| 1)
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            // Leaving a page is always allowed: tabs close without asking.
            if let Some(callback) = callback {
                callback.cont(1, None);
            }
            1
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            with_page(&self.page, |p| {
                if p.js_dialog.take().is_some() {
                    p.push(PageEvent::JsDialogClosed);
                }
            });
        }
    }
}

wrap_dialog_handler! {
    struct WebDialog {
        page: PageRef,
    }

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            _title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_file_dialog_mode_t as M;
            let Some(callback) = callback else { return 0 };
            let mode: M = mode.into();
            let default_path = cef_string(default_file_path);
            let chooser = FileChooser {
                multiple: mode == M::FILE_DIALOG_OPEN_MULTIPLE,
                folder: mode == M::FILE_DIALOG_OPEN_FOLDER,
                save: mode == M::FILE_DIALOG_SAVE,
                default_path: (!default_path.is_empty()).then(|| PathBuf::from(default_path)),
            };
            with_page(&self.page, |p| {
                if let Some(previous) = p.file_dialog.replace(callback.clone()) {
                    previous.cancel();
                }
                p.push(PageEvent::FileChooser(chooser));
            })
            .map_or(0, |()| 1)
        }
    }
}

wrap_download_handler! {
    struct WebDownload {
        page: PageRef,
    }

    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            runtime::download_dir().is_some() as i32
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(item), Some(callback), Some(dir)) = (download_item, callback, runtime::download_dir()) else {
                return 0;
            };
            let name = cef_string(suggested_name);
            let path = unique_path(&dir, if name.is_empty() { "download" } else { &name });
            callback.cont(Some(&path.to_string_lossy().as_ref().into()), 0);
            let id = item.id();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
            with_page(&self.page, |p| {
                p.downloads.insert(id);
                p.push(PageEvent::DownloadStarted { name, path });
            });
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let complete = item.is_complete() != 0;
            if !complete && item.is_canceled() == 0 && item.is_interrupted() == 0 {
                return;
            }
            let path = PathBuf::from(userfree(&item.full_path()));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let id = item.id();
            with_page(&self.page, |p| {
                if p.downloads.remove(&id) {
                    p.push(PageEvent::DownloadFinished { name, path, ok: complete });
                }
            });
        }
    }
}

/// `dir/name`, numbered when a file of that name exists.
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let name = name.replace(['/', '\\'], "_");
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

/// Windowed pages (Windows) have keyboard focus of their own; the handler
/// gives the app its shortcuts back.
#[cfg(target_os = "windows")]
fn keyboard_handler(page: &PageRef) -> Option<KeyboardHandler> {
    Some(WebKeyboard::new(page.clone()))
}

#[cfg(not(target_os = "windows"))]
fn keyboard_handler(_page: &PageRef) -> Option<KeyboardHandler> {
    None
}

#[cfg(target_os = "windows")]
wrap_keyboard_handler! {
    struct WebKeyboard {
        page: PageRef,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: Option<&mut MSG>,
            is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // Only windowed pages have keyboard focus of their own; give the
            // app its shortcuts back.
            if runtime::mode() != Mode::Windowed {
                return 0;
            }
            let Some(event) = event else { return 0 };
            let Some(keystroke) = crate::input::keystroke_of(event) else { return 0 };
            let ours = with_page(&self.page, |p| {
                let ours = p.app_keystrokes.iter().any(|k| crate::input::same_keystroke(k, &keystroke));
                if ours {
                    p.push(PageEvent::Keystroke(keystroke.clone()));
                }
                ours
            })
            .unwrap_or(false);
            if ours {
                if let Some(shortcut) = is_keyboard_shortcut {
                    *shortcut = 1;
                }
                return 1;
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_rect_geometry_keeps_css_pixels_logical() {
        let g = Geometry { width: 800., height: 600., scale: 1.5, device_rect: true };
        assert_eq!(g.view_size(), (1200, 900));
        assert_eq!(g.device_scale_factor(), 1.);
        assert!((1.2f64.powf(g.zoom_level()) - 1.5).abs() < 1e-9);
        assert_eq!(g.to_view(10., 20.), (15, 30));
        assert_eq!(g.from_view(15, 30), (10., 20.));
        let n = Geometry { device_rect: false, ..g };
        assert_eq!(n.view_size(), (800, 600));
        assert_eq!(n.device_scale_factor(), 1.5);
        assert_eq!(n.zoom_level(), 0.);
    }

    #[test]
    fn downloads_get_unique_names() {
        let dir = std::env::temp_dir().join(format!("gpui_web_dl_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        assert_eq!(unique_path(&dir, "x/y"), dir.join("x_y"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn target_ids_come_from_target_info() {
        let text = r#"{"targetInfo":{"targetId":"ABC","type":"page"}}"#;
        assert_eq!(target_id_of(text).as_deref(), Some("ABC"));
        assert_eq!(target_id_of("{}"), None);
    }
}
