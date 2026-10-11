//! A minimal browser: an address bar, the page, and a popover over the page
//! to check that GPUI draws over it.
//!
//! Run: cargo run -p gpui_web --example browser [-- URL]
//! `GPUI_WEB_PRESENTER=cpu` forces the CPU path. `--popover` starts with the
//! popover open; `--click X,Y` clicks there (window coordinates) after five
//! seconds, through GPUI's input path, for checks without a person.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_web::{HoleBackground, Profile, RemoteDebugging, RuntimeOptions, WebView, WebViewEvent};

struct Browser {
    web: Entity<WebView>,
    url: Entity<InputState>,
    popover: bool,
    status: String,
    _subscriptions: Vec<Subscription>,
}

impl Browser {
    fn new(start: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let profile = Profile::new(cache_dir().join("profile"));
        let web = cx.new(|cx| WebView::new(&profile, &start, cx));
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("Address"));
        url.update(cx, |input, cx| input.set_value(start, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&url, window, |this, input, event: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = input.read(cx).value().to_string();
                    let url = if text.contains("://") || text.starts_with("about:") {
                        text
                    } else {
                        format!("https://{text}")
                    };
                    this.web.update(cx, |web, _| web.load_url(&url));
                }
            }),
            cx.subscribe_in(
                &web,
                window,
                |this, web, event: &WebViewEvent, window, cx| {
                    match event {
                        WebViewEvent::UrlChanged => {
                            let url = web.read(cx).url();
                            this.url
                                .update(cx, |input, cx| input.set_value(url, window, cx));
                        }
                        WebViewEvent::NewWindow { url, .. } => {
                            // One page here: open it in place.
                            let url = url.clone();
                            web.update(cx, |web, _| web.load_url(&url));
                        }
                        WebViewEvent::JsDialog(dialog) => {
                            this.status = format!("dialog: {}", dialog.message);
                            web.update(cx, |web, _| web.answer_dialog(true, None));
                        }
                        WebViewEvent::DownloadFinished { path, .. } => {
                            this.status = format!("downloaded {}", path.display());
                        }
                        WebViewEvent::ContextMenu(menu) => {
                            this.status = format!(
                                "context menu at {:?} link {:?}",
                                menu.position, menu.link_url
                            );
                        }
                        WebViewEvent::Crashed => this.status = "the page crashed".into(),
                        _ => {}
                    }
                    cx.notify();
                },
            ),
        ];
        Self {
            web,
            url,
            popover: std::env::args().any(|a| a == "--popover"),
            status: String::new(),
            _subscriptions: subscriptions,
        }
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let web = self.web.read(cx);
        let (back, forward, loading, title) = (
            web.can_go_back(),
            web.can_go_forward(),
            web.is_loading(),
            web.title(),
        );
        div()
            .size_full()
            .relative()
            .text_color(theme.foreground)
            .child(HoleBackground::new(theme.background))
            .child(
                v_flex()
                    .size_full()
                    .child(
                        h_flex()
                            .gap_1()
                            .p_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("back")
                                    .label("←")
                                    .ghost()
                                    .small()
                                    .disabled(!back)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.web.read(cx).go_back()),
                                    ),
                            )
                            .child(
                                Button::new("forward")
                                    .label("→")
                                    .ghost()
                                    .small()
                                    .disabled(!forward)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.web.read(cx).go_forward()
                                        }),
                                    ),
                            )
                            .child(
                                Button::new("reload")
                                    .label(if loading { "×" } else { "⟳" })
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let web = this.web.read(cx);
                                        if loading { web.stop() } else { web.reload() }
                                    })),
                            )
                            .child(div().flex_1().child(Input::new(&self.url).small()))
                            .child(
                                Button::new("popover")
                                    .label("Popover")
                                    .small()
                                    .tooltip("A GPUI tooltip, over the page")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.popover = !this.popover;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(self.web.clone()))
                    .child(
                        h_flex()
                            .px_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{title} {}", self.status)),
                    ),
            )
            .when(self.popover, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .right(px(16.))
                        .w(px(300.))
                        .p_3()
                        .rounded_lg()
                        .shadow_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .child("A GPUI popover drawn over the page.")
                        .child(
                            div()
                                .id("popover-strip")
                                .mt_2()
                                .p_2()
                                .rounded_md()
                                .bg(theme.primary.opacity(0.5))
                                .child("Translucent: the page shows through. Click me.")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.status = "the popover got the click".into();
                                    cx.notify();
                                })),
                        ),
                )
            })
    }
}

/// `--click X,Y`.
fn click_arg() -> Option<(f32, f32)> {
    let args: Vec<String> = std::env::args().collect();
    let value = args
        .iter()
        .position(|a| a == "--click")
        .and_then(|i| args.get(i + 1))?;
    let (x, y) = value.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn cache_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("gpui_web_example")
}

fn main() {
    if let Some(code) = gpui_web::handle_subprocess(false) {
        std::process::exit(code);
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let start = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && (*i == 0 || args[i - 1] != "--click"))
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| "https://example.com".into());
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        gpui_web::init(
            RuntimeOptions {
                root_cache_dir: cache_dir(),
                download_dir: Some(std::env::temp_dir()),
                remote_debugging: RemoteDebugging::Off,
                log_level: Default::default(),
                log_file: None,
                user_agent_product: None,
                subprocess_path: None,
            },
            cx,
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(760.)),
                cx,
            ))),
            window_background: gpui_web::window_background(),
            ..Default::default()
        };
        let start = start.clone();
        let (window, _) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Browser::new(start, window, cx))
        })
        .expect("open window");
        // gpui-component's root paints the theme background over the whole
        // window; the HoleBackground paints it instead, around the page.
        if let Some(root) = window.downcast::<gpui_kit::base::Root>() {
            root.update(cx, |root, _, _| {
                root.style().background = Some(gpui_kit::transparent_black().into());
            })
            .ok();
        }
        if let Some((x, y)) = click_arg() {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                window
                    .update(cx, |_, window, cx| {
                        let position = point(px(x), px(y));
                        window.dispatch_event(
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers: Modifiers::default(),
                            }),
                            cx,
                        );
                        for down in [true, false] {
                            let event = if down {
                                PlatformInput::MouseDown(MouseDownEvent {
                                    button: MouseButton::Left,
                                    position,
                                    modifiers: Modifiers::default(),
                                    click_count: 1,
                                    first_mouse: false,
                                })
                            } else {
                                PlatformInput::MouseUp(MouseUpEvent {
                                    button: MouseButton::Left,
                                    position,
                                    modifiers: Modifiers::default(),
                                    click_count: 1,
                                })
                            };
                            window.dispatch_event(event, cx);
                        }
                    })
                    .ok();
            })
            .detach();
        }
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
    gpui_web::shutdown(Duration::from_secs(2));
}
