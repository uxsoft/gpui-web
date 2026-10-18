# Embedded browser: feasibility spikes

`gpui_web` embeds Chromium through CEF; [Wisp](https://github.com/uxsoft/wisp-rs)'s
canvas browser is built on it. Before building it, these spikes checked the
assumptions the design rests on. Each spike is an example in `examples/` and
can be re-run after a GPUI or CEF bump.

Pinned versions: GPUI `gpui-pre =0.3.8` (via `gpui-kit 0.7.1`), CEF
`154.0.34` (Chromium 154.0.8037.98, `cef =154.5.0`).

Test machine: Linux 7.2 (CachyOS), COSMIC (cosmic-comp) on Wayland, two
3840×2160 outputs at 150 %, NVIDIA RTX 4080 SUPER (driver 615.71, renderD128,
drives the compositor) and AMD Raphael iGPU (renderD129).

## S1: Wayland hole-punch (`spike_wayland_hole`): works

A transparent GPUI window paints its background everywhere except a hole. A
`wl_subsurface` of our own, on GPUI's `wl_display` through
`Backend::from_foreign_display` with a private event queue, sits
`place_below` GPUI's surface in desync mode with an empty input region and a
`wp_viewport`.

- A1: GPUI's surface is premultiplied-alpha when the window background is
  `Transparent`; the subsurface shows through the hole.
- A2: `gpui::Window` implements `HasWindowHandle`/`HasDisplayHandle`; on
  Wayland they return the `wl_surface` and `wl_display`. `Window` also has an
  inherent `window_handle()` (GPUI's `AnyWindowHandle`), so call the trait
  method as `HasWindowHandle::window_handle(window)`.
- A GPUI popover draws over the subsurface, and a 50 % alpha fill blends with
  it. Input reaches GPUI.
- **Scale:** on this machine GPUI's `Window::scale_factor()` reports 2 while
  the compositor's fractional scale for the output is 1.5 (the subsurface's own
  `wp_fractional_scale_v1` says 1.5). The presenter therefore sizes buffers
  from its own fractional-scale object and never from GPUI's scale.
- Not checked here: GNOME, KDE, Sway, Hyprland (none installed); see
  `docs/webview-validation.md`. `cosmic-screenshot` saves logical-size images,
  so it cannot judge pixel exactness.

## S4: CEF OSR shared textures on Wayland (`spike_cef_dmabuf`): works, with a workaround

`OnAcceleratedPaint` delivers one plane, `CEF_COLOR_TYPE_BGRA_8888`,
modifier `0x0` (LINEAR), stride padded to a multiple of 64 pixels (800 px →
3328 bytes), coded size equal to the view rect. LINEAR buffers can be imported
on either GPU.

The spike counted frames; building the presenter then checked their
contents (a solid red page, read back from the dma-buf and after our copy):

| Flags | Chromium's GPU | Frames |
|---|---|---|
| `--use-gl=angle --use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE` | either | arrive at full rate but are **empty** (all zero), even read 100 ms later |
| none (ANGLE on GL) | AMD (Mesa) | **correct** |
| none, or `--use-angle=gl-egl` | NVIDIA | none: "Unable to initialize SkSurface" |
| `--use-angle=swiftshader` | – | none ("Unable to allocate frame") |

So the runtime renders with ANGLE on GL on a Mesa GPU (`--render-node-override`):
the compositor's GPU when it is Mesa's, else another Mesa GPU (here the AMD
iGPU while the compositor runs on NVIDIA). The presenter imports those
LINEAR buffers on the compositor's GPU, which works across GPUs. With no
Mesa GPU, pages are painted on the CPU (`OnPaint`) and uploaded into the
same subsurface. `GPUI_WEB_ANGLE` and `GPUI_WEB_RENDER_NODE` override the
choice.

On NVIDIA, a LINEAR dma-buf can't be a framebuffer attachment (texture or
renderbuffer, any fourcc: `GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT`), so the
copy samples CEF's buffer as a `GL_TEXTURE_EXTERNAL_OES` texture with
nearest filtering and draws it texel for pixel into our own buffer (GBM,
compositor-preferred modifier, which NVIDIA can render to).

- **Frame rate:** `windowless_frame_rate` above 60 is honored (120 and 144 fps
  measured), so the presenter sets it to the output's refresh rate and needs no
  external begin frames.
- **Scale (known CEF issue):** `CefScreenInfo.device_scale_factor` is asked
  for and ignored, at 1.25, 1.5, 2 and 3, in both shared-texture and software
  paths; `--force-device-scale-factor` doesn't help either. Buffers always
  match the view rect. Workaround, as in purego-cef2gtk: report a view rect in
  device pixels with scale 1 and set the page zoom to the scale
  (`zoom_level = ln(scale) / ln(1.2)`). Verified at 1.5: buffer 1200×900, CSS
  viewport 800×600, `devicePixelRatio` 1.5. Zoom must be re-applied after each
  navigation commits, and mouse coordinates are scaled to device pixels.

## Found while building

- **The message pump must tick while CEF runs, even with no browser.**
  CEF sometimes asks for work once and then not again while it creates the
  first browser; a pump that only ticks while browsers exist then stalls
  before the first page exists.
- **gpui-component's `Root` paints the theme background over the whole
  window** (a root plugin's style). Apps clear it (`Root::style()`'s
  background, which applies after the plugins) and let `HoleBackground`
  paint the background instead.
- **One process per cache directory.** CEF locks `root_cache_path`; a
  second instance with the same directory fails `CefInitialize`.

## S5: DevTools for agents (`spike_cef_devtools`): works

- `--remote-debugging-pipe` works in-process. The browser process moves a pipe
  pair onto fds 3 (Chromium reads) and 4 (Chromium writes) before
  `CefInitialize`, after `execute_process` (only the browser process). The fds
  must first be moved above 10, since `pipe2` itself may return 3 and 4.
- Browser-level `Target.*` works: `setDiscoverTargets`, `getTargets`,
  `attachToBrowserTarget`, `targetCreated`/`targetInfoChanged`/
  `targetDestroyed`, `Browser.getVersion`, `Browser.setDownloadBehavior`.
- `SendDevToolsMessage` with `Target.getTargetInfo` on a browser returns its
  own `targetId`, so Wisp knows which DevTools target each web view is.
- `Target.createTarget` makes a Chrome-style browser (it also creates
  `browser_ui` omnibox targets). The CDP proxy intercepts it and opens a web
  view instead, and the hub only tracks `page` targets.

## S2: Windows hole-punch (`spike_windows_hole`): to run on Windows

`cargo run --example spike_windows_hole`. A solid red child window
sits in a hole of a transparent GPUI window, under a GPUI popover. Check that
the red shows through the hole and the desktop never does (A3), that the
popover and its translucent strip draw over the red, and that clicking the
strip counts clicks: the child window is made transparent to the mouse
(`WM_NCHITTEST` → `HTTRANSPARENT`), as Chromium's windows are, so GPUI
hit-tests its overlays. If the desktop shows through, check the window's
redirection bitmap and style flags. With `GPUI_DISABLE_DIRECT_COMPOSITION`
set the runtime switches to CPU frames instead.

## S3: macOS bootstrap: to run on macOS

`mise run bundle` (or `cargo run -p wisp`, which runs from `Wisp Dev.app`
with the framework and helper apps linked in), then open a Browser tab.
Check that CEF starts inside GPUI's `NSApplication` (the log says "CEF
started"; `CefAppProtocol` is added at run time, A4) and that pages show.

The design changed here: GPUI's `surface` element (`gpui-pre 0.3.8`,
`metal_renderer.rs`) asserts 4:2:0 YCbCr (`420YpCbCr8BiPlanarFullRange`)
pixel buffers, so BGRA frames would panic, and converting to 4:2:0 would blur
colored text. Pages are composited the way GPUI's own blur view is: a
layer-backed `NSView` added under GPUI's view in the content view
(`NSWindowBelow`), whose layer shows a pool of IOSurfaces each frame is
blitted into with Metal; the window is transparent where the page is.
