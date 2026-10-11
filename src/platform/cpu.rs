//! Frames on the CPU, drawn by GPUI as an image: the X11 path, and the
//! fallback wherever the GPU paths can't start. Costs a CPU copy and an
//! upload per frame.

use std::sync::Arc;

use cef::{AcceleratedPaintInfo, PaintElementType, Rect};
use gpui::{App, Corners, RenderImage, Window};

use super::{Placement, Presenter};

#[derive(Default)]
pub(crate) struct CpuPresenter {
    /// The page, BGRA.
    view: Frame,
    popup: Frame,
    popup_rect: Option<Rect>,
    popup_shown: bool,
    /// Whether `view` or the popup changed since the image was made.
    changed: bool,
    image: Option<Arc<RenderImage>>,
    /// Images to release from the GPU, once GPUI no longer draws them.
    retired: Vec<Arc<RenderImage>>,
    stats: super::Stats,
}

#[derive(Default)]
struct Frame {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Frame {
    /// Takes `pixels` (tightly packed) as the new contents.
    fn set(&mut self, pixels: &[u8], width: i32, height: i32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.extend_from_slice(pixels);
    }

    /// Takes the rows of a buffer laid out with `stride`.
    #[cfg(target_os = "linux")]
    fn set_strided(&mut self, data: &[u8], stride: usize, width: i32, height: i32) {
        let row = width as usize * 4;
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.reserve(row * height as usize);
        for y in 0..height as usize {
            let start = y * stride;
            self.pixels.extend_from_slice(&data[start..start + row]);
        }
    }
}

impl CpuPresenter {
    fn composed(&self) -> Option<Vec<u8>> {
        let (w, h) = (self.view.width, self.view.height);
        if w <= 0 || h <= 0 || self.view.pixels.len() != (w * h * 4) as usize {
            return None;
        }
        let mut pixels = self.view.pixels.clone();
        if let (true, Some(rect)) = (self.popup_shown, &self.popup_rect)
            && self.popup.width > 0
        {
            let row = |y: i32| y * w * 4;
            for py in 0..self.popup.height.min(rect.height) {
                let y = rect.y + py;
                if y < 0 || y >= h {
                    continue;
                }
                let x0 = rect.x.max(0);
                let x1 = (rect.x + self.popup.width.min(rect.width)).min(w);
                if x1 <= x0 {
                    continue;
                }
                let src = ((py * self.popup.width + (x0 - rect.x)) * 4) as usize;
                let dst = (row(y) + x0 * 4) as usize;
                let len = ((x1 - x0) * 4) as usize;
                pixels[dst..dst + len].copy_from_slice(&self.popup.pixels[src..src + len]);
            }
        }
        Some(pixels)
    }
}

impl Presenter for CpuPresenter {
    fn punches_hole(&self) -> bool {
        false
    }

    fn has_content(&self) -> bool {
        self.image.is_some()
    }

    fn place(&mut self, place: &Placement, window: &mut Window, _cx: &mut App) {
        for image in self.retired.drain(..) {
            window.drop_image(image).ok();
        }
        if self.changed {
            self.changed = false;
            // Without a popup the frame moves into the image: CEF hands over
            // whole frames, so there's nothing to keep.
            let pixels = if self.popup_shown {
                self.composed()
            } else {
                Some(std::mem::take(&mut self.view.pixels)).filter(|p| {
                    p.len() == (self.view.width * self.view.height * 4).max(0) as usize
                        && !p.is_empty()
                })
            };
            if let Some(pixels) = pixels {
                let (w, h) = (self.view.width as u32, self.view.height as u32);
                if let Some(buffer) = image::RgbaImage::from_raw(w, h, pixels) {
                    let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                    if let Some(old) = self.image.replace(image) {
                        self.retired.push(old);
                    }
                    self.stats.frame();
                }
            }
        }
        if let Some(image) = &self.image {
            window
                .paint_image(
                    place.bounds,
                    place.bounds,
                    Corners::default(),
                    image.clone(),
                    0,
                    false,
                )
                .ok();
        }
    }

    fn hide(&mut self) {}

    fn on_paint(
        &mut self,
        kind: PaintElementType,
        _dirty: &[Rect],
        pixels: &[u8],
        width: i32,
        height: i32,
    ) -> bool {
        if kind == PaintElementType::POPUP {
            self.popup.set(pixels, width, height);
        } else {
            self.view.set(pixels, width, height);
        }
        self.changed = true;
        true
    }

    fn on_accelerated_paint(
        &mut self,
        kind: PaintElementType,
        info: &AcceleratedPaintInfo,
    ) -> bool {
        #[cfg(target_os = "linux")]
        {
            let frame = if kind == PaintElementType::POPUP {
                &mut self.popup
            } else {
                &mut self.view
            };
            if read_dmabuf(info, frame) {
                self.changed = true;
                return true;
            }
        }
        let _ = (kind, info);
        false
    }

    fn on_popup_show(&mut self, show: bool) {
        self.popup_shown = show;
        if !show {
            self.popup = Frame::default();
        }
        self.changed = true;
    }

    fn on_popup_size(&mut self, rect: Rect) {
        self.popup_rect = Some(rect);
    }
}

/// Copies a LINEAR single-plane BGRA dma-buf into `frame`.
#[cfg(target_os = "linux")]
fn read_dmabuf(info: &AcceleratedPaintInfo, frame: &mut Frame) -> bool {
    use cef::sys::cef_color_type_t;
    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
    const SYNC_READ: u64 = 1;
    const SYNC_END: u64 = 4;
    if info.plane_count < 1 || info.modifier != DRM_FORMAT_MOD_LINEAR {
        return false;
    }
    let format: cef_color_type_t = info.format.into();
    if format != cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 {
        return false;
    }
    let plane = &info.planes[0];
    let (width, height) = (info.extra.coded_size.width, info.extra.coded_size.height);
    let stride = plane.stride as usize;
    if width <= 0 || height <= 0 || stride < width as usize * 4 {
        return false;
    }
    let len = plane.offset as usize + stride * height as usize;
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            plane.fd,
            0,
        );
        if map == libc::MAP_FAILED {
            return false;
        }
        let mut sync = SYNC_READ;
        libc::ioctl(plane.fd, DMA_BUF_IOCTL_SYNC, &mut sync);
        let data = std::slice::from_raw_parts(
            (map as *const u8).add(plane.offset as usize),
            stride * height as usize,
        );
        frame.set_strided(data, stride, width, height);
        let mut sync = SYNC_READ | SYNC_END;
        libc::ioctl(plane.fd, DMA_BUF_IOCTL_SYNC, &mut sync);
        libc::munmap(map, len);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popups_are_drawn_over_the_page() {
        let mut p = CpuPresenter::default();
        p.on_paint(PaintElementType::VIEW, &[], &[0; 4 * 4 * 4], 4, 4);
        p.on_popup_show(true);
        p.on_popup_size(Rect {
            x: 2,
            y: 1,
            width: 4,
            height: 1,
        });
        p.on_paint(PaintElementType::POPUP, &[], &[9; 4 * 4], 4, 1);
        let pixels = p.composed().unwrap();
        let px = |x: usize, y: usize| pixels[(y * 4 + x) * 4];
        assert_eq!(px(1, 1), 0);
        assert_eq!(px(2, 1), 9);
        assert_eq!(px(3, 1), 9);
        assert_eq!(px(2, 2), 0);
    }
}
