//! The [`WebView`] entity: one page, its state and its events.

use std::ops::Range;
use std::path::PathBuf;

use cef::*;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, MouseButton, ParentElement as _, Pixels, Render, Styled as _, Task,
    UTF16Selection, Window, div, px,
};

use crate::browser::{self, ContextMenu, Disposition, FileChooser, JsDialog, PageEvent, PageRef};
use crate::element::WebViewElement;
use crate::runtime::Profile;

/// On Linux the page's own clipboard can't reach the system's (off-screen,
/// Chromium has no window of its own to hold it), so copy and paste go
/// through GPUI, which has the user's key press.
const CLIPBOARD_THROUGH_GPUI: bool = cfg!(target_os = "linux");

/// Reads the selection, in a text field or the page.
const SELECTION_JS: &str = "(() => { const e = document.activeElement; \
if (e && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA') && typeof e.selectionStart === 'number') \
return e.value.substring(e.selectionStart, e.selectionEnd); return String(getSelection()); })()";

pub enum WebViewEvent {
    TitleChanged,
    UrlChanged,
    /// Loading started or stopped, or back/forward became (un)available.
    LoadingChanged,
    FaviconChanged,
    /// The page opened another: a popup, or a link meant for a new tab.
    /// `popup` already shows it; keep it to keep it open.
    NewWindow {
        popup: gpui::Entity<WebView>,
        url: String,
        disposition: Disposition,
        user_gesture: bool,
    },
    /// Answer with [`WebView::answer_dialog`].
    JsDialog(JsDialog),
    JsDialogClosed,
    /// Answer with [`WebView::choose_files`].
    FileChooser(FileChooser),
    /// The page wants a context menu at a point; the web view shows none.
    ContextMenu(ContextMenu),
    DownloadStarted {
        name: String,
        path: PathBuf,
    },
    DownloadFinished {
        name: String,
        path: PathBuf,
        ok: bool,
    },
    /// The page's renderer died; [`WebView::reload`] brings it back.
    Crashed,
    /// The page has its DevTools target id ([`WebView::target_id`]).
    Ready,
    /// The page closed (by itself, through DevTools, or [`WebView::close`]).
    Closed,
}

/// A web page. Render it as a child element to show it; pages that aren't
/// drawn keep running off screen.
pub struct WebView {
    pub(crate) page: PageRef,
    focus: FocusHandle,
    /// Whether CEF was last told the page has focus.
    pub(crate) focused: bool,
    pub(crate) pressed: Vec<MouseButton>,
    pub(crate) hovered: bool,
    pub(crate) modifiers: Modifiers,
    pub(crate) window: Option<AnyWindowHandle>,
    /// A URL to load once the browser exists.
    pending_url: Option<String>,
    error: Option<String>,
    _wake: Task<()>,
}

impl EventEmitter<WebViewEvent> for WebView {}

impl Focusable for WebView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WebView {
    /// A page in `profile` showing `url`. Starts the browser engine with the
    /// first page.
    pub fn new(profile: &Profile, url: &str, cx: &mut Context<Self>) -> Self {
        let page = browser::new_page(profile.clone());
        page.borrow_mut().url = url.to_string();
        let mut view = Self::adopt(page, cx);
        if let Err(err) = browser::create_browser(&view.page, url) {
            tracing::warn!("web view: {err}");
            view.error = Some(err);
        }
        view
    }

    /// A web view for a page Chromium already created (a popup).
    fn adopt(page: PageRef, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded(1);
        {
            let mut p = page.borrow_mut();
            p.notify = Some(tx);
            p.notify_pending = false;
        }
        let wake = cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.flush(cx)).is_err() {
                    return;
                }
            }
        });
        let view = Self {
            page,
            focus: cx.focus_handle(),
            focused: false,
            pressed: Vec::new(),
            hovered: false,
            modifiers: Modifiers::default(),
            window: None,
            pending_url: None,
            error: None,
            _wake: wake,
        };
        // Events may have queued before there was anyone to tell.
        view.page.borrow_mut().wake();
        view
    }

    /// Turns what the page reported into events, and redraws.
    fn flush(&mut self, cx: &mut Context<Self>) {
        let events = {
            let mut page = self.page.borrow_mut();
            page.notify_pending = false;
            std::mem::take(&mut page.events)
        };
        if let Some(url) = self.pending_url.take() {
            self.load_url(&url);
        }
        for event in events {
            let event = match event {
                PageEvent::Title => WebViewEvent::TitleChanged,
                PageEvent::Url => WebViewEvent::UrlChanged,
                PageEvent::Loading => WebViewEvent::LoadingChanged,
                PageEvent::Favicon => WebViewEvent::FaviconChanged,
                PageEvent::Popup {
                    page,
                    url,
                    disposition,
                    user_gesture,
                } => {
                    let popup = cx.new(|cx| WebView::adopt(page, cx));
                    WebViewEvent::NewWindow {
                        popup,
                        url,
                        disposition,
                        user_gesture,
                    }
                }
                PageEvent::JsDialog(dialog) => WebViewEvent::JsDialog(dialog),
                PageEvent::JsDialogClosed => WebViewEvent::JsDialogClosed,
                PageEvent::FileChooser(chooser) => WebViewEvent::FileChooser(chooser),
                PageEvent::ContextMenu(menu) => WebViewEvent::ContextMenu(menu),
                PageEvent::DownloadStarted { name, path } => {
                    WebViewEvent::DownloadStarted { name, path }
                }
                PageEvent::DownloadFinished { name, path, ok } => {
                    WebViewEvent::DownloadFinished { name, path, ok }
                }
                PageEvent::Crashed => WebViewEvent::Crashed,
                PageEvent::Ready => WebViewEvent::Ready,
                PageEvent::Closed => WebViewEvent::Closed,
                PageEvent::Keystroke(keystroke) => {
                    if let Some(window) = self.window {
                        cx.defer(move |cx| {
                            window
                                .update(cx, |_, window, cx| {
                                    window.dispatch_keystroke(keystroke, cx);
                                })
                                .ok();
                        });
                    }
                    continue;
                }
            };
            cx.emit(event);
        }
        cx.notify();
    }

    fn host(&self) -> Option<BrowserHost> {
        self.page.borrow().host()
    }

    fn browser(&self) -> Option<Browser> {
        self.page.borrow().browser.clone()
    }

    fn main_frame(&self) -> Option<Frame> {
        self.browser()?.main_frame()
    }

    fn focused_frame(&self) -> Option<Frame> {
        let browser = self.browser()?;
        browser.focused_frame().or_else(|| browser.main_frame())
    }

    /// Why the page couldn't open, if it couldn't.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn url(&self) -> String {
        self.page.borrow().url.clone()
    }

    pub fn title(&self) -> String {
        self.page.borrow().title.clone()
    }

    pub fn favicon_url(&self) -> Option<String> {
        self.page.borrow().favicon.clone()
    }

    pub fn is_loading(&self) -> bool {
        self.page.borrow().loading
    }

    pub fn can_go_back(&self) -> bool {
        self.page.borrow().can_go_back
    }

    pub fn can_go_forward(&self) -> bool {
        self.page.borrow().can_go_forward
    }

    /// The page's DevTools target id, once [`WebViewEvent::Ready`] came.
    pub fn target_id(&self) -> Option<String> {
        self.page.borrow().target_id.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.page.borrow().closed
    }

    /// Whether a JavaScript dialog waits for an answer.
    pub fn has_dialog(&self) -> bool {
        self.page.borrow().js_dialog.is_some()
    }

    pub fn load_url(&mut self, url: &str) {
        match self.main_frame() {
            Some(frame) => frame.load_url(Some(&url.into())),
            None => self.pending_url = Some(url.to_string()),
        }
    }

    pub fn go_back(&self) {
        if let Some(browser) = self.browser() {
            browser.go_back();
        }
    }

    pub fn go_forward(&self) {
        if let Some(browser) = self.browser() {
            browser.go_forward();
        }
    }

    pub fn reload(&self) {
        if let Some(browser) = self.browser() {
            browser.reload();
        }
    }

    pub fn stop(&self) {
        if let Some(browser) = self.browser() {
            browser.stop_load();
        }
    }

    /// Copies the selection to the clipboard.
    pub fn copy(&self, cx: &mut App) {
        self.copy_or_cut(false, cx);
    }

    /// Cuts the selection to the clipboard.
    pub fn cut(&self, cx: &mut App) {
        self.copy_or_cut(true, cx);
    }

    /// Pastes the clipboard's text.
    pub fn paste_from_clipboard(&self, cx: &mut App) {
        if !CLIPBOARD_THROUGH_GPUI {
            if let Some(frame) = self.focused_frame() {
                frame.paste();
            }
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.insert_text(&text, cx);
        }
    }

    fn copy_or_cut(&self, cut: bool, cx: &mut App) {
        if !CLIPBOARD_THROUGH_GPUI {
            if let Some(frame) = self.focused_frame() {
                if cut { frame.cut() } else { frame.copy() }
            }
            return;
        }
        let read = self.evaluate(SELECTION_JS, cx);
        let frame = self.focused_frame();
        cx.spawn(async move |cx| {
            let Ok(serde_json::Value::String(text)) = read.await else {
                return;
            };
            if text.is_empty() {
                return;
            }
            cx.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)));
            if cut && let Some(frame) = frame {
                frame.execute_java_script(
                    Some(&"document.execCommand('delete')".into()),
                    Some(&"".into()),
                    0,
                );
            }
        })
        .detach();
    }

    pub fn select_all(&self) {
        if let Some(frame) = self.focused_frame() {
            frame.select_all();
        }
    }

    pub fn undo(&self) {
        if let Some(frame) = self.focused_frame() {
            frame.undo();
        }
    }

    pub fn redo(&self) {
        if let Some(frame) = self.focused_frame() {
            frame.redo();
        }
    }

    /// Types `text` into the focused element, as if pasted.
    pub fn insert_text(&self, text: &str, cx: &App) {
        self.devtools_call("Input.insertText", serde_json::json!({ "text": text }), cx)
            .detach();
    }

    /// Runs `code` in the page's main frame.
    pub fn execute_javascript(&self, code: &str) {
        if let Some(frame) = self.main_frame() {
            frame.execute_java_script(Some(&code.into()), Some(&"".into()), 0);
        }
    }

    /// Calls a DevTools method on the page; resolves to the JSON result.
    pub fn devtools_call(
        &self,
        method: &str,
        params: serde_json::Value,
        cx: &App,
    ) -> Task<Result<serde_json::Value, String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let sent = {
            let mut page = self.page.borrow_mut();
            let id = page.next_devtools_id;
            page.next_devtools_id += 1;
            page.devtools_calls.push((id, tx));
            let message =
                serde_json::json!({ "id": id, "method": method, "params": params }).to_string();
            page.host().map(|host| (host, message))
        };
        let Some((host, message)) = sent else {
            return Task::ready(Err("the page isn't open".into()));
        };
        if host.send_dev_tools_message(Some(message.as_bytes())) == 0 {
            return Task::ready(Err("the page didn't take the call".into()));
        }
        crate::runtime::schedule_pump(0);
        cx.background_spawn(async move {
            let text = rx.await.map_err(|_| "the page closed".to_string())??;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        })
    }

    /// Evaluates a JavaScript expression in the page and returns its value.
    pub fn evaluate(&self, expression: &str, cx: &App) -> Task<Result<serde_json::Value, String>> {
        let call = self.devtools_call(
            "Runtime.evaluate",
            serde_json::json!({ "expression": expression, "returnByValue": true }),
            cx,
        );
        cx.background_spawn(async move {
            let result = call.await?;
            Ok(result.pointer("/result/value").cloned().unwrap_or_default())
        })
    }

    /// Answers the open JavaScript dialog.
    pub fn answer_dialog(&mut self, accept: bool, text: Option<&str>) {
        let callback = self.page.borrow_mut().js_dialog.take();
        if let Some(callback) = callback {
            callback.cont(accept as i32, text.map(CefString::from).as_ref());
        }
    }

    /// Answers the open file chooser: the chosen paths, or `None` to cancel.
    pub fn choose_files(&mut self, paths: Option<Vec<PathBuf>>) {
        let callback = self.page.borrow_mut().file_dialog.take();
        let Some(callback) = callback else { return };
        match paths {
            Some(paths) if !paths.is_empty() => {
                let mut list = CefStringList::new();
                for path in paths {
                    list.append(&path.to_string_lossy());
                }
                callback.cont(Some(&mut list));
            }
            _ => callback.cancel(),
        }
    }

    /// Closes the page. [`WebViewEvent::Closed`] follows.
    pub fn close(&self) {
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }

    /// Keystrokes the app binds, given back to it when the page has
    /// keyboard focus of its own (Windows).
    pub fn set_app_keystrokes(&self, keystrokes: Vec<Keystroke>) {
        self.page.borrow_mut().app_keystrokes = keystrokes;
    }

    pub(crate) fn send_key(&self, keystroke: &Keystroke, down: bool, held: bool) {
        let Some(event) = crate::input::key_down_up(keystroke, down, held) else {
            return;
        };
        if let Some(host) = self.host() {
            host.send_key_event(Some(&event));
        }
    }

    fn type_text(&self, text: &str) {
        let Some(host) = self.host() else { return };
        for event in crate::input::char_events(text, &self.modifiers) {
            host.send_key_event(Some(&event));
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        // The page goes with its last view.
        if let Ok(page) = self.page.try_borrow()
            && let Some(host) = page.host()
        {
            drop(page);
            host.close_browser(1);
        }
    }
}

impl Render for WebView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("web-view")
            .track_focus(&self.focus)
            .size_full()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                this.modifiers = event.keystroke.modifiers;
                this.send_key(&event.keystroke, true, event.is_held);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| {
                this.modifiers = event.keystroke.modifiers;
                this.send_key(&event.keystroke, false, false);
            }))
            .child(WebViewElement::new(
                cx.entity(),
                self.page.clone(),
                self.focus.clone(),
            ))
    }
}

impl EntityInputHandler for WebView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self
            .page
            .borrow()
            .ime
            .composing
            .as_ref()
            .map_or(0, |r| r.end);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.page.borrow().ime.composing.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        let composing = self.page.borrow_mut().ime.composing.take();
        if composing.is_some()
            && let Some(host) = self.host()
        {
            host.ime_finish_composing_text(0);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let composing = self.page.borrow_mut().ime.composing.take();
        if composing.is_some() {
            if let Some(host) = self.host() {
                host.ime_commit_text(Some(&text.into()), None, 0);
            }
        } else {
            self.type_text(text);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else { return };
        let len = new_text.encode_utf16().count();
        if len == 0 {
            self.page.borrow_mut().ime.composing = None;
            host.ime_cancel_composition();
            return;
        }
        self.page.borrow_mut().ime.composing = Some(0..len);
        let underline = CompositionUnderline {
            range: cef::Range {
                from: 0,
                to: len as u32,
            },
            color: 0xff00_0000,
            background_color: 0,
            thick: 0,
            ..Default::default()
        };
        let selection = new_selected_range.unwrap_or(len..len);
        let selection = cef::Range {
            from: selection.start as u32,
            to: selection.end as u32,
        };
        host.ime_set_composition(
            Some(&new_text.into()),
            Some(&[underline]),
            None,
            Some(&selection),
        );
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let page = self.page.borrow();
        let bounds = &page.ime.character_bounds;
        let rect = bounds
            .get(range_utf16.start)
            .or_else(|| bounds.last())?
            .clone();
        let (x, y) = page.geometry.logical_point(rect.x, rect.y);
        let (right, bottom) = page
            .geometry
            .logical_point(rect.x + rect.width, rect.y + rect.height);
        Some(Bounds::from_corners(
            element_bounds.origin + gpui::point(px(x), px(y)),
            element_bounds.origin + gpui::point(px(right), px(bottom)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
