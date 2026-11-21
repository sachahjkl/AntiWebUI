//! Wayland protocol state and event dispatch.

use crate::keyboard::{Keyboard, Repeat};
use crate::popup::{Popup, SurfaceFrame};
use anti_webui_core::event::{
    AXIS_MOTION_GAIN, ClipboardKind, ImeUpdate, PinchPhase, PointerButton, ScrollPhase,
    ScrollSource, WindowEvent,
};
use anti_webui_core::geometry::{PointF, PointI, Scale, SizeI};
use anti_webui_core::platform::{CursorIcon, ImePurpose, ImeState, PopupId, PopupParams};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::time::Instant;
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
    wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_subcompositor,
    wl_surface,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, Shape},
    wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_hold_v1, zwp_pointer_gesture_pinch_v1, zwp_pointer_gestures_v1,
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3,
    zwp_text_input_v3::{self, ContentHint, ContentPurpose},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1;
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

#[derive(Default)]
pub struct PendingIme {
    pub preedit: Option<(String, Option<(usize, usize)>)>,
    pub commit: Option<String>,
    pub delete: Option<(usize, usize)>,
}

#[derive(Default)]
pub struct PointerFrame {
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub wheel_x: i32,
    pub wheel_y: i32,
    pub has_scroll: bool,
    pub source: Option<wl_pointer::AxisSource>,
    pub stopped: bool,
    pub time: u32,
}

pub use anti_webui_core::platform::SurfaceId;

/// `zwp_pointer_gestures_v1.get_hold_gesture` came in this version.
const HOLD_VERSION: u32 = 3;
/// How long a popup may hold up the next frame by not getting its frame callback.
const POPUP_FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// What the window's surface is to the compositor.
pub(crate) enum SurfaceRole {
    Toplevel {
        xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
    Layer(crate::layer::LayerRole),
    Lock(crate::lock::LockRole),
}

pub struct State {
    pub qh: QueueHandle<State>,
    pub compositor: wl_compositor::WlCompositor,
    pub wm_base: xdg_wm_base::XdgWmBase,
    pub surface: wl_surface::WlSurface,
    pub(crate) role: SurfaceRole,
    /// The outputs a layer surface can be put on.
    pub(crate) outputs: Option<crate::layer::Outputs>,
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    pub subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    pub shm: Option<wl_shm::WlShm>,
    pub seat: Option<wl_seat::WlSeat>,
    pub viewport: Option<wp_viewport::WpViewport>,
    pub fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    pub(crate) decor: crate::decor::Decor,
    pub cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    pub primary_manager:
        Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,

    pub configured: bool,
    pub logical_size: (i32, i32),
    pending_size: Option<(i32, i32)>,
    /// What the window is, and what it becomes with the next `xdg_surface.configure`.
    fullscreen: bool,
    pending_fullscreen: Option<bool>,
    /// The size to go back to when the compositor leaves it up to us.
    windowed_size: (i32, i32),
    pub scale: Scale,
    pub buffer_size: SizeI,
    pub size_announced: Option<(SizeI, Scale)>,
    pub closed: bool,

    pub pointer: Option<wl_pointer::WlPointer>,
    pub cursor_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    pub pointer_serial: u32,
    /// The pointer is on a surface of this window. Every window of a session gets every pointer
    /// event; the others drop them (see `docs/design/36-multi-window.md`).
    pub pointer_here: bool,
    /// The pointer is on an edge to resize the window from, and what it would resize.
    pub(crate) pointer_border: Option<(crate::decor::Side, anti_webui_core::window::ResizeEdge)>,
    /// What the pointer is on, which `pointer_pos` is relative to.
    pub pointer_surface: SurfaceId,
    pub pointer_pos: (f64, f64),
    pub cursor: CursorIcon,
    pub frame: PointerFrame,
    /// Fingers are moving on a touch surface: `axis_stop` will end the sequence.
    pub finger_scrolling: bool,
    pub pointer_gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    pub hold_gesture: Option<zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1>,
    pub pinch_gesture: Option<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1>,
    /// How big the pinch going on says it is, which it counts from where it began.
    pub pinch_scale: f64,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    pub kb: Keyboard,
    /// What the keyboard is on, which the IME's cursor rectangle is relative to.
    pub keyboard_surface: SurfaceId,
    pub keyboard_focused: bool,
    /// The keyboard is on a surface of this window, like `pointer_here`.
    pub keyboard_here: bool,
    /// Something dragged is over a surface of this window, like `pointer_here`.
    pub drag_here: bool,
    /// What the core was told; moving between our surfaces isn't a change.
    pub focus_announced: bool,
    pub input_serial: u32,
    /// The last press of a button or key, which a popup's grab refers to.
    pub press_serial: u32,

    pub text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    pub ti_entered: bool,
    pub ti_enabled: bool,
    pub ime_wanted: Option<ImeState>,
    pub pending_ime: PendingIme,

    pub data_device: Option<wl_data_device::WlDataDevice>,
    pub offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    pub selection_offer: Option<wl_data_offer::WlDataOffer>,
    pub data_source: Option<(wl_data_source::WlDataSource, String)>,
    pub primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,
    pub primary_offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    pub primary_offer: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    pub primary_source: Option<(
        zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        String,
    )>,
    pub paste_tx: mpsc::Sender<(ClipboardKind, String)>,
    pub wake: OwnedFd,
    pub(crate) drag: crate::drag::DragState,

    pub(crate) window_frame: SurfaceFrame,
    pub(crate) popups: HashMap<PopupId, Popup>,
    /// The xdg popups of every window on the connection, for the one grab they share.
    pub(crate) connection_popups: crate::grab::ConnectionPopups,
    /// Where other windows ask this one to dismiss popups whose grab is in their way.
    pub(crate) mailbox: std::sync::Arc<crate::grab::Mailbox>,
    /// Popups whose parent has no buffer yet, or whose grab waits for another to go.
    pub(crate) waiting_popups: Vec<PopupParams>,
    pub redraw_requested: bool,
    pub redraw_at: Option<Instant>,
    pub events: Vec<WindowEvent>,
    pub(crate) present: crate::present::PresentState,
}

impl Drop for State {
    fn drop(&mut self) {
        // Not to leave a grab in the way of the other windows, if it goes without closing them.
        self.connection_popups.lock().destroy_all(&self.mailbox);
    }
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        qh: QueueHandle<State>,
        compositor: wl_compositor::WlCompositor,
        wm_base: xdg_wm_base::XdgWmBase,
        surface: wl_surface::WlSurface,
        role: SurfaceRole,
        logical_size: (i32, i32),
        paste_tx: mpsc::Sender<(ClipboardKind, String)>,
        wake: OwnedFd,
        connection_popups: crate::grab::ConnectionPopups,
        mailbox: std::sync::Arc<crate::grab::Mailbox>,
    ) -> Self {
        Self {
            qh,
            compositor,
            wm_base,
            surface,
            role,
            outputs: None,
            viewporter: None,
            subcompositor: None,
            shm: None,
            seat: None,
            viewport: None,
            fractional: None,
            decor: Default::default(),
            cursor_shape_manager: None,
            text_input_manager: None,
            data_device_manager: None,
            primary_manager: None,
            configured: false,
            logical_size,
            fullscreen: false,
            pending_fullscreen: None,
            windowed_size: logical_size,
            pending_size: None,
            scale: Scale::ONE,
            buffer_size: SizeI::new(logical_size.0, logical_size.1),
            size_announced: None,
            closed: false,
            pointer: None,
            cursor_device: None,
            pointer_serial: 0,
            pointer_here: false,
            pointer_border: None,
            pointer_surface: SurfaceId::Window,
            pointer_pos: (0.0, 0.0),
            cursor: CursorIcon::Default,
            frame: PointerFrame::default(),
            finger_scrolling: false,
            pointer_gestures: None,
            pinch_gesture: None,
            pinch_scale: 1.0,
            hold_gesture: None,
            keyboard: None,
            kb: Keyboard::new(),
            keyboard_surface: SurfaceId::Window,
            keyboard_focused: false,
            keyboard_here: false,
            drag_here: false,
            focus_announced: false,
            input_serial: 0,
            press_serial: 0,
            text_input: None,
            ti_entered: false,
            ti_enabled: false,
            ime_wanted: None,
            pending_ime: PendingIme::default(),
            data_device: None,
            offers: HashMap::new(),
            selection_offer: None,
            data_source: None,
            primary_device: None,
            primary_offers: HashMap::new(),
            primary_offer: None,
            primary_source: None,
            paste_tx,
            wake,
            window_frame: SurfaceFrame::default(),
            popups: HashMap::new(),
            connection_popups,
            mailbox,
            waiting_popups: Vec::new(),
            redraw_requested: true,
            redraw_at: None,
            events: Vec::new(),
            present: Default::default(),
            drag: Default::default(),
        }
    }

    /// Whether a surface is one of this window's. Each window of a session gives its surfaces
    /// the same kind of user data, so the objects themselves are compared.
    pub(crate) fn owns(&self, surface: &wl_surface::WlSurface) -> bool {
        *surface == self.surface
            || self.popups.values().any(|p| p.surface == *surface)
            || self.border_side(surface).is_some()
    }

    pub(crate) fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// The toplevel, unless the window is something else, like a layer surface.
    pub(crate) fn toplevel(&self) -> Option<&xdg_toplevel::XdgToplevel> {
        match &self.role {
            SurfaceRole::Toplevel { toplevel, .. } => Some(toplevel),
            SurfaceRole::Layer(_) | SurfaceRole::Lock(_) => None,
        }
    }

    pub(crate) fn xdg_surface(&self) -> Option<&xdg_surface::XdgSurface> {
        match &self.role {
            SurfaceRole::Toplevel { xdg_surface, .. } => Some(xdg_surface),
            SurfaceRole::Layer(_) | SurfaceRole::Lock(_) => None,
        }
    }

    /// A position on the pointer's surface in window coordinates.
    fn physical(&self, x: f64, y: f64) -> PointF {
        self.physical_on(self.pointer_surface, x, y)
    }

    /// A position on a surface in window coordinates.
    pub(crate) fn physical_on(&self, surface: SurfaceId, x: f64, y: f64) -> PointF {
        let s = self.scale.as_f64();
        let origin = self.surface_origin(surface);
        PointF::new(
            (x * s) as f32 + origin.x as f32,
            (y * s) as f32 + origin.y as f32,
        )
    }

    /// Where a surface's origin is in window coordinates.
    pub(crate) fn surface_origin(&self, surface: SurfaceId) -> PointI {
        match surface {
            SurfaceId::Popup(id) => self.popups.get(&id).map_or(PointI::default(), |p| p.origin),
            SurfaceId::Window | SurfaceId::DragIcon => PointI::default(),
        }
    }

    pub(crate) fn frame_state(&mut self, surface: SurfaceId) -> Option<&mut SurfaceFrame> {
        match surface {
            SurfaceId::Window => Some(&mut self.window_frame),
            SurfaceId::Popup(id) => self.popups.get_mut(&id).map(|p| &mut p.frame),
            SurfaceId::DragIcon => self.drag.icon.as_mut().map(|i| &mut i.frame),
        }
    }

    pub(crate) fn wl_surface(&self, surface: SurfaceId) -> Option<&wl_surface::WlSurface> {
        match surface {
            SurfaceId::Window => Some(&self.surface),
            SurfaceId::Popup(id) => self.popups.get(&id).map(|p| &p.surface),
            SurfaceId::DragIcon => self.drag.icon.as_ref().map(|i| &i.surface),
        }
    }

    /// Asks for a frame callback with the next commit of the surface.
    pub(crate) fn prepare_commit(&mut self, surface: SurfaceId) {
        let qh = self.qh.clone();
        let Some(wl) = self.wl_surface(surface).cloned() else {
            return;
        };
        if let Some(f) = self.frame_state(surface)
            && !f.callback_queued
        {
            f.callback_queued = true;
            wl.frame(&qh, surface);
        }
        self.apply_opaque_region(surface);
    }

    /// A buffer was committed to the surface.
    pub(crate) fn committed(&mut self, surface: SurfaceId) {
        let now = Instant::now();
        if let Some(f) = self.frame_state(surface) {
            f.callback_queued = false;
            f.pending = Some(now);
            if !std::mem::replace(&mut f.mapped, true) {
                self.create_waiting_popups();
            }
        }
    }

    /// Whether frame callbacks allow a frame now. If not, the time when a popup stops holding it
    /// up, if that's all it is waiting for.
    pub(crate) fn frame_wait(&self, now: Instant) -> Result<(), Option<Instant>> {
        if self.window_frame.pending.is_some() {
            return Err(None);
        }
        let latest = self
            .popups
            .values()
            .filter_map(|p| p.frame.pending)
            .map(|t| t + POPUP_FRAME_WAIT)
            .max();
        match latest {
            Some(t) if t > now => Err(Some(t)),
            _ => Ok(()),
        }
    }

    /// Recomputes the buffer size (R3) and announces size/scale changes.
    pub fn update_size(&mut self) {
        let (w, h) = self.logical_size;
        let n = self.scale.0 as f64 / 120.0;
        let size = SizeI::new((w as f64 * n).round() as i32, (h as f64 * n).round() as i32);
        self.buffer_size = size;
        if let Some(vp) = &self.viewport {
            vp.set_destination(w.max(1), h.max(1));
        } else {
            self.surface
                .set_buffer_scale((self.scale.0 / 120).max(1) as i32);
        }
        // Edges to resize from are outside of it.
        if let Some(xdg_surface) = self.xdg_surface() {
            xdg_surface.set_window_geometry(0, 0, w.max(1), h.max(1));
            self.update_borders();
        }
        if self.size_announced != Some((size, self.scale)) {
            self.size_announced = Some((size, self.scale));
            self.events.push(WindowEvent::Resized {
                size,
                scale: self.scale,
            });
            self.redraw_requested = true;
        }
    }

    pub fn set_cursor(&mut self, icon: CursorIcon) {
        self.cursor = icon;
        self.apply_cursor();
    }

    fn apply_cursor(&self) {
        if self.cursor == CursorIcon::None {
            if let Some(pointer) = &self.pointer {
                pointer.set_cursor(self.pointer_serial, None, 0, 0);
            }
            return;
        }
        let Some(dev) = &self.cursor_device else {
            return;
        };
        let shape = match self.cursor {
            CursorIcon::Default => Shape::Default,
            CursorIcon::Pointer => Shape::Pointer,
            CursorIcon::Text => Shape::Text,
            CursorIcon::NotAllowed => Shape::NotAllowed,
            CursorIcon::Grab => Shape::Grab,
            CursorIcon::Grabbing => Shape::Grabbing,
            CursorIcon::Move => Shape::Move,
            CursorIcon::EwResize => Shape::EwResize,
            CursorIcon::NsResize => Shape::NsResize,
            CursorIcon::Crosshair => Shape::Crosshair,
            CursorIcon::Copy => Shape::Copy,
            CursorIcon::None => return,
        };
        dev.set_shape(self.pointer_serial, shape);
    }

    /// Sends the wanted IME state to the compositor if the text-input has focus.
    pub fn apply_ime(&mut self) {
        let Some(ti) = &self.text_input else { return };
        if !self.ti_entered {
            return;
        }
        match &self.ime_wanted {
            Some(state) => {
                if !self.ti_enabled {
                    ti.enable();
                    self.ti_enabled = true;
                }
                ti.set_surrounding_text(
                    state.surrounding.clone(),
                    state.cursor as i32,
                    state.anchor as i32,
                );
                ti.set_text_change_cause(zwp_text_input_v3::ChangeCause::Other);
                let purpose = match state.purpose {
                    ImePurpose::Normal => ContentPurpose::Normal,
                    ImePurpose::Password => ContentPurpose::Password,
                    ImePurpose::Number => ContentPurpose::Number,
                    ImePurpose::Email => ContentPurpose::Email,
                    ImePurpose::Url => ContentPurpose::Url,
                };
                let mut hint = ContentHint::empty();
                if state.multiline {
                    hint |= ContentHint::Multiline;
                }
                if state.purpose == ImePurpose::Password {
                    hint |= ContentHint::SensitiveData | ContentHint::HiddenText;
                }
                ti.set_content_type(hint, purpose);
                let origin = self.surface_origin(self.keyboard_surface);
                let r = state.cursor_rect.translate(-origin.x, -origin.y);
                let s = self.scale;
                let x = s.to_logical(r.x as f32).floor() as i32;
                let y = s.to_logical(r.y as f32).floor() as i32;
                let w = (s.to_logical(r.right() as f32).ceil() as i32 - x).max(1);
                let h = (s.to_logical(r.bottom() as f32).ceil() as i32 - y).max(1);
                ti.set_cursor_rectangle(x, y, w, h);
                ti.commit();
            }
            None => {
                if self.ti_enabled {
                    ti.disable();
                    ti.commit();
                    self.ti_enabled = false;
                }
            }
        }
    }

    pub fn set_clipboard(&mut self, kind: ClipboardKind, text: String) {
        match kind {
            ClipboardKind::Clipboard => {
                let (Some(mgr), Some(dev)) = (&self.data_device_manager, &self.data_device) else {
                    return;
                };
                let source = mgr.create_data_source(&self.qh, ());
                for m in TEXT_MIMES {
                    source.offer(m.to_owned());
                }
                dev.set_selection(Some(&source), self.input_serial);
                if let Some((old, _)) = self.data_source.replace((source, text)) {
                    old.destroy();
                }
            }
            ClipboardKind::Primary => {
                let (Some(mgr), Some(dev)) = (&self.primary_manager, &self.primary_device) else {
                    return;
                };
                let source = mgr.create_source(&self.qh, ());
                for m in TEXT_MIMES {
                    source.offer(m.to_owned());
                }
                dev.set_selection(Some(&source), self.input_serial);
                if let Some((old, _)) = self.primary_source.replace((source, text)) {
                    old.destroy();
                }
            }
        }
    }

    /// Starts reading the clipboard; the result arrives through the paste channel.
    pub fn request_paste(&mut self, conn: &Connection, kind: ClipboardKind) {
        let own = match kind {
            ClipboardKind::Clipboard => self.data_source.as_ref().map(|(_, t)| t.clone()),
            ClipboardKind::Primary => self.primary_source.as_ref().map(|(_, t)| t.clone()),
        };
        if let Some(text) = own {
            let _ = self.paste_tx.send((kind, text));
            let _ = rustix::io::write(&self.wake, &[1]);
            return;
        }
        let Ok((read, write)) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) else {
            return;
        };
        let received = match kind {
            ClipboardKind::Clipboard => self.selection_offer.as_ref().and_then(|offer| {
                let mimes = self.offers.get(&offer.id())?;
                let mime = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m))?;
                offer.receive(mime.to_string(), write.as_fd());
                Some(())
            }),
            ClipboardKind::Primary => self.primary_offer.as_ref().and_then(|offer| {
                let mimes = self.primary_offers.get(&offer.id())?;
                let mime = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m))?;
                offer.receive(mime.to_string(), write.as_fd());
                Some(())
            }),
        };
        if received.is_none() {
            return;
        }
        let _ = conn.flush();
        drop(write);
        let tx = self.paste_tx.clone();
        let wake = self.wake.try_clone().ok();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut file = std::fs::File::from(read);
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() {
                let text = String::from_utf8_lossy(&buf).replace("\r\n", "\n");
                let _ = tx.send((kind, text));
                if let Some(w) = wake {
                    let _ = rustix::io::write(&w, &[1]);
                }
            }
        });
    }

    pub fn key_repeat_due(&mut self, now: Instant) -> Option<u32> {
        let r = self.kb.repeat.as_mut()?;
        if now < r.next {
            return None;
        }
        let interval = std::time::Duration::from_secs_f64(1.0 / self.kb.repeat_rate.max(1) as f64);
        r.next += interval;
        if r.next < now {
            r.next = now + interval;
        }
        Some(r.keycode)
    }
}

fn write_async(fd: OwnedFd, text: String) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let _ = file.write_all(text.as_bytes());
    });
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(State: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(State: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(State: ignore zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceId> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let SurfaceId::Popup(id) = id {
                state.popup_placed(*id);
                return;
            }
            if let Some(fullscreen) = state.pending_fullscreen.take()
                && fullscreen != state.fullscreen
            {
                state.fullscreen = fullscreen;
                state.events.push(WindowEvent::Fullscreen(fullscreen));
            }
            if let Some(size) = state.pending_size.take() {
                state.logical_size = size;
            }
            if !state.fullscreen {
                state.windowed_size = state.logical_size;
            }
            state.apply_toplevel_states();
            state.configured = true;
            state.update_size();
            state.redraw_requested = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                state.toplevel_states(&states);
                let fullscreen = states
                    .chunks_exact(4)
                    .map(|s| u32::from_ne_bytes(s.try_into().unwrap()))
                    .any(|s| s == xdg_toplevel::State::Fullscreen as u32);
                state.pending_fullscreen = Some(fullscreen);
                if width > 0 && height > 0 {
                    state.pending_size = Some((width, height));
                } else if state.fullscreen && !fullscreen {
                    state.pending_size = Some(state.windowed_size);
                }
            }
            xdg_toplevel::Event::Close => state.events.push(WindowEvent::CloseRequested),
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scale = Scale(scale.max(1));
            if state.configured {
                state.update_size();
            }
        }
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && *id == SurfaceId::Window
            && state.fractional.is_none()
        {
            state.scale = Scale(factor.max(1) as u32 * 120);
            if state.configured {
                state.update_size();
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        surface: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && let Some(f) = state.frame_state(*surface)
        {
            // Any commit takes a queued request along, not only those that present something.
            f.callback_queued = false;
            f.pending = None;
            f.last_callback = Some(Instant::now());
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.seat.get_or_insert_with(|| seat.clone());
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            if let Some(mgr) = &state.cursor_shape_manager {
                state.cursor_device = Some(mgr.get_pointer(&pointer, qh, ()));
            }
            if let Some(mgr) = &state.pointer_gestures {
                state.pinch_gesture = Some(mgr.get_pinch_gesture(&pointer, qh, ()));
                if mgr.version() >= HOLD_VERSION {
                    state.hold_gesture = Some(mgr.get_hold_gesture(&pointer, qh, ()));
                }
            }
            state.pointer = Some(pointer);
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
            if let Some(mgr) = &state.text_input_manager {
                state.text_input = Some(mgr.get_text_input(seat, qh, ()));
            }
        }
        if state.data_device.is_none()
            && let Some(mgr) = &state.data_device_manager
        {
            state.data_device = Some(mgr.get_data_device(seat, qh, ()));
        }
        if state.primary_device.is_none()
            && let Some(mgr) = &state.primary_manager
        {
            state.primary_device = Some(mgr.get_device(seat, qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let modifiers = state.kb.modifiers;
        match &event {
            wl_pointer::Event::Enter { surface, .. } => {
                state.pointer_here = state.owns(surface);
                if !state.pointer_here {
                    return;
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if !std::mem::take(&mut state.pointer_here) {
                    return;
                }
            }
            _ if !state.pointer_here => return,
            _ => {}
        }
        if state.pointer_on_border(&event) {
            return;
        }
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.pointer_serial = serial;
                state.pointer_surface = surface_id(&surface);
                state.pointer_pos = (surface_x, surface_y);
                state.apply_cursor();
                let position = state.physical(surface_x, surface_y);
                state.events.push(WindowEvent::PointerMoved {
                    position,
                    modifiers,
                });
            }
            wl_pointer::Event::Leave { .. } => state.events.push(WindowEvent::PointerLeft),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
                let position = state.physical(surface_x, surface_y);
                state.events.push(WindowEvent::PointerMoved {
                    position,
                    modifiers,
                });
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: btn_state,
            } => {
                state.input_serial = serial;
                let pressed = matches!(btn_state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if pressed {
                    state.press_serial = serial;
                    state.drag.button_serial = serial;
                }
                let button = match button {
                    0x110 => PointerButton::Primary,
                    0x111 => PointerButton::Secondary,
                    0x112 => PointerButton::Middle,
                    b => PointerButton::Other(b),
                };
                let (x, y) = state.pointer_pos;
                let position = state.physical(x, y);
                state.events.push(WindowEvent::PointerButton {
                    position,
                    button,
                    pressed,
                    modifiers,
                    time,
                });
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => state.frame.source = Some(source),
            wl_pointer::Event::Axis { time, axis, value } => {
                state.frame.has_scroll = true;
                state.frame.time = time;
                match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => state.frame.scroll_y += value,
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        state.frame.scroll_x += value
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                state.frame.has_scroll = true;
                match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => {
                        state.frame.wheel_y += value120
                    }
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        state.frame.wheel_x += value120
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::AxisStop { time, .. } => {
                state.frame.stopped = true;
                state.frame.time = time;
            }
            wl_pointer::Event::Frame => {
                let f = std::mem::take(&mut state.frame);
                let scale = state.scale.as_f64();
                let (x, y) = state.pointer_pos;
                let position = state.physical(x, y);
                for (delta_x, delta_y, source, phase) in
                    f.scrolls(scale, &mut state.finger_scrolling)
                {
                    state.events.push(WindowEvent::Scroll {
                        position,
                        delta_x: delta_x as f32,
                        delta_y: delta_y as f32,
                        modifiers,
                        source,
                        phase,
                        time: f.time,
                    });
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn surface_id(surface: &wl_surface::WlSurface) -> SurfaceId {
    surface
        .data::<SurfaceId>()
        .copied()
        .unwrap_or(SurfaceId::Window)
}

impl PointerFrame {
    /// What the axis events of this `wl_pointer.frame` mean: `(dx, dy, source, phase)`. Keeps
    /// `finger_scrolling` for the next frame.
    fn scrolls(
        &self,
        scale: f64,
        finger_scrolling: &mut bool,
    ) -> Vec<(f64, f64, ScrollSource, ScrollPhase)> {
        let mut out = Vec::new();
        let pixels = (self.scroll_x * scale, self.scroll_y * scale);
        let moved = (
            pixels.0 * AXIS_MOTION_GAIN as f64,
            pixels.1 * AXIS_MOTION_GAIN as f64,
        );
        if self.has_scroll {
            if self.source == Some(wl_pointer::AxisSource::Finger) {
                let phase = if std::mem::replace(finger_scrolling, true) {
                    ScrollPhase::Moved
                } else {
                    ScrollPhase::Began
                };
                out.push((moved.0, moved.1, ScrollSource::Touchpad, phase));
            } else if self.wheel_x != 0 || self.wheel_y != 0 {
                out.push((
                    self.wheel_x as f64 / 120.0,
                    self.wheel_y as f64 / 120.0,
                    ScrollSource::Wheel,
                    ScrollPhase::Single,
                ));
            } else if self.source == Some(wl_pointer::AxisSource::Continuous) {
                out.push((moved.0, moved.1, ScrollSource::Unknown, ScrollPhase::Single));
            } else {
                out.push((
                    pixels.0,
                    pixels.1,
                    ScrollSource::Unknown,
                    ScrollPhase::Single,
                ));
            }
        }
        // One axis can stop while the fingers keep moving along the other.
        if self.stopped && self.scroll_x == 0.0 && self.scroll_y == 0.0 && *finger_scrolling {
            *finger_scrolling = false;
            out.push((
                0.0,
                0.0,
                ScrollSource::Touchpad,
                ScrollPhase::Lifted { fling: true },
            ));
        }
        out
    }
}

impl Dispatch<zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1,
        event: zwp_pointer_gesture_hold_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !state.pointer_here || state.pointer_border.is_some() {
            return;
        }
        if let zwp_pointer_gesture_hold_v1::Event::Begin { time, .. } = event {
            let (x, y) = state.pointer_pos;
            let position = state.physical(x, y);
            let modifiers = state.kb.modifiers;
            state.events.push(WindowEvent::Scroll {
                position,
                delta_x: 0.0,
                delta_y: 0.0,
                modifiers,
                source: ScrollSource::Touchpad,
                phase: ScrollPhase::Held,
                time,
            });
        }
    }
}

impl Dispatch<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
        event: zwp_pointer_gesture_pinch_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_pinch_v1::Event;
        if !state.pointer_here || state.pointer_border.is_some() {
            return;
        }
        let (phase, scale) = match event {
            Event::Begin { .. } => {
                state.pinch_scale = 1.0;
                (PinchPhase::Began, 1.0)
            }
            Event::Update { scale, .. } if scale > 0.0 => {
                let ratio = scale / state.pinch_scale;
                state.pinch_scale = scale;
                (PinchPhase::Moved, ratio as f32)
            }
            Event::End { cancelled, .. } => (
                if cancelled != 0 {
                    PinchPhase::Cancelled
                } else {
                    PinchPhase::Ended
                },
                1.0,
            ),
            _ => return,
        };
        let (x, y) = state.pointer_pos;
        let position = state.physical(x, y);
        let modifiers = state.kb.modifiers;
        state.events.push(WindowEvent::Pinch {
            position,
            scale,
            modifiers,
            phase,
        });
    }
}

delegate_noop!(State: ignore zwp_pointer_gestures_v1::ZwpPointerGesturesV1);

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { fd, size, .. } => state.kb.set_keymap(fd, size),
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.keyboard_here = state.owns(&surface);
                if !state.keyboard_here {
                    return;
                }
                state.input_serial = serial;
                state.keyboard_surface = surface_id(&surface);
                state.keyboard_focused = true;
            }
            wl_keyboard::Event::Leave { .. } => {
                state.kb.clear_modifiers();
                if !std::mem::take(&mut state.keyboard_here) {
                    return;
                }
                state.kb.repeat = None;
                state.keyboard_focused = false;
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                // Wayland broadcasts `Modifiers` to every `wl_keyboard` of the focused client.
                // A window without focus (such as a `Keyboard::None` layer surface) does not get
                // its own `Leave` when the focused surface is destroyed, so taking modifiers
                // while unfocused would leave them stuck on it.
                if !state.keyboard_here {
                    return;
                }
                state
                    .kb
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.kb.repeat_rate = rate.max(0) as u32;
                state.kb.repeat_delay = std::time::Duration::from_millis(delay.max(0) as u64);
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: key_state,
                ..
            } => {
                if !state.keyboard_here {
                    return;
                }
                state.input_serial = serial;
                let pressed = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if pressed {
                    state.press_serial = serial;
                    if state.kb.repeat_rate > 0 && state.kb.repeats(key) {
                        state.kb.repeat = Some(Repeat {
                            keycode: key,
                            next: Instant::now() + state.kb.repeat_delay,
                        });
                    }
                } else if state.kb.repeat.as_ref().is_some_and(|r| r.keycode == key) {
                    state.kb.repeat = None;
                }
                if let Some(ev) = state.kb.key_event(key, pressed, false) {
                    state.events.push(WindowEvent::Key(ev));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match &event {
            zwp_text_input_v3::Event::Enter { surface } if !state.owns(surface) => return,
            zwp_text_input_v3::Event::Enter { .. } => {}
            _ if !state.ti_entered => return,
            _ => {}
        }
        match event {
            zwp_text_input_v3::Event::Enter { .. } => {
                state.ti_entered = true;
                state.ti_enabled = false;
                state.apply_ime();
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                state.ti_entered = false;
                state.ti_enabled = false;
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let cursor = (cursor_begin >= 0 && cursor_end >= 0)
                    .then_some((cursor_begin as usize, cursor_end as usize));
                state.pending_ime.preedit = text.map(|t| (t, cursor));
            }
            zwp_text_input_v3::Event::CommitString { text } => state.pending_ime.commit = text,
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.pending_ime.delete = Some((before_length as usize, after_length as usize));
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let p = std::mem::take(&mut state.pending_ime);
                state.events.push(WindowEvent::Ime(ImeUpdate {
                    preedit: p.preedit,
                    commit: p.commit,
                    delete_surrounding: p.delete,
                }));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(old) = state.selection_offer.take()
                    && Some(&old) != id.as_ref()
                {
                    state.offers.remove(&old.id());
                    old.destroy();
                }
                state.selection_offer = id;
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                state.drag_here = state.owns(&surface);
                if state.drag_here {
                    state.drag_entered(serial, surface_id(&surface), x, y, id);
                } else if let Some(offer) = id {
                    // It is over another window of the session, which has an offer of its own.
                    state.offers.remove(&offer.id());
                    offer.destroy();
                }
            }
            wl_data_device::Event::Motion { x, y, .. } if state.drag_here => state.drag_moved(x, y),
            wl_data_device::Event::Leave if std::mem::take(&mut state.drag_here) => {
                state.drag_left()
            }
            wl_data_device::Event::Drop if state.drag_here => state.drag_dropped(),
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                state.offers.entry(offer.id()).or_default().push(mime_type);
            }
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => state.drag.offer_action = action,
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { fd, mime_type } => {
                if let Some((s, text)) = &state.data_source
                    && s == source
                {
                    write_async(fd, text.clone());
                } else {
                    state.drag_data_wanted(source, &mime_type, fd);
                }
            }
            wl_data_source::Event::Cancelled => {
                if state.data_source.as_ref().is_some_and(|(s, _)| s == source) {
                    state.data_source = None;
                }
                state.drag_source_cancelled(source);
                source.destroy();
            }
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => state.drag.source_action = action,
            wl_data_source::Event::DndFinished => {
                state.drag_source_finished(source);
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                state.primary_offers.insert(offer.id(), Vec::new());
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                if let Some(old) = state.primary_offer.take()
                    && Some(&old) != id.as_ref()
                {
                    state.primary_offers.remove(&old.id());
                    old.destroy();
                }
                state.primary_offer = id;
            }
            _ => {}
        }
    }

    event_created_child!(State, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            state
                .primary_offers
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_source_v1::Event::Send { fd, .. } => {
                if let Some((s, text)) = &state.primary_source
                    && s == source
                {
                    write_async(fd, text.clone());
                }
            }
            zwp_primary_selection_source_v1::Event::Cancelled => {
                if state
                    .primary_source
                    .as_ref()
                    .is_some_and(|(s, _)| s == source)
                {
                    state.primary_source = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGER: Option<wl_pointer::AxisSource> = Some(wl_pointer::AxisSource::Finger);

    fn motion(source: Option<wl_pointer::AxisSource>, dy: f64) -> PointerFrame {
        PointerFrame {
            scroll_y: dy,
            has_scroll: true,
            source,
            ..Default::default()
        }
    }

    fn stop() -> PointerFrame {
        PointerFrame {
            source: FINGER,
            stopped: true,
            ..Default::default()
        }
    }

    #[test]
    fn fingers_begin_move_and_lift() {
        let mut fingers = false;
        let f = motion(FINGER, 10.0);
        assert_eq!(
            f.scrolls(2.0, &mut fingers),
            [(0.0, 100.0, ScrollSource::Touchpad, ScrollPhase::Began)]
        );
        assert_eq!(
            f.scrolls(2.0, &mut fingers),
            [(0.0, 100.0, ScrollSource::Touchpad, ScrollPhase::Moved)]
        );
        let lifted = (
            0.0,
            0.0,
            ScrollSource::Touchpad,
            ScrollPhase::Lifted { fling: true },
        );
        assert_eq!(stop().scrolls(2.0, &mut fingers), [lifted]);
        assert!(!fingers);
        // The next motion is a new gesture; a stray stop is nothing.
        assert!(stop().scrolls(2.0, &mut fingers).is_empty());
        assert_eq!(f.scrolls(2.0, &mut fingers)[0].3, ScrollPhase::Began);
    }

    #[test]
    fn one_axis_stopping_is_not_the_fingers_leaving() {
        let mut fingers = true;
        let f = PointerFrame {
            stopped: true,
            ..motion(FINGER, 10.0)
        };
        assert_eq!(f.scrolls(1.0, &mut fingers).len(), 1);
        assert_eq!(f.scrolls(1.0, &mut fingers)[0].3, ScrollPhase::Moved);
        assert!(fingers);
    }

    #[test]
    fn wheels_are_in_notches() {
        let mut fingers = false;
        for source in [
            Some(wl_pointer::AxisSource::Wheel),
            Some(wl_pointer::AxisSource::WheelTilt),
            None,
        ] {
            let f = PointerFrame {
                wheel_y: 60,
                ..motion(source, 7.5)
            };
            assert_eq!(
                f.scrolls(2.0, &mut fingers),
                [(0.0, 0.5, ScrollSource::Wheel, ScrollPhase::Single)]
            );
        }
        assert!(!fingers);
    }

    #[test]
    fn other_sources_never_start_inertia() {
        let mut fingers = false;
        for (source, dy) in [
            (Some(wl_pointer::AxisSource::Continuous), 50.0),
            (Some(wl_pointer::AxisSource::Wheel), 10.0),
            (None, 10.0),
        ] {
            assert_eq!(
                motion(source, 5.0).scrolls(2.0, &mut fingers),
                [(0.0, dy, ScrollSource::Unknown, ScrollPhase::Single)]
            );
            let f = PointerFrame {
                source,
                stopped: true,
                ..Default::default()
            };
            assert!(f.scrolls(2.0, &mut fingers).is_empty());
        }
    }
}
