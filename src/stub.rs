//! The crate without CEF (the `cef` feature off): the same API, but no
//! browser. Pages report [`UNAVAILABLE`] as their error and draw nothing;
//! the rest does nothing. For builds that can't download Chromium.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    App, Context, Div, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    Keystroke, Styled as _, Task, Window, WindowBackgroundAppearance, div,
};

use crate::types::{RuntimeOptions, WebViewEvent};

const UNAVAILABLE: &str = "built without the browser engine (gpui_web's `cef` feature)";

/// There is no subprocess to run.
pub fn handle_subprocess(devtools_pipe: bool) -> Option<i32> {
    if devtools_pipe {
        reserve_devtools_pipe();
    }
    None
}

pub fn reserve_devtools_pipe() {
    #[cfg(unix)]
    crate::devtools::reserve_pipe();
}

/// No page is composited under the window.
pub fn window_background() -> WindowBackgroundAppearance {
    WindowBackgroundAppearance::Opaque
}

pub fn init(_options: RuntimeOptions, _cx: &mut App) {
    tracing::info!("web view: {UNAVAILABLE}");
}

pub fn shutdown(_timeout: Duration) {}

pub fn chromium_version() -> &'static str {
    "(not built in)"
}

#[derive(Clone)]
pub struct Profile(std::rc::Rc<Option<PathBuf>>);

impl Profile {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(std::rc::Rc::new(Some(dir.into())))
    }

    pub fn default_profile() -> Self {
        Self(std::rc::Rc::new(None))
    }

    pub fn dir(&self) -> Option<&Path> {
        self.0.as_deref()
    }
}

impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The window background: there are no holes to leave.
pub struct HoleBackground {
    color: gpui::Hsla,
}

impl HoleBackground {
    pub fn new(color: impl Into<gpui::Hsla>) -> Self {
        Self {
            color: color.into(),
        }
    }
}

impl IntoElement for HoleBackground {
    type Element = Div;

    fn into_element(self) -> Div {
        div().absolute().inset_0().size_full().bg(self.color)
    }
}

/// A page that never loads.
pub struct WebView {
    url: String,
    focus: FocusHandle,
}

impl EventEmitter<WebViewEvent> for WebView {}

impl Focusable for WebView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WebView {
    pub fn new(_profile: &Profile, url: &str, cx: &mut Context<Self>) -> Self {
        Self {
            url: url.to_string(),
            focus: cx.focus_handle(),
        }
    }

    pub fn error(&self) -> Option<&str> {
        Some(UNAVAILABLE)
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn title(&self) -> String {
        String::new()
    }

    pub fn favicon_url(&self) -> Option<String> {
        None
    }

    pub fn is_loading(&self) -> bool {
        false
    }

    pub fn can_go_back(&self) -> bool {
        false
    }

    pub fn can_go_forward(&self) -> bool {
        false
    }

    pub fn target_id(&self) -> Option<String> {
        None
    }

    pub fn is_closed(&self) -> bool {
        false
    }

    pub fn has_dialog(&self) -> bool {
        false
    }

    pub fn load_url(&mut self, url: &str) {
        self.url = url.to_string();
    }

    pub fn go_back(&self) {}

    pub fn go_forward(&self) {}

    pub fn reload(&self) {}

    pub fn stop(&self) {}

    pub fn copy(&self, _cx: &mut App) {}

    pub fn cut(&self, _cx: &mut App) {}

    pub fn paste_from_clipboard(&self, _cx: &mut App) {}

    pub fn select_all(&self) {}

    pub fn undo(&self) {}

    pub fn redo(&self) {}

    pub fn insert_text(&self, _text: &str, _cx: &App) {}

    pub fn execute_javascript(&self, _code: &str) {}

    pub fn devtools_call(
        &self,
        _method: &str,
        _params: serde_json::Value,
        _cx: &App,
    ) -> Task<Result<serde_json::Value, String>> {
        Task::ready(Err(UNAVAILABLE.into()))
    }

    pub fn evaluate(
        &self,
        _expression: &str,
        _cx: &App,
    ) -> Task<Result<serde_json::Value, String>> {
        Task::ready(Err(UNAVAILABLE.into()))
    }

    pub fn answer_dialog(&mut self, _accept: bool, _text: Option<&str>) {}

    pub fn choose_files(&mut self, _paths: Option<Vec<PathBuf>>) {}

    pub fn close(&self) {}

    pub fn set_app_keystrokes(&self, _keystrokes: Vec<Keystroke>) {}
}

impl gpui::Render for WebView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().id("web-view").track_focus(&self.focus).size_full()
    }
}
