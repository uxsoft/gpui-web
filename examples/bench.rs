//! Frame-rate check: a page that animates and scrolls itself for a while,
//! with the presenter's frame statistics logged each second.
//!
//! Run: cargo run --release -p gpui_web --example bench -- [seconds]
//! Each line reads "N frames/s, longest gap M ms"; N should match the
//! display's refresh rate.

use std::time::Duration;

use gpui_kit::*;
use gpui_web::{HoleBackground, Profile, RemoteDebugging, RuntimeOptions, WebView};

const PAGE: &str = "data:text/html,<style>body{margin:0;font:15px sans-serif}\
%23box{position:fixed;top:20px;right:20px;width:120px;height:120px;background:%23d33;\
animation:spin 2s linear infinite}@keyframes spin{to{transform:rotate(360deg)}}</style>\
<div id=box></div><div id=text></div><script>\
const t=document.getElementById('text');\
for(let i=0;i<2000;i++){const p=document.createElement('p');p.textContent=i+' The quick brown fox jumps over the lazy dog.';t.appendChild(p)}\
let y=0;(function step(){y+=4;if(y>document.body.scrollHeight-innerHeight)y=0;scrollTo(0,y);requestAnimationFrame(step)})();\
</script>";

struct Bench {
    web: Entity<WebView>,
}

impl Render for Bench {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .child(HoleBackground::new(rgb(0x202020)))
            .child(div().size_full().p_4().child(self.web.clone()))
    }
}

fn main() {
    if let Some(code) = gpui_web::handle_subprocess(false) {
        std::process::exit(code);
    }
    // SAFETY: before any other thread exists.
    unsafe { std::env::set_var("GPUI_WEB_STATS", "1") };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("gpui_web=info"))
        .init();
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        gpui_web::init(
            RuntimeOptions {
                root_cache_dir: std::env::temp_dir().join("gpui_web_bench"),
                download_dir: None,
                remote_debugging: RemoteDebugging::Off,
                log_level: Default::default(),
                log_file: None,
                user_agent_product: None,
                system_dns: false,
                allow_local_network: false,
                subprocess_path: None,
            },
            cx,
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1000.), px(700.)),
                cx,
            ))),
            window_background: gpui_web::window_background(),
            ..Default::default()
        };
        cx.open_window(options, |_, cx| {
            let profile = Profile::new(std::env::temp_dir().join("gpui_web_bench").join("profile"));
            let web = cx.new(|cx| WebView::new(&profile, PAGE, cx));
            cx.new(|_| Bench { web })
        })
        .unwrap();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_secs(seconds))
                .await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    gpui_web::shutdown(Duration::from_secs(2));
}
