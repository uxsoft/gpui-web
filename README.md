# gpui_web

A Chromium web view for [GPUI](https://www.gpui.rs), built on
[CEF](https://bitbucket.org/chromiumembedded/cef). It is the browser behind
[Wisp](https://github.com/uxsoft/wisp-rs)'s canvas.

On Wayland and Windows the page lives in a native layer under GPUI's window,
and GPUI leaves a hole in its background where the page shows through, so
GPUI's menus and popovers still draw over it. On macOS the page is a GPU
surface GPUI composites; on X11 it is a CPU image.

## Use

```toml
[dependencies]
gpui_web = { git = "https://github.com/uxsoft/gpui-web" }
```

1. First thing in `main`: `if let Some(code) = gpui_web::handle_subprocess(true) { std::process::exit(code) }`
   (CEF runs its renderer and GPU processes from the same executable).
2. At app start: `gpui_web::init`, and `gpui_web::window_background()` for the window.
3. `gpui_web::HoleBackground` as the window root's background.
4. `WebView::new` for each page; `gpui_web::shutdown` when the app quits.

`cargo run --example browser -- URL` is a minimal browser.

## Features

- `cef` (default): the browser engine. Building it downloads CEF 154 (or uses
  the distribution in `CEF_PATH`) and compiles its C++ wrapper, which needs
  CMake and Ninja.
- Without `cef` (`default-features = false`), the crate builds offline, with
  no Chromium: the API is the same, pages draw nothing and report
  `WebView::error`. Apps can expose it as a feature of their own for builds
  where CEF can't be downloaded.
- `dox`: type-checks the CEF code without CEF's binaries (`cargo check` on any
  platform; the result does not link).

Chromium runs without its sandbox. See
[docs/webview-spikes.md](docs/webview-spikes.md) for what each platform path
relies on.

## License

Apache-2.0
