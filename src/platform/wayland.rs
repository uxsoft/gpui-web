//! Wayland: the page in a `wl_subsurface` of our own, below GPUI's surface.
//!
//! We share GPUI's `wl_display` but keep our objects on an event queue of
//! our own, dispatched by a thread. Each CEF frame (a DMA-BUF valid only
//! during the paint callback) is copied by one GPU blit into a small pool of
//! buffers we own, which go to the compositor. Page updates don't involve
//! GPUI at all: the subsurface is desynchronized, except while its position
//! or size changes, when it moves in step with GPUI's next frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use cef::{
    AcceleratedPaintInfo, Browser, ImplBrowser as _, ImplBrowserHost as _, PaintElementType, Rect,
};
use gpui::{App, Window};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_subcompositor::WlSubcompositor,
    wl_subsurface::WlSubsurface,
    wl_surface::{self, WlSurface},
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use super::{Placement, Presenter};

const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// Buffers in flight: one on screen, one queued, one being drawn.
const POOL: usize = 3;

pub(crate) fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// The render node of the compositor's main GPU, from its dma-buf
/// feedback, so Chromium renders on the GPU the compositor reads from.
pub(crate) fn main_render_node() -> Option<PathBuf> {
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();
    let dmabuf: ZwpLinuxDmabufV1 = globals.bind(&qh, 4..=5, ()).ok()?;
    let feedback = Arc::new(Mutex::new(Feedback::default()));
    let _fb = dmabuf.get_default_feedback(&qh, feedback.clone());
    let mut state = State;
    for _ in 0..4 {
        queue.roundtrip(&mut state).ok()?;
        if feedback.lock().ok()?.done {
            break;
        }
    }
    let device = feedback.lock().ok()?.main_device?;
    render_node_of(device)
}

/// Where Chromium renders: the compositor's GPU, else another, as long as
/// its driver is Mesa's. Chromium's shared textures come out empty or not at
/// all on NVIDIA's driver; without a Mesa GPU pages are painted on the CPU.
pub(crate) fn chromium_render_node() -> Option<PathBuf> {
    static NODE: OnceLock<Option<PathBuf>> = OnceLock::new();
    NODE.get_or_init(|| {
        let main = main_render_node();
        let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        if let Some(main) = &main {
            nodes.retain(|n| n != main);
            nodes.insert(0, main.clone());
        }
        let node = nodes
            .into_iter()
            .find(|node| driver_of(node).is_some_and(|d| d != "nvidia"));
        tracing::info!("web view: Chromium renders on {node:?} (compositor on {main:?})");
        node
    })
    .clone()
}

/// The kernel driver of a render node, e.g. `amdgpu`, `i915`, `nvidia`.
fn driver_of(node: &Path) -> Option<String> {
    let name = node.file_name()?.to_string_lossy().into_owned();
    let driver = std::fs::read_link(format!("/sys/class/drm/{name}/device/driver")).ok()?;
    Some(driver.file_name()?.to_string_lossy().into_owned())
}

/// `/dev/dri/renderD*` of the DRM device `dev`.
fn render_node_of(dev: u64) -> Option<PathBuf> {
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    let dir = PathBuf::from(format!("/sys/dev/char/{major}:{minor}/device/drm"));
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("renderD"))
        .map(|name| Path::new("/dev/dri").join(name))
}

/// What our queue's thread knows.
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSubcompositor);
delegate_noop!(State: ignore WlSubsurface);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

impl Dispatch<WlSurface, Arc<Mutex<Vec<WlOutput>>>> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        event: wl_surface::Event,
        outputs: &Arc<Mutex<Vec<WlOutput>>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut outputs = outputs.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            wl_surface::Event::Enter { output } => outputs.push(output),
            wl_surface::Event::Leave { output } => outputs.retain(|o| o != &output),
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, Arc<AtomicI32>> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        refresh: &Arc<AtomicI32>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            flags: WEnum::Value(flags),
            refresh: mhz,
            ..
        } = event
            && flags.contains(wl_output::Mode::Current)
        {
            refresh.store(mhz, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, Arc<AtomicU32>> for State {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        scale: &Arc<AtomicU32>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale: s } = event {
            scale.store(s, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            released.store(true, Ordering::Release);
        }
    }
}

/// The compositor's dma-buf preferences.
#[derive(Default)]
struct Feedback {
    table: Vec<(u32, u64)>,
    main_device: Option<u64>,
    tranche_device: Option<u64>,
    tranche_formats: Vec<u16>,
    /// Modifiers for XRGB8888 on the main device.
    modifiers: Vec<u64>,
    done: bool,
}

fn dev_t(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?))
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, Arc<Mutex<Feedback>>> for State {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        feedback: &Arc<Mutex<Feedback>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event as E;
        let mut f = feedback.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            E::FormatTable { fd, size } => f.table = read_format_table(&fd, size as usize),
            E::MainDevice { device } => f.main_device = dev_t(&device),
            E::TrancheTargetDevice { device } => f.tranche_device = dev_t(&device),
            E::TrancheFormats { indices } => {
                f.tranche_formats = indices
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_ne_bytes(*c))
                    .collect()
            }
            E::TrancheDone => {
                if f.tranche_device.is_none() || f.tranche_device == f.main_device {
                    let picked: Vec<u64> = f
                        .tranche_formats
                        .iter()
                        .filter_map(|&ix| f.table.get(ix as usize).copied())
                        .filter(|(format, _)| *format == DRM_FORMAT_XRGB8888)
                        .map(|(_, modifier)| modifier)
                        .collect();
                    for modifier in picked {
                        if !f.modifiers.contains(&modifier) {
                            f.modifiers.push(modifier);
                        }
                    }
                }
                f.tranche_formats.clear();
            }
            E::Done => f.done = true,
            _ => {}
        }
    }
}

fn read_format_table(fd: &OwnedFd, size: usize) -> Vec<(u32, u64)> {
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd.as_raw_fd(),
            0,
        );
        if map == libc::MAP_FAILED {
            return Vec::new();
        }
        let bytes = std::slice::from_raw_parts(map as *const u8, size);
        let table = bytes
            .as_chunks::<16>()
            .0
            .iter()
            .map(|e| {
                let format = u32::from_ne_bytes(e[0..4].try_into().unwrap());
                let modifier = u64::from_ne_bytes(e[8..16].try_into().unwrap());
                (format, modifier)
            })
            .collect();
        libc::munmap(map, size);
        table
    }
}

/// Our objects on GPUI's connection, shared by every page.
struct Globals {
    conn: Connection,
    qh: QueueHandle<State>,
    compositor: WlCompositor,
    subcompositor: WlSubcompositor,
    viewporter: WpViewporter,
    fractional: Option<WpFractionalScaleManagerV1>,
    dmabuf: ZwpLinuxDmabufV1,
    main_device: u64,
    modifiers: Vec<u64>,
    _outputs: Vec<WlOutput>,
}

static GLOBALS: OnceLock<Result<Arc<Globals>, String>> = OnceLock::new();

fn globals(display: *mut std::ffi::c_void) -> Result<Arc<Globals>> {
    GLOBALS
        .get_or_init(|| connect(display).map(Arc::new).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow!(e))
}

fn connect(display: *mut std::ffi::c_void) -> Result<Globals> {
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue): (_, EventQueue<State>) =
        registry_queue_init(&conn).context("registry")?;
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).context("wl_compositor")?;
    let subcompositor: WlSubcompositor =
        globals.bind(&qh, 1..=1, ()).context("wl_subcompositor")?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ()).context("wp_viewporter")?;
    let fractional: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&qh, 4..=5, ())
        .context("zwp_linux_dmabuf_v1 v4")?;
    let outputs: Vec<WlOutput> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == WlOutput::interface().name)
            .map(|g| {
                globals.registry().bind::<WlOutput, _, State>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    Arc::new(AtomicI32::new(0)),
                )
            })
            .collect()
    });
    let feedback = Arc::new(Mutex::new(Feedback::default()));
    let fb = dmabuf.get_default_feedback(&qh, feedback.clone());
    let mut state = State;
    for _ in 0..4 {
        queue.roundtrip(&mut state).context("roundtrip")?;
        if feedback.lock().unwrap_or_else(|e| e.into_inner()).done {
            break;
        }
    }
    fb.destroy();
    let (main_device, modifiers) = {
        let f = feedback.lock().unwrap_or_else(|e| e.into_inner());
        (
            f.main_device.context("no dma-buf main device")?,
            f.modifiers.clone(),
        )
    };
    std::thread::Builder::new()
        .name("gpui-web-wayland".into())
        .spawn(move || {
            let mut state = State;
            loop {
                if let Err(err) = queue.blocking_dispatch(&mut state) {
                    tracing::warn!("web view: Wayland queue: {err}");
                    return;
                }
            }
        })?;
    Ok(Globals {
        conn,
        qh,
        compositor,
        subcompositor,
        viewporter,
        fractional,
        dmabuf,
        main_device,
        modifiers,
        _outputs: outputs,
    })
}

// --- GBM (loaded at run time) ---

#[allow(non_camel_case_types)]
type gbm_device = std::ffi::c_void;
#[allow(non_camel_case_types)]
type gbm_bo = std::ffi::c_void;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

struct Gbm {
    _lib: libloading::Library,
    create_device: unsafe extern "C" fn(i32) -> *mut gbm_device,
    bo_create: unsafe extern "C" fn(*mut gbm_device, u32, u32, u32, u32) -> *mut gbm_bo,
    bo_create_with_modifiers: Option<
        unsafe extern "C" fn(*mut gbm_device, u32, u32, u32, *const u64, u32) -> *mut gbm_bo,
    >,
    bo_get_fd: unsafe extern "C" fn(*mut gbm_bo) -> i32,
    bo_get_stride: unsafe extern "C" fn(*mut gbm_bo) -> u32,
    bo_get_offset: unsafe extern "C" fn(*mut gbm_bo, i32) -> u32,
    bo_get_modifier: unsafe extern "C" fn(*mut gbm_bo) -> u64,
    bo_get_plane_count: unsafe extern "C" fn(*mut gbm_bo) -> i32,
    bo_destroy: unsafe extern "C" fn(*mut gbm_bo),
}

impl Gbm {
    fn load() -> Result<Self> {
        unsafe {
            let lib = libloading::Library::new("libgbm.so.1").context("loading libgbm.so.1")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)
                        .context(concat!("libgbm: ", stringify!($name)))?
                };
            }
            Ok(Self {
                create_device: sym!(b"gbm_create_device\0"),
                bo_create: sym!(b"gbm_bo_create\0"),
                bo_create_with_modifiers: lib
                    .get(b"gbm_bo_create_with_modifiers\0")
                    .ok()
                    .map(|s| *s),
                bo_get_fd: sym!(b"gbm_bo_get_fd\0"),
                bo_get_stride: sym!(b"gbm_bo_get_stride\0"),
                bo_get_offset: sym!(b"gbm_bo_get_offset\0"),
                bo_get_modifier: sym!(b"gbm_bo_get_modifier\0"),
                bo_get_plane_count: sym!(b"gbm_bo_get_plane_count\0"),
                bo_destroy: sym!(b"gbm_bo_destroy\0"),
                _lib: lib,
            })
        }
    }
}

// --- EGL + GLES blitter, one per process ---

const EGL_PLATFORM_GBM_KHR: khronos_egl::Enum = 0x31d7;
const EGL_LINUX_DMA_BUF_EXT: khronos_egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: usize = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: usize = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: usize = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: usize = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: usize = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: usize = 0x3444;

type ImageTargetFn = unsafe extern "system" fn(u32, *const std::ffi::c_void);

const TEXTURE_EXTERNAL_OES: u32 = 0x8d65;

const VERTEX: &str = "#version 300 es
uniform vec4 u_src;
uniform vec2 u_size;
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = (u_src.xy + p * u_src.zw) / u_size;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}";

const FRAGMENT_EXTERNAL: &str = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_tex;
in vec2 v_uv;
out vec4 color;
void main() { color = texture(u_tex, v_uv); }";

const FRAGMENT_PLAIN: &str = "#version 300 es
precision highp float;
uniform sampler2D u_tex;
in vec2 v_uv;
out vec4 color;
void main() { color = texture(u_tex, v_uv); }";

/// Draws one texture into a framebuffer, texel for pixel.
struct Program {
    program: glow::NativeProgram,
    src: glow::NativeUniformLocation,
    size: glow::NativeUniformLocation,
    target: u32,
}

struct Blitter {
    egl: khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    display: khronos_egl::Display,
    context: khronos_egl::Context,
    gl: glow::Context,
    image_target_texture: ImageTargetFn,
    image_target_renderbuffer: ImageTargetFn,
    gbm: Gbm,
    device: *mut gbm_device,
    _node: std::fs::File,
    external: Program,
    plain: Program,
    /// CEF's buffers imported so far, by dma-buf inode.
    sources: HashMap<u64, Source>,
    /// CPU frames go through this texture.
    upload: Option<(glow::NativeTexture, (i32, i32))>,
}

/// One of CEF's buffers, as a texture.
struct Source {
    image: khronos_egl::Image,
    texture: glow::NativeTexture,
    size: (i32, i32),
}

/// Something to draw into: one of our dma-bufs, or a GL texture.
struct Target {
    image: Option<khronos_egl::Image>,
    renderbuffer: Option<glow::NativeRenderbuffer>,
    texture: Option<glow::NativeTexture>,
    framebuffer: glow::NativeFramebuffer,
    size: (i32, i32),
}

thread_local! {
    static BLITTER: RefCell<Option<Rc<RefCell<Blitter>>>> = const { RefCell::new(None) };
}

fn blitter(main_device: u64) -> Result<Rc<RefCell<Blitter>>> {
    if let Some(b) = BLITTER.with_borrow(|b| b.clone()) {
        return Ok(b);
    }
    let blitter = Rc::new(RefCell::new(Blitter::new(main_device)?));
    BLITTER.with_borrow_mut(|b| *b = Some(blitter.clone()));
    Ok(blitter)
}

fn program(gl: &glow::Context, fragment: &str, target: u32) -> Result<Program> {
    use glow::HasContext as _;
    unsafe {
        let program = gl.create_program().map_err(|e| anyhow!(e))?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                bail!("shader: {}", gl.get_shader_info_log(shader));
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            bail!("program: {}", gl.get_program_info_log(program));
        }
        let uniform = |name| {
            gl.get_uniform_location(program, name)
                .with_context(|| format!("uniform {name}"))
        };
        Ok(Program {
            program,
            src: uniform("u_src")?,
            size: uniform("u_size")?,
            target,
        })
    }
}

impl Blitter {
    fn new(main_device: u64) -> Result<Self> {
        use glow::HasContext as _;
        let node =
            render_node_of(main_device).context("no render node for the compositor's GPU")?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .with_context(|| format!("opening {}", node.display()))?;
        let gbm = Gbm::load()?;
        let device = unsafe { (gbm.create_device)(file.as_raw_fd()) };
        if device.is_null() {
            bail!("gbm_create_device failed on {}", node.display());
        }
        let egl = unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required() }
            .map_err(|e| anyhow!("loading libEGL: {e}"))?;
        let display = unsafe {
            egl.get_platform_display(EGL_PLATFORM_GBM_KHR, device, &[khronos_egl::ATTRIB_NONE])
        }
        .context("eglGetPlatformDisplay(GBM)")?;
        egl.initialize(display).context("eglInitialize")?;
        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .context("eglBindAPI")?;
        let config = egl
            .choose_first_config(
                display,
                &[
                    khronos_egl::RENDERABLE_TYPE,
                    khronos_egl::OPENGL_ES3_BIT,
                    khronos_egl::NONE,
                ],
            )
            .context("eglChooseConfig")?
            .context("no GLES3 config")?;
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[khronos_egl::CONTEXT_MAJOR_VERSION, 3, khronos_egl::NONE],
            )
            .context("eglCreateContext")?;
        egl.make_current(display, None, None, Some(context))
            .context("eglMakeCurrent")?;
        let proc = |name: &str| {
            egl.get_proc_address(name)
                .map(|f| f as *const std::ffi::c_void)
        };
        let gl = unsafe {
            glow::Context::from_loader_function(|name| proc(name).unwrap_or(std::ptr::null()))
        };
        let image_target_texture: ImageTargetFn = unsafe {
            std::mem::transmute(
                proc("glEGLImageTargetTexture2DOES").context("glEGLImageTargetTexture2DOES")?,
            )
        };
        let image_target_renderbuffer: ImageTargetFn = unsafe {
            std::mem::transmute(
                proc("glEGLImageTargetRenderbufferStorageOES")
                    .context("glEGLImageTargetRenderbufferStorageOES")?,
            )
        };
        let external = program(&gl, FRAGMENT_EXTERNAL, TEXTURE_EXTERNAL_OES)?;
        let plain = program(&gl, FRAGMENT_PLAIN, glow::TEXTURE_2D)?;
        tracing::info!("web view: blitting on {} ({})", node.display(), unsafe {
            gl.get_parameter_string(glow::RENDERER)
        });
        egl.make_current(display, None, None, None).ok();
        Ok(Self {
            egl,
            display,
            context,
            gl,
            image_target_texture,
            image_target_renderbuffer,
            gbm,
            device,
            _node: file,
            external,
            plain,
            sources: HashMap::new(),
            upload: None,
        })
    }

    fn current(&self) -> bool {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .is_ok()
    }

    fn release(&self) {
        self.egl.make_current(self.display, None, None, None).ok();
    }

    fn dmabuf_image(
        &self,
        fd: i32,
        fourcc: u32,
        size: (i32, i32),
        offset: u32,
        stride: u32,
        modifier: u64,
    ) -> Result<khronos_egl::Image> {
        let mut attribs = vec![
            khronos_egl::WIDTH as usize,
            size.0 as usize,
            khronos_egl::HEIGHT as usize,
            size.1 as usize,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as usize,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd as usize,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            offset as usize,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as usize,
        ];
        if modifier != DRM_FORMAT_MOD_INVALID {
            attribs.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (modifier & 0xffff_ffff) as usize,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (modifier >> 32) as usize,
            ]);
        }
        attribs.push(khronos_egl::ATTRIB_NONE);
        let no_context = unsafe { khronos_egl::Context::from_ptr(khronos_egl::NO_CONTEXT) };
        let no_buffer = unsafe { khronos_egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        self.egl
            .create_image(
                self.display,
                no_context,
                EGL_LINUX_DMA_BUF_EXT,
                no_buffer,
                &attribs,
            )
            .context("eglCreateImage(dma-buf)")
    }

    fn framebuffer(&self, attach: impl FnOnce(&glow::Context)) -> Result<glow::NativeFramebuffer> {
        use glow::HasContext as _;
        unsafe {
            let framebuffer = self.gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            attach(&self.gl);
            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                self.gl.delete_framebuffer(framebuffer);
                bail!("framebuffer incomplete: {status:#x}");
            }
            Ok(framebuffer)
        }
    }

    /// One of our dma-bufs as a draw target.
    fn dmabuf_target(&self, image: khronos_egl::Image, size: (i32, i32)) -> Result<Target> {
        use glow::HasContext as _;
        unsafe {
            let rb = self.gl.create_renderbuffer().map_err(|e| anyhow!(e))?;
            self.gl.bind_renderbuffer(glow::RENDERBUFFER, Some(rb));
            (self.image_target_renderbuffer)(glow::RENDERBUFFER, image.as_ptr());
            let framebuffer = self.framebuffer(|gl| {
                gl.framebuffer_renderbuffer(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::RENDERBUFFER,
                    Some(rb),
                )
            });
            match framebuffer {
                Ok(framebuffer) => Ok(Target {
                    image: Some(image),
                    renderbuffer: Some(rb),
                    texture: None,
                    framebuffer,
                    size,
                }),
                Err(err) => {
                    self.gl.delete_renderbuffer(rb);
                    self.egl.destroy_image(self.display, image).ok();
                    Err(err)
                }
            }
        }
    }

    /// A GL texture to draw into and read back from (the popup's copy).
    fn texture_target(&self, size: (i32, i32)) -> Result<Target> {
        use glow::HasContext as _;
        unsafe {
            let texture = self.gl.create_texture().map_err(|e| anyhow!(e))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl
                .tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, size.0, size.1);
            set_nearest(&self.gl, glow::TEXTURE_2D);
            let framebuffer = self.framebuffer(|gl| {
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(texture),
                    0,
                )
            });
            match framebuffer {
                Ok(framebuffer) => Ok(Target {
                    image: None,
                    renderbuffer: None,
                    texture: Some(texture),
                    framebuffer,
                    size,
                }),
                Err(err) => {
                    self.gl.delete_texture(texture);
                    Err(err)
                }
            }
        }
    }

    fn free_target(&self, target: Target) {
        use glow::HasContext as _;
        unsafe {
            self.gl.delete_framebuffer(target.framebuffer);
            if let Some(rb) = target.renderbuffer {
                self.gl.delete_renderbuffer(rb);
            }
            if let Some(texture) = target.texture {
                self.gl.delete_texture(texture);
            }
        }
        if let Some(image) = target.image {
            self.egl.destroy_image(self.display, image).ok();
        }
    }

    fn free_source(&self, source: Source) {
        use glow::HasContext as _;
        unsafe { self.gl.delete_texture(source.texture) };
        self.egl.destroy_image(self.display, source.image).ok();
    }

    /// CEF's frame as a texture, imported once per buffer.
    fn source(&mut self, info: &AcceleratedPaintInfo) -> Result<(glow::NativeTexture, (i32, i32))> {
        use cef::sys::cef_color_type_t as C;
        use glow::HasContext as _;
        if info.plane_count < 1 {
            bail!("no planes");
        }
        let plane = &info.planes[0];
        let size = (info.extra.coded_size.width, info.extra.coded_size.height);
        let inode = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(plane.fd, &mut stat) != 0 {
                bail!("fstat on CEF's frame failed");
            }
            stat.st_ino
        };
        if let Some(source) = self.sources.get(&inode)
            && source.size == size
        {
            return Ok((source.texture, size));
        }
        if let Some(old) = self.sources.remove(&inode) {
            self.free_source(old);
        }
        // Chromium cycles through a few buffers; more means it replaced them.
        if self.sources.len() >= 8 {
            for (_, source) in std::mem::take(&mut self.sources) {
                self.free_source(source);
            }
        }
        let fourcc = match C::from(info.format) {
            C::CEF_COLOR_TYPE_RGBA_8888 => DRM_FORMAT_ABGR8888,
            _ => DRM_FORMAT_ARGB8888,
        };
        let image = self.dmabuf_image(
            plane.fd,
            fourcc,
            size,
            plane.offset as u32,
            plane.stride,
            info.modifier,
        )?;
        let texture = unsafe {
            let texture = self.gl.create_texture().map_err(|e| anyhow!(e))?;
            self.gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(texture));
            (self.image_target_texture)(TEXTURE_EXTERNAL_OES, image.as_ptr());
            set_nearest(&self.gl, TEXTURE_EXTERNAL_OES);
            texture
        };
        self.sources.insert(
            inode,
            Source {
                image,
                texture,
                size,
            },
        );
        Ok((texture, size))
    }

    /// A texture holding CPU `pixels` (BGRA, `size`).
    fn upload(&mut self, pixels: &[u8], size: (i32, i32)) -> Result<glow::NativeTexture> {
        use glow::HasContext as _;
        const BGRA: u32 = 0x80e1;
        let gl = &self.gl;
        unsafe {
            let texture = match self.upload {
                Some((texture, s)) if s == size => texture,
                old => {
                    if let Some((texture, _)) = old {
                        gl.delete_texture(texture);
                    }
                    let texture = gl.create_texture().map_err(|e| anyhow!(e))?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        BGRA as i32,
                        size.0,
                        size.1,
                        0,
                        BGRA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None),
                    );
                    set_nearest(gl, glow::TEXTURE_2D);
                    self.upload = Some((texture, size));
                    texture
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                size.0,
                size.1,
                BGRA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(pixels)),
            );
            Ok(texture)
        }
    }

    /// Draws texels `src` (x, y, w, h) of `texture` (of `size`) into area
    /// `dst` of `target`, one texel per pixel.
    fn draw(
        &self,
        program: &Program,
        texture: glow::NativeTexture,
        size: (i32, i32),
        src: (i32, i32, i32, i32),
        target: &Target,
        dst: (i32, i32),
    ) {
        use glow::HasContext as _;
        let gl = &self.gl;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.viewport(dst.0, dst.1, src.2, src.3);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.use_program(Some(program.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(program.target, Some(texture));
            gl.uniform_4_f32(
                Some(&program.src),
                src.0 as f32,
                src.1 as f32,
                src.2 as f32,
                src.3 as f32,
            );
            gl.uniform_2_f32(Some(&program.size), size.0 as f32, size.1 as f32);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    fn finish(&self) {
        use glow::HasContext as _;
        unsafe { self.gl.finish() };
    }
}

fn set_nearest(gl: &glow::Context, target: u32) {
    use glow::HasContext as _;
    unsafe {
        gl.tex_parameter_i32(target, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(target, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }
}

/// A frame from CEF: a dma-buf, or BGRA pixels.
enum Frame<'a> {
    Gpu(&'a AcceleratedPaintInfo),
    Cpu(&'a [u8]),
}

/// One buffer of ours: a GBM buffer the compositor shows and GL draws to.
struct PoolBuffer {
    bo: *mut gbm_bo,
    target: Target,
    buffer: WlBuffer,
    released: Arc<AtomicBool>,
}

impl PoolBuffer {
    fn new(globals: &Globals, blitter: &Blitter, size: (i32, i32)) -> Result<Self> {
        let gbm = &blitter.gbm;
        let (w, h) = (size.0 as u32, size.1 as u32);
        let modifiers: Vec<u64> = globals
            .modifiers
            .iter()
            .copied()
            .filter(|m| *m != DRM_FORMAT_MOD_INVALID)
            .collect();
        let mut bo = std::ptr::null_mut();
        if let (Some(create), false) = (gbm.bo_create_with_modifiers, modifiers.is_empty()) {
            bo = unsafe {
                create(
                    blitter.device,
                    w,
                    h,
                    DRM_FORMAT_XRGB8888,
                    modifiers.as_ptr(),
                    modifiers.len() as u32,
                )
            };
        }
        if bo.is_null() {
            bo = unsafe {
                (gbm.bo_create)(
                    blitter.device,
                    w,
                    h,
                    DRM_FORMAT_XRGB8888,
                    GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
                )
            };
        }
        if bo.is_null() {
            bail!("gbm_bo_create {w}x{h} failed");
        }
        let fail = |e: anyhow::Error| {
            unsafe { (gbm.bo_destroy)(bo) };
            e
        };
        unsafe {
            if (gbm.bo_get_plane_count)(bo) != 1 {
                return Err(fail(anyhow!("multi-plane buffers aren't supported")));
            }
        }
        let fd = unsafe { (gbm.bo_get_fd)(bo) };
        if fd < 0 {
            return Err(fail(anyhow!("gbm_bo_get_fd failed")));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let (stride, offset, modifier) = unsafe {
            (
                (gbm.bo_get_stride)(bo),
                (gbm.bo_get_offset)(bo, 0),
                (gbm.bo_get_modifier)(bo),
            )
        };
        let image = blitter
            .dmabuf_image(
                fd.as_raw_fd(),
                DRM_FORMAT_XRGB8888,
                size,
                offset,
                stride,
                modifier,
            )
            .map_err(fail)?;
        let target = blitter.dmabuf_target(image, size).map_err(fail)?;
        let params = globals.dmabuf.create_params(&globals.qh, ());
        params.add(
            fd.as_fd(),
            0,
            offset,
            stride,
            (modifier >> 32) as u32,
            (modifier & 0xffff_ffff) as u32,
        );
        let released = Arc::new(AtomicBool::new(true));
        let buffer = params.create_immed(
            size.0,
            size.1,
            DRM_FORMAT_XRGB8888,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &globals.qh,
            released.clone(),
        );
        params.destroy();
        Ok(Self {
            bo,
            target,
            buffer,
            released,
        })
    }

    fn free(self, blitter: &Blitter) {
        self.buffer.destroy();
        blitter.free_target(self.target);
        unsafe { (blitter.gbm.bo_destroy)(self.bo) };
    }
}

pub(crate) struct WaylandPresenter {
    globals: Arc<Globals>,
    blitter: Rc<RefCell<Blitter>>,
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    fractional: Option<WpFractionalScaleV1>,
    scale: Arc<AtomicU32>,
    outputs: Arc<Mutex<Vec<WlOutput>>>,
    pool: Vec<PoolBuffer>,
    /// Buffers of an old size, freed once the compositor lets go of them.
    retired: Vec<PoolBuffer>,
    pool_size: (i32, i32),
    /// The pool buffer last attached.
    attached: Option<usize>,
    shown: bool,
    /// Position and size on screen (logical px, relative to GPUI's surface).
    position: Option<(i32, i32)>,
    destination: Option<(i32, i32)>,
    source: Option<(f64, f64, f64, f64)>,
    /// The subsurface moves with GPUI's frames until a buffer of this size
    /// arrives.
    sync_until: Option<(i32, i32)>,
    synced: bool,
    desync_next_frame: bool,
    expected: (i32, i32),
    popup: Option<Target>,
    popup_rect: Option<Rect>,
    popup_shown: bool,
    browser: Option<Browser>,
    stats: super::Stats,
}

impl WaylandPresenter {
    pub fn new(window: &mut Window) -> Result<Self> {
        let display = match window.display_handle()?.as_raw() {
            RawDisplayHandle::Wayland(h) => h.display.as_ptr(),
            other => bail!("not a Wayland window ({other:?})"),
        };
        let parent = match HasWindowHandle::window_handle(window)?.as_raw() {
            RawWindowHandle::Wayland(h) => h.surface.as_ptr(),
            other => bail!("not a Wayland window ({other:?})"),
        };
        let globals = globals(display)?;
        let blitter = blitter(globals.main_device)?;
        let parent_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), parent.cast()) }
            .map_err(|e| anyhow!("GPUI's surface: {e:?}"))?;
        let parent = WlSurface::from_id(&globals.conn, parent_id)
            .map_err(|e| anyhow!("GPUI's surface: {e:?}"))?;
        let qh = &globals.qh;
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let surface = globals.compositor.create_surface(qh, outputs.clone());
        let subsurface = globals
            .subcompositor
            .get_subsurface(&surface, &parent, qh, ());
        subsurface.place_below(&parent);
        subsurface.set_desync();
        // Input goes to GPUI, whose surface covers ours.
        let region = globals.compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let viewport = globals.viewporter.get_viewport(&surface, qh, ());
        let scale = Arc::new(AtomicU32::new(0));
        let fractional = globals
            .fractional
            .as_ref()
            .map(|f| f.get_fractional_scale(&surface, qh, scale.clone()));
        surface.commit();
        globals.conn.flush().ok();
        Ok(Self {
            globals,
            blitter,
            surface,
            subsurface,
            viewport,
            fractional,
            scale,
            outputs,
            pool: Vec::new(),
            retired: Vec::new(),
            pool_size: (0, 0),
            attached: None,
            shown: false,
            position: None,
            destination: None,
            source: None,
            sync_until: None,
            synced: false,
            desync_next_frame: false,
            expected: (0, 0),
            popup: None,
            popup_rect: None,
            popup_shown: false,
            browser: None,
            stats: super::Stats::default(),
        })
    }

    fn set_sync(&mut self, sync: bool) {
        if sync != self.synced {
            self.synced = sync;
            if sync {
                self.subsurface.set_sync();
            } else {
                self.subsurface.set_desync();
            }
        }
    }

    fn free_retired(&mut self) {
        let blitter = self.blitter.borrow();
        let mut keep = Vec::new();
        for buffer in self.retired.drain(..) {
            if buffer.released.load(Ordering::Acquire) {
                buffer.free(&blitter);
            } else {
                keep.push(buffer);
            }
        }
        self.retired = keep;
    }

    /// A pool buffer of `size` the compositor isn't using.
    fn free_buffer(&mut self, size: (i32, i32)) -> Result<usize> {
        if self.pool_size != size {
            self.retired.append(&mut self.pool);
            self.attached = None;
            self.pool_size = size;
        }
        self.free_retired();
        if let Some(ix) = (0..self.pool.len())
            .find(|&ix| Some(ix) != self.attached && self.pool[ix].released.load(Ordering::Acquire))
        {
            return Ok(ix);
        }
        if self.pool.len() < POOL + 1 {
            let buffer = PoolBuffer::new(&self.globals, &self.blitter.borrow(), size)?;
            self.pool.push(buffer);
            return Ok(self.pool.len() - 1);
        }
        bail!("the compositor holds every buffer")
    }

    /// Copies CEF's frame (and the open popup) into a free buffer and puts
    /// it on screen; returns whether GPUI must commit for it to show.
    fn present(&mut self, size: (i32, i32), frame: Frame<'_>) -> Result<bool> {
        if size.0 <= 0 || size.1 <= 0 {
            return Ok(false);
        }
        if !self.blitter.borrow().current() {
            bail!("eglMakeCurrent failed");
        }
        let result = (|| {
            let ix = self.free_buffer(size)?;
            let mut blitter = self.blitter.borrow_mut();
            let (program, texture, frame_size) = match frame {
                Frame::Gpu(info) => {
                    let (texture, frame_size) = blitter.source(info)?;
                    (true, texture, frame_size)
                }
                Frame::Cpu(pixels) => (false, blitter.upload(pixels, size)?, size),
            };
            let blitter = &*blitter;
            let program = if program {
                &blitter.external
            } else {
                &blitter.plain
            };
            let target = &self.pool[ix].target;
            blitter.draw(
                program,
                texture,
                frame_size,
                (0, 0, size.0, size.1),
                target,
                (0, 0),
            );
            if let (true, Some(popup), Some(rect)) =
                (self.popup_shown, &self.popup, &self.popup_rect)
                && let Some(popup_texture) = popup.texture
            {
                let (w, h) = popup.size;
                blitter.draw(
                    &blitter.plain,
                    popup_texture,
                    popup.size,
                    (0, 0, w, h),
                    target,
                    (rect.x, rect.y),
                );
            }
            blitter.finish();
            Ok::<_, anyhow::Error>(ix)
        })();
        self.blitter.borrow().release();
        let ix = result?;
        self.pool[ix].released.store(false, Ordering::Release);
        self.attached = Some(ix);
        self.stats.frame();
        if self.shown {
            self.surface.attach(Some(&self.pool[ix].buffer), 0, 0);
            self.surface.damage_buffer(0, 0, size.0, size.1);
            self.apply_source(size);
            self.surface.commit();
            self.globals.conn.flush().ok();
        }
        // A resize waits for its first frame of the new size, then goes back
        // to updating on its own.
        if self.sync_until.is_some_and(|until| until == size) {
            self.sync_until = None;
            self.desync_next_frame = true;
        }
        Ok(self.synced)
    }

    /// Crops the buffer to the visible part of the page.
    fn apply_source(&mut self, size: (i32, i32)) {
        let source = self.source.map(|(x, y, w, h)| {
            // `source` is relative to the expected size; scale to this buffer.
            let kx = size.0 as f64 / self.expected.0.max(1) as f64;
            let ky = size.1 as f64 / self.expected.1.max(1) as f64;
            (x * kx, y * ky, w * kx, h * ky)
        });
        match source {
            Some((x, y, w, h)) => self.viewport.set_source(x, y, w, h),
            None => self.viewport.set_source(-1., -1., -1., -1.),
        }
    }

    fn upload_popup(&mut self, pixels: &[u8], size: (i32, i32)) -> Result<()> {
        use glow::HasContext as _;
        let blitter_rc = self.blitter.clone();
        let blitter = blitter_rc.borrow();
        if !blitter.current() {
            bail!("eglMakeCurrent failed");
        }
        let result = (|| {
            if self.popup.as_ref().is_none_or(|p| p.size != size) {
                if let Some(old) = self.popup.take() {
                    blitter.free_target(old);
                }
                self.popup = Some(blitter.texture_target(size)?);
            }
            let texture = self
                .popup
                .as_ref()
                .and_then(|p| p.texture)
                .context("popup")?;
            unsafe {
                // The popup's texture is RGBA; swap in the upload instead.
                blitter.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                let rgba: Vec<u8> = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect();
                blitter.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    size.0,
                    size.1,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&rgba)),
                );
            }
            Ok(())
        })();
        blitter.release();
        result
    }

    fn copy_popup(&mut self, info: &AcceleratedPaintInfo) -> Result<()> {
        let size = (info.extra.coded_size.width, info.extra.coded_size.height);
        let blitter_rc = self.blitter.clone();
        let mut blitter = blitter_rc.borrow_mut();
        if !blitter.current() {
            bail!("eglMakeCurrent failed");
        }
        let result = (|| {
            if self.popup.as_ref().is_none_or(|p| p.size != size) {
                if let Some(old) = self.popup.take() {
                    blitter.free_target(old);
                }
                self.popup = Some(blitter.texture_target(size)?);
            }
            let (texture, frame) = blitter.source(info)?;
            let target = self.popup.as_ref().context("popup")?;
            blitter.draw(
                &blitter.external,
                texture,
                frame,
                (0, 0, size.0, size.1),
                target,
                (0, 0),
            );
            Ok(())
        })();
        blitter.release();
        result
    }
}

impl Presenter for WaylandPresenter {
    fn punches_hole(&self) -> bool {
        true
    }

    fn has_content(&self) -> bool {
        self.attached.is_some()
    }

    fn scale(&self) -> Option<f32> {
        let scale = self.scale.load(Ordering::Relaxed);
        (scale > 0).then(|| scale as f32 / 120.)
    }

    fn refresh_rate(&self) -> Option<i32> {
        let outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        outputs
            .iter()
            .filter_map(|o| o.data::<Arc<AtomicI32>>())
            .map(|r| r.load(Ordering::Relaxed))
            .max()
            .filter(|mhz| *mhz > 0)
            .map(|mhz| (mhz + 500) / 1000)
    }

    fn place(&mut self, place: &Placement, window: &mut Window, _cx: &mut App) {
        let visible = place.visible;
        if visible.size.width <= gpui::px(0.) || visible.size.height <= gpui::px(0.) {
            self.hide();
            return;
        }
        let scale = self.scale().unwrap_or_else(|| window.scale_factor());
        let logical = |v: gpui::Pixels| f32::from(v).round() as i32;
        let position = (logical(visible.origin.x), logical(visible.origin.y));
        let destination = (logical(visible.size.width), logical(visible.size.height));
        let bounds = place.bounds;
        self.expected = (
            (f32::from(bounds.size.width) * scale).round() as i32,
            (f32::from(bounds.size.height) * scale).round() as i32,
        );
        let source = (visible != bounds).then(|| {
            let k = scale as f64;
            (
                f32::from(visible.origin.x - bounds.origin.x) as f64 * k,
                f32::from(visible.origin.y - bounds.origin.y) as f64 * k,
                f32::from(visible.size.width) as f64 * k,
                f32::from(visible.size.height) as f64 * k,
            )
        });
        let moved = self.position != Some(position)
            || self.destination != Some(destination)
            || self.source != source;
        let mut commit = false;
        if moved {
            // Move in step with GPUI's frame: the position applies on GPUI's
            // commit, and so does our state while synchronized.
            self.set_sync(true);
            self.subsurface.set_position(position.0, position.1);
            self.viewport.set_destination(destination.0, destination.1);
            self.position = Some(position);
            self.destination = Some(destination);
            self.source = source;
            self.apply_source(self.pool_size);
            commit = true;
            if self.pool_size != self.expected {
                self.sync_until = Some(self.expected);
                self.desync_next_frame = false;
            } else {
                self.desync_next_frame = true;
            }
        } else if self.desync_next_frame && self.sync_until.is_none() {
            self.desync_next_frame = false;
            self.set_sync(false);
        }
        if !self.shown {
            self.shown = true;
            if let Some(ix) = self.attached {
                self.surface.attach(Some(&self.pool[ix].buffer), 0, 0);
                self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                commit = true;
            }
        }
        if commit {
            self.surface.commit();
        }
        self.globals.conn.flush().ok();
    }

    fn hide(&mut self) {
        if !self.shown {
            return;
        }
        self.shown = false;
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.globals.conn.flush().ok();
    }

    fn on_accelerated_paint(
        &mut self,
        kind: PaintElementType,
        info: &AcceleratedPaintInfo,
    ) -> bool {
        if kind == PaintElementType::POPUP {
            if let Err(err) = self.copy_popup(info) {
                tracing::warn!("web view: popup frame: {err:#}");
            }
            // Drawn with the next page frame.
            if let Some(host) = self.browser.as_ref().and_then(|b| b.host()) {
                host.invalidate(PaintElementType::VIEW);
            }
            return false;
        }
        let size = (info.extra.coded_size.width, info.extra.coded_size.height);
        match self.present(size, Frame::Gpu(info)) {
            Ok(redraw) => redraw,
            Err(err) => {
                tracing::warn!("web view: frame: {err:#}");
                false
            }
        }
    }

    fn on_paint(
        &mut self,
        kind: PaintElementType,
        _dirty: &[Rect],
        pixels: &[u8],
        width: i32,
        height: i32,
    ) -> bool {
        if kind == PaintElementType::POPUP {
            if let Err(err) = self.upload_popup(pixels, (width, height)) {
                tracing::warn!("web view: popup frame: {err:#}");
            }
            if let Some(host) = self.browser.as_ref().and_then(|b| b.host()) {
                host.invalidate(PaintElementType::VIEW);
            }
            return false;
        }
        match self.present((width, height), Frame::Cpu(pixels)) {
            Ok(redraw) => redraw,
            Err(err) => {
                tracing::warn!("web view: frame: {err:#}");
                false
            }
        }
    }

    fn on_popup_show(&mut self, show: bool) {
        self.popup_shown = show;
    }

    fn on_popup_size(&mut self, rect: Rect) {
        self.popup_rect = Some(rect);
    }

    fn set_browser(&mut self, browser: &Browser) {
        if self.browser.is_none() {
            self.browser = Some(browser.clone());
        }
    }
}

impl Drop for WaylandPresenter {
    fn drop(&mut self) {
        {
            let blitter = self.blitter.borrow();
            if blitter.current() {
                for buffer in self.pool.drain(..).chain(self.retired.drain(..)) {
                    buffer.free(&blitter);
                }
                if let Some(popup) = self.popup.take() {
                    blitter.free_target(popup);
                }
                blitter.release();
            }
        }
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        self.viewport.destroy();
        self.subsurface.destroy();
        self.surface.destroy();
        self.globals.conn.flush().ok();
    }
}
