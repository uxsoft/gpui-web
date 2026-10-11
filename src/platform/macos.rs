//! macOS: CEF inside GPUI's `NSApplication`, and pages in a layer of our
//! own behind GPUI's view.
//!
//! GPUI's `surface` element only draws 4:2:0 video frames, which would blur
//! text, so pages are composited by Core Animation instead, like on Wayland:
//! a layer-backed `NSView` below GPUI's view shows each frame (one Metal
//! blit from CEF's IOSurface into a small pool of our own), and GPUI's
//! transparent window leaves a hole for it.

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, bail};
use cef::{
    AcceleratedPaintInfo, Browser, ImplBrowser as _, ImplBrowserHost as _, PaintElementType, Rect,
    Settings,
};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_video::pixel_buffer::{
    CVPixelBuffer, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelFormatType_32BGRA,
};
use core_video::pixel_buffer_io_surface::CVPixelBufferGetIOSurface;
use gpui::{App, Window};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder as _, MTLCommandBuffer as _, MTLCommandEncoder as _, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLSize, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{Placement, Presenter};

/// Frames in flight: one shown, one queued, one being drawn.
const POOL: usize = 3;

/// Loads the Chromium Embedded Framework from the app bundle (helpers live
/// three levels deeper than the app). Keeps it loaded for the process.
pub(crate) fn load_framework() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let helper = exe.to_string_lossy().contains(" Helper");
    let loaded = std::panic::catch_unwind(|| {
        let loader = cef::library_loader::LibraryLoader::new(&exe, helper);
        let ok = loader.load();
        std::mem::forget(loader);
        ok
    });
    loaded.unwrap_or(false)
}

/// Paths into the app bundle: the framework, the bundle, and the helper.
pub(crate) fn settings() -> Settings {
    let mut settings = Settings::default();
    let Ok(exe) = std::env::current_exe() else {
        return settings;
    };
    // .../Wisp.app/Contents/MacOS/wisp
    let (Some(contents), Some(name)) = (exe.parent().and_then(|p| p.parent()), exe.file_name())
    else {
        return settings;
    };
    let name = name.to_string_lossy();
    let frameworks = contents.join("Frameworks");
    let path = |p: &std::path::Path| cef::CefString::from(p.to_string_lossy().as_ref());
    settings.framework_dir_path = path(&frameworks.join("Chromium Embedded Framework.framework"));
    if let Some(bundle) = contents.parent() {
        settings.main_bundle_path = path(bundle);
    }
    settings.browser_subprocess_path = path(
        &frameworks
            .join(format!("{name} Helper.app"))
            .join("Contents/MacOS")
            .join(format!("{name} Helper")),
    );
    settings
}

/// Whether `NSApp` is inside `sendEvent:`, which Chromium asks about.
static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
static ORIGINAL_SEND_EVENT: OnceLock<Imp> = OnceLock::new();

extern "C-unwind" fn is_handling_send_event(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling_send_event(_this: &AnyObject, _cmd: Sel, value: Bool) {
    HANDLING_SEND_EVENT.store(value.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(this: &AnyObject, cmd: Sel, event: *mut AnyObject) {
    let was = HANDLING_SEND_EVENT.swap(true, Ordering::Relaxed);
    if let Some(original) = ORIGINAL_SEND_EVENT.get() {
        // SAFETY: the original `sendEvent:` implementation of NSApp's class.
        let original: extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) =
            unsafe { std::mem::transmute(*original) };
        original(this, cmd, event);
    }
    HANDLING_SEND_EVENT.store(was, Ordering::Relaxed);
}

/// Makes GPUI's `NSApplication` subclass speak `CefAppProtocol` (CEF
/// requires it), by adding the methods at run time: no fork of GPUI.
pub(crate) fn install_app_protocol() {
    #[cfg(target_arch = "aarch64")]
    const BOOL_TYPE: &CStr = c"B";
    #[cfg(not(target_arch = "aarch64"))]
    const BOOL_TYPE: &CStr = c"c";
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app.as_ref() else { return };
        let cls = app.class() as *const AnyClass as *mut AnyClass;
        let types = |ret: &CStr, args: &str| {
            std::ffi::CString::new(format!("{}@:{args}", ret.to_string_lossy())).unwrap()
        };
        let getter = types(BOOL_TYPE, "");
        let setter = types(c"v", &BOOL_TYPE.to_string_lossy());
        let is_handling: Imp = std::mem::transmute(
            is_handling_send_event as extern "C-unwind" fn(&AnyObject, Sel) -> Bool,
        );
        let set_handling: Imp = std::mem::transmute(
            set_handling_send_event as extern "C-unwind" fn(&AnyObject, Sel, Bool),
        );
        objc2::ffi::class_addMethod(cls, sel!(isHandlingSendEvent), is_handling, getter.as_ptr());
        objc2::ffi::class_addMethod(
            cls,
            sel!(setHandlingSendEvent:),
            set_handling,
            setter.as_ptr(),
        );
        objc2::ffi::class_addMethod(
            cls,
            sel!(_setHandlingSendEvent:),
            set_handling,
            setter.as_ptr(),
        );
        // Wrap sendEvent: so the flag is set while it runs.
        let method = objc2::ffi::class_getInstanceMethod(cls, sel!(sendEvent:));
        if !method.is_null()
            && let Some(original) = objc2::ffi::method_getImplementation(method)
        {
            ORIGINAL_SEND_EVENT.set(original).ok();
            let ours: Imp = std::mem::transmute(
                send_event as extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject),
            );
            if !objc2::ffi::class_addMethod(cls, sel!(sendEvent:), ours, c"v@:@".as_ptr()).as_bool()
            {
                objc2::ffi::method_setImplementation(method, ours);
            }
        }
        for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
            let protocol = objc2::ffi::objc_getProtocol(name.as_ptr());
            if !protocol.is_null() {
                objc2::ffi::class_addProtocol(cls, protocol);
            }
        }
    }
}

/// Runs `f` without Core Animation's implicit animations.
fn without_animation(f: impl FnOnce()) {
    unsafe {
        let _: () = msg_send![class!(CATransaction), begin];
        let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        f();
        let _: () = msg_send![class!(CATransaction), commit];
    }
}

pub(crate) struct LayerPresenter {
    /// GPUI's view, and ours below it in the same superview.
    gpui_view: *mut AnyObject,
    view: *mut AnyObject,
    layer: *mut AnyObject,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pool: Vec<CVPixelBuffer>,
    pool_size: (usize, usize),
    next: usize,
    has_frame: bool,
    shown: bool,
    frame: Option<NSRect>,
    scale: f64,
    popup: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    popup_rect: Option<Rect>,
    popup_shown: bool,
    browser: Option<Browser>,
}

impl LayerPresenter {
    pub fn new(window: &mut Window) -> Result<Self> {
        let gpui_view = match HasWindowHandle::window_handle(window)?.as_raw() {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as *mut AnyObject,
            other => bail!("not an AppKit window ({other:?})"),
        };
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        unsafe {
            let superview: *mut AnyObject = msg_send![gpui_view, superview];
            if superview.is_null() {
                bail!("GPUI's view has no superview");
            }
            let view: *mut AnyObject = msg_send![class!(NSView), alloc];
            let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(1., 1.));
            let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
            let _: () = msg_send![view, setWantsLayer: true];
            let _: () = msg_send![view, setHidden: true];
            let layer: *mut AnyObject = msg_send![view, layer];
            let opaque: Bool = Bool::YES;
            let _: () = msg_send![layer, setOpaque: opaque];
            // NSWindowBelow, relative to nothing: under every subview.
            let _: () = msg_send![superview, addSubview: view, positioned: -1isize, relativeTo: std::ptr::null_mut::<AnyObject>()];
            Ok(Self {
                gpui_view,
                view,
                layer,
                device,
                queue,
                pool: Vec::new(),
                pool_size: (0, 0),
                next: 0,
                has_frame: false,
                shown: false,
                frame: None,
                scale: 1.,
                popup: None,
                popup_rect: None,
                popup_shown: false,
                browser: None,
            })
        }
    }

    fn pixel_buffer(&mut self, size: (usize, usize)) -> Result<CVPixelBuffer> {
        if self.pool_size != size {
            self.pool.clear();
            self.pool_size = size;
            self.next = 0;
        }
        if self.pool.len() < POOL {
            let options: CFDictionary<CFString, CFType> = unsafe {
                CFDictionary::from_CFType_pairs(&[
                    (
                        CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey),
                        CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]).as_CFType(),
                    ),
                    (
                        CFString::wrap_under_get_rule(kCVPixelBufferMetalCompatibilityKey),
                        CFBoolean::true_value().as_CFType(),
                    ),
                ])
            };
            let buffer =
                CVPixelBuffer::new(kCVPixelFormatType_32BGRA, size.0, size.1, Some(&options))
                    .map_err(|status| anyhow::anyhow!("CVPixelBufferCreate failed ({status})"))?;
            self.pool.push(buffer);
        }
        let buffer = self.pool[self.next % self.pool.len()].clone();
        self.next = self.next.wrapping_add(1);
        Ok(buffer)
    }

    fn texture(
        &self,
        surface: *const c_void,
        size: (usize, usize),
    ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        // SAFETY: an IOSurfaceRef, alive for the call.
        let surface = unsafe { (surface as *const IOSurfaceRef).as_ref()? };
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                size.0,
                size.1,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        self.device
            .newTextureWithDescriptor_iosurface_plane(&descriptor, surface, 0)
    }

    fn copy(
        &self,
        from: &ProtocolObject<dyn MTLTexture>,
        size: (usize, usize),
        to: &ProtocolObject<dyn MTLTexture>,
        at: (usize, usize),
    ) -> Result<()> {
        let buffer = self
            .queue
            .commandBuffer()
            .context("no Metal command buffer")?;
        let blit = buffer
            .blitCommandEncoder()
            .context("no Metal blit encoder")?;
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                from,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize { width: size.0, height: size.1, depth: 1 },
                to,
                0,
                0,
                MTLOrigin { x: at.0, y: at.1, z: 0 },
            );
        }
        blit.endEncoding();
        buffer.commit();
        buffer.waitUntilCompleted();
        Ok(())
    }

    fn present(&mut self, info: &AcceleratedPaintInfo) -> Result<()> {
        let size = (
            info.extra.coded_size.width.max(0) as usize,
            info.extra.coded_size.height.max(0) as usize,
        );
        if size.0 == 0 || size.1 == 0 || info.shared_texture_io_surface.is_null() {
            return Ok(());
        }
        let source = self
            .texture(info.shared_texture_io_surface, size)
            .context("CEF's IOSurface")?;
        let buffer = self.pixel_buffer(size)?;
        let surface = unsafe { CVPixelBufferGetIOSurface(buffer.as_concrete_TypeRef()) };
        let target = self
            .texture(surface as *const c_void, size)
            .context("our IOSurface")?;
        self.copy(&source, size, &target, (0, 0))?;
        if let (true, Some(popup), Some(rect)) = (self.popup_shown, &self.popup, &self.popup_rect) {
            let popup_size = (popup.width(), popup.height());
            let fits = rect.x >= 0
                && rect.y >= 0
                && rect.x as usize + popup_size.0 <= size.0
                && rect.y as usize + popup_size.1 <= size.1;
            if fits {
                self.copy(
                    popup,
                    popup_size,
                    &target,
                    (rect.x as usize, rect.y as usize),
                )?;
            }
        }
        let layer = self.layer;
        let scale = self.scale;
        without_animation(|| unsafe {
            let _: () = msg_send![layer, setContents: surface as *mut AnyObject];
            let _: () = msg_send![layer, setContentsScale: scale];
        });
        self.has_frame = true;
        Ok(())
    }

    fn copy_popup(&mut self, info: &AcceleratedPaintInfo) -> Result<()> {
        let size = (
            info.extra.coded_size.width.max(0) as usize,
            info.extra.coded_size.height.max(0) as usize,
        );
        if size.0 == 0 || size.1 == 0 || info.shared_texture_io_surface.is_null() {
            return Ok(());
        }
        let source = self
            .texture(info.shared_texture_io_surface, size)
            .context("CEF's popup")?;
        if self
            .popup
            .as_ref()
            .is_none_or(|p| (p.width(), p.height()) != size)
        {
            let descriptor = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    size.0,
                    size.1,
                    false,
                )
            };
            self.popup = self.device.newTextureWithDescriptor(&descriptor);
        }
        let popup = self.popup.clone().context("popup texture")?;
        self.copy(&source, size, &popup, (0, 0))
    }
}

impl Presenter for LayerPresenter {
    fn punches_hole(&self) -> bool {
        true
    }

    fn has_content(&self) -> bool {
        self.has_frame
    }

    fn place(&mut self, place: &Placement, window: &mut Window, _cx: &mut App) {
        self.scale = window.scale_factor() as f64;
        let bounds = place.visible;
        unsafe {
            let superview: *mut AnyObject = msg_send![self.gpui_view, superview];
            if superview.is_null() {
                return;
            }
            let flipped: Bool = msg_send![superview, isFlipped];
            let host: NSRect = msg_send![superview, bounds];
            let (x, y, w, h) = (
                f32::from(bounds.origin.x) as f64,
                f32::from(bounds.origin.y) as f64,
                f32::from(bounds.size.width) as f64,
                f32::from(bounds.size.height) as f64,
            );
            let y = if flipped.as_bool() {
                y
            } else {
                host.size.height - y - h
            };
            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
            let view = self.view;
            let moved = self.frame != Some(frame);
            let show = !self.shown;
            if moved || show {
                without_animation(|| {
                    if moved {
                        let _: () = msg_send![view, setFrame: frame];
                    }
                    if show {
                        let _: () = msg_send![view, setHidden: false];
                    }
                });
            }
            self.frame = Some(frame);
            self.shown = true;
        }
    }

    fn hide(&mut self) {
        if !self.shown {
            return;
        }
        self.shown = false;
        let view = self.view;
        without_animation(|| unsafe {
            let _: () = msg_send![view, setHidden: true];
        });
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
            if let Some(host) = self.browser.as_ref().and_then(|b| b.host()) {
                host.invalidate(PaintElementType::VIEW);
            }
            return false;
        }
        let first = !self.has_frame;
        if let Err(err) = self.present(info) {
            tracing::warn!("web view: frame: {err:#}");
        }
        // GPUI punches the hole once the first frame is up.
        first
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

impl Drop for LayerPresenter {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.view, removeFromSuperview];
            let _: () = msg_send![self.view, release];
        }
    }
}
