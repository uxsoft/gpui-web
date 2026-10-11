//! The page's element, and [`HoleBackground`], the window background that
//! leaves holes where pages composited under the window show through.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::rc::Weak;

use cef::{ImplBrowserHost as _, MouseButtonType, MouseEvent};
use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, ElementInputHandler, Entity, FocusHandle,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels,
    ScrollDelta, ScrollWheelEvent, Style, Window, WindowId, fill, point, px, relative, size,
};

use crate::browser::{self, Geometry, Page, PageRef};
use crate::platform::{self, Placement};
use crate::runtime;
use crate::webview::WebView;

/// Holes and drawn pages of each window, this frame and the last.
#[derive(Default)]
struct WindowFrame {
    holes: Vec<Bounds<Pixels>>,
    pages: Vec<Weak<RefCell<Page>>>,
    previous: Vec<Weak<RefCell<Page>>>,
}

thread_local! {
    static FRAMES: RefCell<HashMap<WindowId, WindowFrame>> = RefCell::new(HashMap::new());
}

fn begin_frame(window: &Window) {
    FRAMES.with_borrow_mut(|frames| {
        let frame = frames
            .entry(window.window_handle().window_id())
            .or_default();
        frame.previous = std::mem::take(&mut frame.pages);
        frame.holes.clear();
    });
}

fn register(window: &Window, page: &PageRef, hole: Option<Bounds<Pixels>>) {
    FRAMES.with_borrow_mut(|frames| {
        let frame = frames
            .entry(window.window_handle().window_id())
            .or_default();
        frame.pages.push(page.downgrade());
        frame.holes.extend(hole);
    });
}

/// This frame's holes; pages drawn last frame but not this one go off
/// screen.
fn finish_frame(window: &Window) -> Vec<Bounds<Pixels>> {
    let (holes, gone) = FRAMES.with_borrow_mut(|frames| {
        let Some(frame) = frames.get_mut(&window.window_handle().window_id()) else {
            return (Vec::new(), Vec::new());
        };
        let gone: Vec<_> = frame
            .previous
            .drain(..)
            .filter(|old| !frame.pages.iter().any(|new| new.ptr_eq(old)))
            .filter_map(|old| old.upgrade())
            .collect();
        (frame.holes.clone(), gone)
    });
    for page in gone {
        let host = match page.try_borrow_mut() {
            Ok(mut page) => {
                if let Some(presenter) = &mut page.presenter {
                    presenter.hide();
                }
                page.host()
            }
            Err(_) => None,
        };
        // Pages off screen keep running (agents use them), more slowly.
        if let Some(host) = host {
            host.set_windowless_frame_rate(HIDDEN_FRAME_RATE);
        }
    }
    holes
}

const HIDDEN_FRAME_RATE: i32 = 10;

/// Paints the window background, except where pages show through from
/// under the window. Make it the first child of the window's root, in place
/// of the root's background, so it lays out before any page:
///
/// ```ignore
/// div().size_full().child(gpui_web::HoleBackground::new(bg)).child(content)
/// ```
///
/// Pages must not be inside a `.cached()` view: a page that isn't drawn in
/// a frame is taken off screen.
pub struct HoleBackground {
    color: Hsla,
}

impl HoleBackground {
    pub fn new(color: impl Into<Hsla>) -> Self {
        Self {
            color: color.into(),
        }
    }
}

impl IntoElement for HoleBackground {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for HoleBackground {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style {
            position: gpui::Position::Absolute,
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        style.inset.top = px(0.).into();
        style.inset.left = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        begin_frame(window);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let holes = finish_frame(window);
        for rect in subtract(bounds, &holes) {
            window.paint_quad(fill(rect, self.color));
        }
        if std::env::var_os("GPUI_WEB_DEBUG_HOLES").is_some() {
            for hole in &holes {
                window.paint_quad(fill(*hole, gpui::hsla(0., 1., 0.5, 0.3)));
            }
        }
    }
}

/// `outer` minus `holes`, as non-overlapping rectangles.
pub(crate) fn subtract(outer: Bounds<Pixels>, holes: &[Bounds<Pixels>]) -> Vec<Bounds<Pixels>> {
    let mut rects = vec![outer];
    for hole in holes {
        let mut next = Vec::with_capacity(rects.len() + 4);
        for rect in rects {
            let cut = rect.intersect(hole);
            if cut.size.width <= px(0.) || cut.size.height <= px(0.) {
                next.push(rect);
                continue;
            }
            let corners = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| {
                Bounds::from_corners(point(x0, y0), point(x1, y1))
            };
            let pieces = [
                corners(rect.left(), rect.top(), rect.right(), cut.top()),
                corners(rect.left(), cut.bottom(), rect.right(), rect.bottom()),
                corners(rect.left(), cut.top(), cut.left(), cut.bottom()),
                corners(cut.right(), cut.top(), rect.right(), cut.bottom()),
            ];
            next.extend(
                pieces
                    .into_iter()
                    .filter(|r| r.size.width > px(0.) && r.size.height > px(0.)),
            );
        }
        rects = next;
    }
    rects
}

/// `bounds` shrunk so its edges fall on whole device pixels at `scale`
/// (e.g. even logical pixels at 1.5): a page composited off them would be
/// resampled.
pub(crate) fn snap(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let n = (scale * 120.).round() as i64;
    let mut step = if n > 0 { 120 / gcd(n, 120) } else { 1 };
    if step > 8 {
        step = 1;
    }
    let step = step as f32;
    let down = |v: Pixels| px((f32::from(v) / step).floor() * step);
    let up = |v: Pixels| px((f32::from(v) / step).ceil() * step);
    let (x0, y0) = (up(bounds.left()), up(bounds.top()));
    let (x1, y1) = (down(bounds.right()).max(x0), down(bounds.bottom()).max(y0));
    Bounds::from_corners(point(x0, y0), point(x1, y1))
}

pub(crate) struct WebViewElement {
    view: Entity<WebView>,
    page: PageRef,
    focus: FocusHandle,
}

impl WebViewElement {
    pub fn new(view: Entity<WebView>, page: PageRef, focus: FocusHandle) -> Self {
        Self { view, page, focus }
    }
}

impl IntoElement for WebViewElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

pub(crate) struct Prepainted {
    hitbox: Hitbox,
    placement: Placement,
}

impl Element for WebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(relative(1.).into(), relative(1.).into()),
            flex_grow: 1.,
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Prepainted {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let mut page = self.page.borrow_mut();
        if page.presenter.is_none() && !page.closed {
            page.presenter = Some(platform::create(window, runtime::mode()));
        }
        let (punches_hole, has_content, scale) =
            page.presenter.as_ref().map_or((false, false, None), |p| {
                (p.punches_hole(), p.has_content(), p.scale())
            });
        let scale = scale.unwrap_or_else(|| window.scale_factor());
        let snapped = if punches_hole {
            snap(bounds, scale)
        } else {
            bounds
        };
        let visible = snapped.intersect(&window.content_mask().bounds);
        drop(page);
        let hole = (punches_hole && has_content).then_some(visible);
        register(window, &self.page, hole);
        Prepainted {
            hitbox,
            placement: Placement {
                bounds: snapped,
                visible,
            },
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepainted: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let placement = prepainted.placement;
        // Size and scale first, so CEF renders for where the page is.
        let (host, resized, rescaled, zoom, rate, bind, cursor) = {
            let mut page = self.page.borrow_mut();
            let scale = page
                .presenter
                .as_ref()
                .and_then(|p| p.scale())
                .unwrap_or_else(|| window.scale_factor());
            let geometry = Geometry {
                width: f32::from(placement.bounds.size.width).max(1.),
                height: f32::from(placement.bounds.size.height).max(1.),
                scale,
                ..page.geometry
            };
            let old = std::mem::replace(&mut page.geometry, geometry);
            let resized = old.view_size() != geometry.view_size();
            let rescaled = old.scale != geometry.scale;
            let rate = page
                .presenter
                .as_ref()
                .and_then(|p| p.refresh_rate())
                .unwrap_or(60)
                .clamp(30, 240);
            let rate_changed = std::mem::replace(&mut page.frame_rate, rate) != rate;
            let browser = page.browser.clone();
            if let Some(presenter) = &mut page.presenter {
                presenter.place(&placement, window, cx);
            }
            (
                page.host(),
                resized,
                rescaled,
                geometry.zoom_level(),
                rate_changed.then_some(rate),
                browser,
                page.cursor,
            )
        };
        if let Some(host) = &host {
            if resized {
                host.was_resized();
            }
            if rescaled {
                host.notify_screen_info_changed();
                browser::apply_zoom(host, zoom);
            }
            // Back from off screen, or a new display.
            let rate = rate.unwrap_or(self.page.borrow().frame_rate);
            if host.windowless_frame_rate() != rate {
                host.set_windowless_frame_rate(rate);
            }
        }
        if let Some(browser) = bind {
            let mut page = self.page.borrow_mut();
            if let Some(presenter) = &mut page.presenter {
                presenter.set_browser(&browser);
            }
        }

        let focused = self.focus.is_focused(window);
        let window_handle = window.window_handle();
        let focus_changed = self.view.update(cx, |view, _| {
            view.window = Some(window_handle);
            std::mem::replace(&mut view.focused, focused) != focused
        });
        if focus_changed {
            if let Some(host) = &host {
                host.set_focus(focused as i32);
            }
            if let Some(presenter) = &mut self.page.borrow_mut().presenter {
                presenter.focus_changed(focused);
            }
        }
        #[cfg(target_os = "windows")]
        if self.page.borrow().app_keystrokes.is_empty() {
            let keystrokes = app_keystrokes(cx);
            self.page.borrow_mut().app_keystrokes = keystrokes;
        }
        if focused {
            window.handle_input(
                &self.focus,
                ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
        window.set_cursor_style(cursor, &prepainted.hitbox);
        self.listen(placement.bounds, &prepainted.hitbox, window);
    }
}

impl WebViewElement {
    fn listen(&self, bounds: Bounds<Pixels>, hitbox: &Hitbox, window: &mut Window) {
        let origin = bounds.origin;
        let event_at = {
            let page = self.page.clone();
            move |position: gpui::Point<Pixels>, modifiers: u32| -> MouseEvent {
                let geometry = page.borrow().geometry;
                let (x, y) = geometry.view_point(
                    f32::from(position.x - origin.x),
                    f32::from(position.y - origin.y),
                );
                MouseEvent { x, y, modifiers }
            }
        };

        window.on_mouse_event({
            let view = self.view.clone();
            let focus = self.focus.clone();
            let hitbox = hitbox.clone();
            let event_at = event_at.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                window.focus(&focus, cx);
                view.update(cx, |view, _| {
                    let host = view.page.borrow().host();
                    let Some(host) = host else { return };
                    match event.button {
                        MouseButton::Navigate(NavigationDirection::Back) => view.go_back(),
                        MouseButton::Navigate(NavigationDirection::Forward) => view.go_forward(),
                        button => {
                            if !view.pressed.contains(&button) {
                                view.pressed.push(button);
                            }
                            let flags = crate::input::event_flags(&event.modifiers, &view.pressed);
                            host.send_mouse_click_event(
                                Some(&event_at(event.position, flags)),
                                mouse_button(button),
                                0,
                                event.click_count.max(1) as i32,
                            );
                        }
                    }
                });
                cx.stop_propagation();
            }
        });

        window.on_mouse_event({
            let view = self.view.clone();
            let event_at = event_at.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                view.update(cx, |view, _| {
                    let Some(ix) = view.pressed.iter().position(|b| *b == event.button) else {
                        return;
                    };
                    view.pressed.remove(ix);
                    let host = view.page.borrow().host();
                    let Some(host) = host else { return };
                    let flags = crate::input::event_flags(&event.modifiers, &view.pressed);
                    host.send_mouse_click_event(
                        Some(&event_at(event.position, flags)),
                        mouse_button(event.button),
                        1,
                        event.click_count.max(1) as i32,
                    );
                });
            }
        });

        window.on_mouse_event({
            let view = self.view.clone();
            let hitbox = hitbox.clone();
            let event_at = event_at.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let hovered = hitbox.is_hovered(window);
                view.update(cx, |view, _| {
                    let was = std::mem::replace(&mut view.hovered, hovered);
                    if !hovered && !was && view.pressed.is_empty() {
                        return;
                    }
                    let host = view.page.borrow().host();
                    let Some(host) = host else { return };
                    let flags = crate::input::event_flags(&event.modifiers, &view.pressed);
                    let leave = !hovered && view.pressed.is_empty();
                    host.send_mouse_move_event(
                        Some(&event_at(event.position, flags)),
                        leave as i32,
                    );
                });
            }
        });

        window.on_mouse_event({
            let view = self.view.clone();
            let hitbox = hitbox.clone();
            let page = self.page.clone();
            move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let geometry = page.borrow().geometry;
                let k = if geometry.device_rect {
                    geometry.scale
                } else {
                    1.
                };
                let (dx, dy) = match event.delta {
                    ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
                    ScrollDelta::Lines(delta) => (delta.x * 40., delta.y * 40.),
                };
                view.update(cx, |view, _| {
                    let host = view.page.borrow().host();
                    let Some(host) = host else { return };
                    let flags = crate::input::event_flags(&event.modifiers, &view.pressed);
                    host.send_mouse_wheel_event(
                        Some(&event_at(event.position, flags)),
                        (dx * k).round() as i32,
                        (dy * k).round() as i32,
                    );
                });
                cx.stop_propagation();
            }
        });
    }
}

/// The first keystrokes of the app's shortcuts, which a windowed page must
/// hand back to GPUI.
#[cfg(target_os = "windows")]
fn app_keystrokes(cx: &App) -> Vec<gpui::Keystroke> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let mut keystrokes: Vec<gpui::Keystroke> = Vec::new();
    for binding in keymap.bindings() {
        let Some(first) = binding.keystrokes().first() else {
            continue;
        };
        let keystroke = first.inner().clone();
        let m = &keystroke.modifiers;
        let shortcut = m.control || m.alt || m.platform || keystroke.key.starts_with('f');
        if shortcut
            && !keystrokes
                .iter()
                .any(|k| crate::input::same_keystroke(k, &keystroke))
        {
            keystrokes.push(keystroke);
        }
    }
    keystrokes
}

fn mouse_button(button: MouseButton) -> MouseButtonType {
    use cef::sys::cef_mouse_button_type_t as B;
    MouseButtonType::from(match button {
        MouseButton::Middle => B::MBT_MIDDLE,
        MouseButton::Right => B::MBT_RIGHT,
        _ => B::MBT_LEFT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn area(rects: &[Bounds<Pixels>]) -> f32 {
        rects
            .iter()
            .map(|r| f32::from(r.size.width) * f32::from(r.size.height))
            .sum()
    }

    #[test]
    fn holes_are_left_unpainted() {
        let outer = rect(0., 0., 100., 100.);
        let rects = subtract(outer, &[rect(10., 20., 30., 40.)]);
        assert_eq!(area(&rects), 100. * 100. - 30. * 40.);
        for r in &rects {
            assert!(
                r.intersect(&rect(10., 20., 30., 40.)).size.width <= px(0.)
                    || r.intersect(&rect(10., 20., 30., 40.)).size.height <= px(0.)
            );
        }
        // A hole sticking out of the window, and two holes.
        let rects = subtract(outer, &[rect(90., 90., 50., 50.), rect(0., 0., 10., 10.)]);
        assert_eq!(area(&rects), 100. * 100. - 100. - 100.);
    }

    #[test]
    fn pages_snap_to_device_pixels() {
        // 1.5: even logical pixels.
        let s = snap(rect(17., 58.5, 801., 400.), 1.5);
        assert_eq!(
            s,
            Bounds::from_corners(point(px(18.), point_y(60.)), point(px(818.), point_y(458.)))
        );
        // 1.25: multiples of 4.
        let s = snap(rect(1., 1., 100., 100.), 1.25);
        assert_eq!(s.origin, point(px(4.), px(4.)));
        assert_eq!(s.size, size(px(96.), px(96.)));
        // Integer scales: whole pixels.
        let s = snap(rect(1.5, 2., 10., 10.), 2.);
        assert_eq!(s.origin.x, px(2.));
    }

    fn point_y(v: f32) -> Pixels {
        px(v)
    }
}
