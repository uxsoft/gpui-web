//! The public types both builds share: with CEF and without it.

use std::path::PathBuf;

use gpui::Point;

use crate::WebView;

/// How the app's DevTools clients reach the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteDebugging {
    Off,
    /// `--remote-debugging-pipe` on fds 3 and 4 (Unix). Needs
    /// [`handle_subprocess`](crate::handle_subprocess) to reserve them; see [`take_devtools_pipe`](crate::take_devtools_pipe).
    Pipe,
    /// `--remote-debugging-port=0` on localhost; see [`devtools_url`](crate::devtools_url).
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
    /// Replaces Chromium's `Chrome/<version>` in the user agent, which sites
    /// that look for Chrome then miss; `None` keeps the standard one.
    pub user_agent_product: Option<String>,
    /// Resolves host names through the OS, as other apps do, instead of
    /// Chromium's own DNS client and DNS-over-HTTPS, which VPN and zero-trust
    /// clients that route by DNS (Zscaler) never see.
    pub system_dns: bool,
    /// Lets public pages reach local and loopback addresses, answering the
    /// local network access prompt pages can't show. Behind a zero-trust
    /// client, public services can resolve to such addresses.
    pub allow_local_network: bool,
    /// The executable CEF runs its subprocesses from, when it isn't this
    /// one (tests); it must call [`handle_subprocess`](crate::handle_subprocess) first thing.
    pub subprocess_path: Option<PathBuf>,
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
