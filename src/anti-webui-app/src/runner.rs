//! Runs the UI on whichever backend there is: Wayland or X11 (see "バックエンドの選び方" in
//! `docs/design/35-platform-x11.md`), AppKit on macOS (`38-platform-macos.md`), or Win32 on Windows
//! (`39-platform-windows.md`), with as many
//! windows as the program opens, on the threads it chooses (see `docs/design/36-multi-window.md`).

use super::App;
use crate::frame_stats::FrameStats;
use crate::host::Host;
use crate::pacing::Pacing;
use crate::perf_overlay::{LAST_WORD, OverlayFeed};
use crate::present::{Presented, Presenter};
use anti_webui_core::Ui;
use anti_webui_core::event::WindowEvent;
use anti_webui_core::geometry::SizeI;
use anti_webui_core::layer::{LayerChange, LayerSize, LayerSpec};
use anti_webui_core::platform::{PlatformRequest, PopupId, SurfaceId};
use anti_webui_core::video::VideoTiming;
use anti_webui_core::view::IntoView;
use anti_webui_core::window::{
    Command, DecorationMode, OpenRequest, Role, SpawnRequest, ThreadRequest, WindowHandle, WindowId,
};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

type BoxError = Box<dyn std::error::Error>;

struct Runner {
    ui: Ui,
    presenter: Option<Presenter>,
    error: Option<Box<dyn std::error::Error>>,
    /// `ANTIWEBUI_DUMP_FRAME=path.png` writes every presented frame to a file (debugging).
    dump: Option<(String, anti_webui_wgpu::Renderer)>,
    /// `ANTIWEBUI_FRAME_STATS=path.csv` records what every frame took (see `frame_stats`).
    stats: Option<FrameStats>,
    /// What frames take, over the window (see `perf_overlay`).
    overlay: OverlayFeed,
    pacing: Pacing,
    /// Decoded pictures say so through the event loop.
    waker_set: bool,
    /// For a layer surface: the axes that follow what is in it, the size last asked for them, and
    /// the size the surface has, in logical pixels.
    fit: (bool, bool),
    asked: Option<SizeI>,
    logical: SizeI,
}

impl Runner {
    fn new(ui: Ui) -> Self {
        Self {
            ui,
            presenter: None,
            error: None,
            dump: None,
            stats: FrameStats::from_env(),
            overlay: OverlayFeed::new(),
            pacing: Pacing::new(),
            waker_set: false,
            fit: (false, false),
            asked: None,
            logical: SizeI::default(),
        }
    }

    /// Makes the axes given follow what is in the window.
    fn set_fit(&mut self, fit: (bool, bool)) {
        self.fit = fit;
        self.asked = None;
        self.ui.set_fit_content(fit.0, fit.1);
    }

    /// Asks for the size of what is in a layer surface when it changed. Whether the frame should
    /// wait for the surface to have it.
    fn follow_content(&mut self, ctx: &mut impl Host) -> bool {
        if !self.fit.0 && !self.fit.1 {
            return false;
        }
        let Some(content) = self.ui.content_size() else {
            return false;
        };
        let want = SizeI::new(
            if self.fit.0 {
                content.width
            } else {
                self.logical.width
            },
            if self.fit.1 {
                content.height
            } else {
                self.logical.height
            },
        );
        if self.asked == Some(want) {
            return false;
        }
        self.asked = Some(want);
        ctx.set_layer_content_size(want);
        if want == self.logical {
            return false;
        }
        // The compositor answers with a configure, and the frame comes with that size; if it
        // gives another one, the frame comes anyway.
        ctx.request_redraw_after(std::time::Duration::from_millis(100));
        true
    }

    fn set_waker(&mut self, ctx: &mut impl Host) {
        if !std::mem::replace(&mut self.waker_set, true)
            && let Some(waker) = ctx.waker()
        {
            self.ui.set_image_waker(waker);
            self.ui.set_platform_drag(ctx.supports_drag());
        }
    }

    fn process(&mut self, ctx: &mut impl Host) {
        self.set_waker(ctx);
        self.handle_requests(ctx);
        if self.ui.needs_redraw() {
            ctx.request_redraw();
        }
    }

    fn handle_requests(&mut self, ctx: &mut impl Host) {
        for req in self.ui.take_requests() {
            match req {
                PlatformRequest::SetCursor(c) => ctx.set_cursor(c),
                PlatformRequest::SetIme(state) => ctx.set_ime(state),
                PlatformRequest::SetClipboard { kind, text } => ctx.set_clipboard(kind, text),
                PlatformRequest::Paste(kind) => ctx.request_paste(kind),
                PlatformRequest::SetTitle(t) => ctx.set_title(&t),
                PlatformRequest::SetFullscreen(on) => ctx.set_fullscreen(on),
                PlatformRequest::Close => ctx.exit(),
                PlatformRequest::ShowPopup(params) => ctx.show_popup(params),
                PlatformRequest::UpdatePopup(params) => ctx.update_popup(params),
                PlatformRequest::ClosePopup(id) => {
                    self.forget_popup(ctx, id);
                    ctx.close_popup(id);
                }
                PlatformRequest::StartDrag(offer) => ctx.start_drag(offer),
                PlatformRequest::CancelDrag => ctx.cancel_drag(),
                PlatformRequest::CloseDragIcon => {
                    if let Some(p) = &mut self.presenter {
                        p.forget(ctx, SurfaceId::DragIcon);
                    }
                    ctx.close_drag_icon();
                }
                PlatformRequest::AcceptDrag(wanted) => ctx.accept_drag(wanted),
                PlatformRequest::ReceiveDrop(mime) => ctx.receive_drop(mime),
                PlatformRequest::FinishDrop => ctx.finish_drop(),
                PlatformRequest::StartMove => ctx.start_move(),
                PlatformRequest::StartResize(edge) => ctx.start_resize(edge),
                PlatformRequest::Minimize => ctx.minimize(),
                PlatformRequest::SetMaximized(on) => ctx.set_maximized(on),
                PlatformRequest::ShowWindowMenu => ctx.show_window_menu(),
                PlatformRequest::Layer(change) => {
                    if let LayerChange::Size(w, h) = change {
                        self.set_fit((w == LayerSize::Content, h == LayerSize::Content));
                    }
                    ctx.change_layer(change);
                }
            }
        }
    }

    /// What is on a popup and what closes with it has to go before their surfaces.
    fn forget_popup(&mut self, ctx: &mut impl Host, id: PopupId) {
        for child in ctx.popups_attached_to(id) {
            self.forget_popup(ctx, child);
        }
        if let Some(p) = &mut self.presenter {
            p.forget(ctx, SurfaceId::Popup(id));
        }
    }

    fn dump(&mut self, surface: SurfaceId, suffix: &str) {
        let Some(presenter) = &self.presenter else {
            return;
        };
        let Some(scene) = presenter.last_scene(surface) else {
            return;
        };
        if self.dump.is_none()
            && let Ok(path) = std::env::var("ANTIWEBUI_DUMP_FRAME")
        {
            // Only Linux reads frames of videos on the GPU with it.
            #[allow(unused_mut)]
            let mut r = anti_webui_wgpu::Renderer::new(
                &presenter.gpu().device,
                anti_webui_wgpu::wgpu::TextureFormat::Rgba8Unorm,
            );
            #[cfg(target_os = "linux")]
            if let Some(importer) = presenter.video_importer() {
                r.set_video_importer(importer);
            }
            self.dump = Some((path, r));
        }
        if let Some((path, renderer)) = &mut self.dump {
            renderer.set_target(match surface {
                SurfaceId::Window => 0,
                SurfaceId::Popup(id) => id.0,
                SurfaceId::DragIcon => u64::MAX,
            });
            let pixels = presenter.gpu().render_to_rgba(renderer, scene);
            let path = format!("{path}{suffix}");
            let _ = crate::headless::write_png(path.as_str(), scene.width, scene.height, &pixels);
            eprintln!(
                "[anti-webui] frame {}x{} scale {:.3} -> {path}",
                scene.width,
                scene.height,
                self.ui.scale().as_f64()
            );
        }
    }

    fn on_event(&mut self, ctx: &mut impl Host, event: WindowEvent) {
        if let WindowEvent::Resized { size, scale } = &event {
            self.logical = SizeI::new(
                scale.logical_ceil(size.width),
                scale.logical_ceil(size.height),
            );
            // A size asked for again later is asked for again.
            self.asked = None;
        }
        if let (WindowEvent::Resized { .. }, Some(p)) = (&event, &mut self.presenter) {
            p.drop_held(SurfaceId::Window);
        }
        self.ui.handle_event(event);
        self.process(ctx);
    }

    fn on_woken(&mut self, ctx: &mut impl Host) {
        self.ui.poll_images();
        self.process(ctx);
    }

    fn on_frame(&mut self, ctx: &mut impl Host) {
        self.set_waker(ctx);
        if self.presenter.is_none() {
            match Presenter::new(ctx) {
                Ok(mut p) => {
                    // What writes frames to files draws them again, with its own renderer.
                    p.keep_picture_pixels(std::env::var_os("ANTIWEBUI_DUMP_FRAME").is_some());
                    self.ui.set_video_render_node(p.video_render_node());
                    self.presenter = Some(p);
                }
                Err(e) => {
                    self.error = Some(Box::new(e));
                    ctx.exit();
                    return;
                }
            }
        }
        if self.fit.0 || self.fit.1 {
            self.ui.update();
            if self.follow_content(ctx) {
                return;
            }
        }
        self.overlay.update();
        let own = self.overlay.is_own_frame(self.ui.needs_redraw());
        let (stats, overlay) = (&mut self.stats, &mut self.overlay);
        self.pacing.begin_frame(ctx, |frame| {
            if let Some(s) = stats {
                s.add(frame);
            }
            overlay.add(frame);
        });
        let now = self.pacing.frame_time();
        self.ui
            .set_video_timing(self.pacing.refresh().map(|refresh| VideoTiming {
                refresh,
                vsync: self.pacing.last_refresh(),
            }));
        self.ui.animate(now);
        self.ui.update();
        // Not `process`: the frame painted below is the redraw it would ask for.
        self.handle_requests(ctx);
        let mut frame = self.ui.paint_frame(now);
        let trace = self.pacing.trace();
        self.overlay.paint(
            &mut frame.window,
            self.ui.scale(),
            self.pacing.advance_words(),
            trace.token,
            own,
        );
        let presenter = self.presenter.as_mut().unwrap();
        presenter.trace_next(trace);
        let window = presenter.present(ctx, SurfaceId::Window, frame.window);
        if presenter.take_video_import_failed() {
            self.ui.video_use_memory();
        }
        let mut results = vec![(SurfaceId::Window, window)];
        for (id, scene) in frame.popups {
            let surface = SurfaceId::Popup(id);
            results.push((surface, presenter.present(ctx, surface, scene)));
        }
        if let Some(icon) = frame.drag_icon {
            ctx.place_drag_icon(icon.logical, icon.hotspot);
            let surface = SurfaceId::DragIcon;
            results.push((surface, presenter.present(ctx, surface, icon.scene)));
        }
        if let Some(trace) = presenter.take_trace() {
            let feedback = ctx.presentation_clock().is_some();
            self.pacing
                .end_frame(trace, results[0].1 == Presented::Yes, feedback);
        }
        if let Some(stats) = &mut self.stats {
            stats.describe(presenter.gpu(), presenter.path());
        }
        self.overlay.describe(presenter.gpu(), presenter.path());
        let mut committed = false;
        let mut retry = false;
        for (i, (surface, presented)) in results.into_iter().enumerate() {
            committed |= presented == Presented::Yes;
            retry |= presented == Presented::Retry;
            if presented == Presented::Yes {
                let suffix = if i == 0 {
                    String::new()
                } else {
                    format!(".popup{i}.png")
                };
                self.dump(surface, &suffix);
            }
        }
        // Nothing was committed, so no frame callback will pace the next try.
        let soon = std::time::Duration::from_millis(4);
        if retry {
            ctx.request_redraw_after(soon);
        } else if !committed && self.ui.animating() {
            // Nothing to show this time: it may have something at the next refresh.
            let wait = self.pacing.until_next_refresh(ctx, Instant::now());
            ctx.request_redraw_after(wait.unwrap_or(soon));
        } else if self.ui.animating() {
            ctx.request_redraw();
        } else if committed && self.overlay.wants_last_word(own) {
            // The last frames are still on their way to the screen.
            ctx.request_redraw_after(LAST_WORD);
        }
    }

    fn on_surfaces_ready(&mut self, ctx: &mut impl Host) {
        if let Some(p) = &mut self.presenter
            && let Some((token, time)) = p.commit_held(ctx, SurfaceId::Window)
        {
            self.pacing.committed(token, time);
        }
    }

    fn on_may_render_ahead(&mut self, ctx: &mut impl Host) -> bool {
        // Popups show what was just asked for, and keep in step with the window.
        !ctx.has_popups()
            && ctx.frame_pending(SurfaceId::Window)
            && self
                .presenter
                .as_mut()
                .is_some_and(|p| p.can_hold_back(SurfaceId::Window, self.pacing.max_advance()))
    }

    fn on_timer(&mut self, ctx: &mut impl Host, now: Instant) {
        self.ui.tick(now);
        self.process(ctx);
    }

    fn on_exit(&mut self) {
        if let Some(mut stats) = self.stats.take() {
            self.pacing.drain(&mut |frame| stats.add(frame));
            stats.finish();
        }
        // GPU objects must go before the surface they render to.
        self.dump = None;
        self.presenter = None;
    }
}

/// The backends' handler traits have the same methods; each is answered by the runner's.
macro_rules! handler_for {
    ($handler:path, $ctx:ty) => {
        impl $handler for Runner {
            fn event(&mut self, ctx: &mut $ctx, event: WindowEvent) {
                self.on_event(ctx, event)
            }
            fn frame(&mut self, ctx: &mut $ctx) {
                self.on_frame(ctx)
            }
            fn deadline(&mut self, now: Instant) -> Option<Instant> {
                self.ui.next_deadline(now)
            }
            fn timer(&mut self, ctx: &mut $ctx, now: Instant) {
                self.on_timer(ctx, now)
            }
            fn exit(&mut self) {
                self.on_exit()
            }
            fn surfaces_ready(&mut self, ctx: &mut $ctx) {
                self.on_surfaces_ready(ctx)
            }
            fn may_render_ahead(&mut self, ctx: &mut $ctx) -> bool {
                self.on_may_render_ahead(ctx)
            }
            fn woken(&mut self, ctx: &mut $ctx) {
                self.on_woken(ctx)
            }
        }
    };
}

#[cfg(all(target_os = "linux", feature = "wayland"))]
handler_for!(
    anti_webui_wayland::WaylandHandler,
    anti_webui_wayland::Ctx<'_>
);
#[cfg(all(target_os = "linux", feature = "x11"))]
handler_for!(anti_webui_x11::X11Handler, anti_webui_x11::Ctx<'_>);
#[cfg(all(target_os = "macos", feature = "macos"))]
handler_for!(anti_webui_macos::MacHandler, anti_webui_macos::Ctx<'_>);
#[cfg(all(windows, feature = "windows"))]
handler_for!(anti_webui_windows::WinHandler, anti_webui_windows::Ctx<'_>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    Wayland,
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11,
    #[cfg(all(target_os = "macos", feature = "macos"))]
    MacOS,
    #[cfg(all(windows, feature = "windows"))]
    Windows,
}

/// Win32 on Windows, which has nothing else.
#[cfg(windows)]
fn choose_backend() -> Result<Backend, BoxError> {
    match std::env::var("ANTIWEBUI_BACKEND").ok().as_deref() {
        None | Some("" | "windows") => {}
        Some(other) => return Err(format!("ANTIWEBUI_BACKEND={other} isn't built in").into()),
    }
    #[cfg(feature = "windows")]
    return Ok(Backend::Windows);
    #[cfg(not(feature = "windows"))]
    Err("the windows feature is off".into())
}

/// AppKit on macOS, which has nothing else.
#[cfg(target_os = "macos")]
fn choose_backend() -> Result<Backend, BoxError> {
    match std::env::var("ANTIWEBUI_BACKEND").ok().as_deref() {
        None | Some("" | "macos") => {}
        Some(other) => return Err(format!("ANTIWEBUI_BACKEND={other} isn't built in").into()),
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    return Ok(Backend::MacOS);
    #[cfg(not(feature = "macos"))]
    Err("the macos feature is off".into())
}

/// `ANTIWEBUI_BACKEND` if given, otherwise Wayland if it can be reached, and X11 after it.
#[cfg(target_os = "linux")]
fn choose_backend() -> Result<Backend, BoxError> {
    let wanted = std::env::var("ANTIWEBUI_BACKEND").ok();
    match wanted.as_deref() {
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        Some("wayland") => return Ok(Backend::Wayland),
        #[cfg(all(target_os = "linux", feature = "x11"))]
        Some("x11") => return Ok(Backend::X11),
        Some(other) if !other.is_empty() => {
            return Err(format!("ANTIWEBUI_BACKEND={other} isn't built in").into());
        }
        _ => {}
    }
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && anti_webui_wayland::can_connect() {
        return Ok(Backend::Wayland);
    }
    #[cfg(all(target_os = "linux", feature = "x11"))]
    if std::env::var_os("DISPLAY").is_some() {
        return Ok(Backend::X11);
    }
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        // `WAYLAND_DISPLAY` may be left out for the default socket.
        return Ok(Backend::Wayland);
    }
    Err("found neither a Wayland compositor nor an X server".into())
}

/// Before any thread of the app's: it sets the environment of the process.
fn limit_vulkan_drivers(device: Option<u64>) {
    #[cfg(not(target_os = "linux"))]
    let _ = device;
    #[cfg(target_os = "linux")]
    if let Some(device) = device {
        // SAFETY: the app's views, and with them its threads, aren't made yet.
        let drivers = unsafe { anti_webui_wgpu::limit_vulkan_drivers(device) };
        if let Some(d) = drivers {
            log::debug!("Vulkan drivers of the display server's GPU only: {d}");
        }
    }
}

// --- Windows ----------------------------------------------------------------------------------

/// How to open a window on the platform.
struct Spec {
    title: String,
    app_id: String,
    width: i32,
    height: i32,
    fullscreen: bool,
    decorations: DecorationMode,
    modal: bool,
    /// What the system is told the window belongs to, for a dialog.
    parent: Option<WindowId>,
    layer: Option<LayerSpec>,
    /// The output a lock surface covers.
    lock: Option<String>,
}

impl Spec {
    fn of(request: &OpenRequest, app_id: &str) -> Self {
        let o = &request.options;
        Self {
            title: o.title.clone(),
            app_id: app_id.to_owned(),
            width: o.width,
            height: o.height,
            fullscreen: o.fullscreen,
            decorations: o.decorations,
            modal: o.modal,
            // Only a dialog is the system's business: other windows get all of their buttons.
            parent: request
                .parent
                .as_ref()
                .filter(|_| o.dialog || o.modal)
                .map(WindowHandle::id),
            layer: match &o.role {
                Role::Layer(spec) => Some((**spec).clone()),
                _ => None,
            },
            lock: match &o.role {
                Role::Lock(output) => Some(output.clone()),
                _ => None,
            },
        }
    }
}

/// What a window can be put above by, whichever thread it is on.
#[derive(Clone)]
enum Link {
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    Wayland(anti_webui_wayland::ParentLink),
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11(anti_webui_x11::ParentLink),
    #[cfg(all(target_os = "macos", feature = "macos"))]
    MacOS(anti_webui_macos::ParentLink),
    #[cfg(all(windows, feature = "windows"))]
    Windows(anti_webui_windows::ParentLink),
}

/// The open windows of every thread, for the windows that open above them.
static LINKS: Mutex<Vec<(WindowId, Link)>> = Mutex::new(Vec::new());

/// The threads windows were spawned on, which the program waits for at the end.
static THREADS: Mutex<Vec<(WindowHandle, JoinHandle<()>)>> = Mutex::new(Vec::new());

fn link_of(id: WindowId) -> Option<Link> {
    let links = LINKS.lock().ok()?;
    links.iter().find(|(w, _)| *w == id).map(|(_, l)| l.clone())
}

/// A backend's session: the windows of one thread.
trait Platform: Sized + 'static {
    type Key: Copy + Eq + std::fmt::Debug;
    type Share: Send + 'static;
    fn connect() -> Result<Self, BoxError>;
    fn join(share: Self::Share) -> Result<Self, BoxError>;
    fn share(&self) -> Self::Share;
    /// Whether windows from `spawn_window` get threads of their own.
    fn own_threads() -> bool;
    /// Waits a little while threads end. On macOS, the main thread does what they ask of it
    /// meanwhile, as AppKit's windows are its (see `docs/design/38-platform-macos.md`).
    fn idle(&mut self, timeout: std::time::Duration);
    fn waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>>;
    fn open(&mut self, spec: Spec) -> Result<Self::Key, BoxError>;
    fn link(&self, key: Self::Key) -> Option<Link>;
    fn close(&mut self, key: Self::Key);
    fn exited(&self) -> Vec<Self::Key>;
    fn request_redraw(&mut self, key: Self::Key);
    fn redraw_pending(&self, key: Self::Key) -> bool;
    fn dispatch(&mut self, windows: &mut Windows<Self::Key>) -> Result<(), BoxError>;
    fn wait(
        &mut self,
        windows: &mut Windows<Self::Key>,
        until: Option<Instant>,
    ) -> Result<(), BoxError>;
}

/// The windows of a thread and what runs each.
struct Windows<K> {
    entries: Vec<Entry<K>>,
}

struct Entry<K> {
    key: K,
    id: WindowId,
    runner: Runner,
}

impl<K: Copy + Eq> Windows<K> {
    fn find(&mut self, key: K) -> Option<&mut Runner> {
        self.entries
            .iter_mut()
            .find(|e| e.key == key)
            .map(|e| &mut e.runner)
    }
}

/// Implements [`Platform`] for a backend's session, which has methods of these names.
macro_rules! platform_for {
    (@idle macos, $session:expr, $timeout:expr) => {
        $session.idle($timeout)
    };
    (@idle $backend:ident, $session:expr, $timeout:expr) => {
        std::thread::sleep($timeout)
    };
    ($backend:ident, $variant:ident) => {
        impl $backend::Handlers for Windows<$backend::WindowKey> {
            type Handler = Runner;
            fn handler(&mut self, key: $backend::WindowKey) -> Option<&mut Runner> {
                self.find(key)
            }
        }

        impl Platform for $backend::Session {
            type Key = $backend::WindowKey;
            type Share = $backend::SessionShare;
            fn connect() -> Result<Self, BoxError> {
                Ok($backend::Session::connect()?)
            }
            fn join(share: Self::Share) -> Result<Self, BoxError> {
                Ok($backend::Session::join(share)?)
            }
            fn share(&self) -> Self::Share {
                $backend::Session::share(self)
            }
            fn own_threads() -> bool {
                $backend::Session::OWN_THREADS
            }
            fn idle(&mut self, timeout: std::time::Duration) {
                platform_for!(@idle $backend, self, timeout)
            }
            fn waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
                let waker = $backend::Session::waker(self)?;
                Some(Arc::new(waker))
            }
            fn open(&mut self, spec: Spec) -> Result<Self::Key, BoxError> {
                let parent = spec.parent.and_then(link_of).map(|link| match link {
                    Link::$variant(l) => l,
                    #[allow(unreachable_patterns)]
                    _ => unreachable!("one backend per program"),
                });
                let options = $backend::WindowOptions {
                    title: spec.title,
                    app_id: spec.app_id,
                    width: spec.width,
                    height: spec.height,
                    fullscreen: spec.fullscreen,
                    decorations: spec.decorations,
                    modal: spec.modal,
                    parent,
                    layer: spec.layer,
                    lock: spec.lock,
                };
                Ok($backend::Session::open(self, options)?)
            }
            fn link(&self, key: Self::Key) -> Option<Link> {
                self.parent_link(key).map(Link::$variant)
            }
            fn close(&mut self, key: Self::Key) {
                $backend::Session::close(self, key)
            }
            fn exited(&self) -> Vec<Self::Key> {
                $backend::Session::exited(self)
            }
            fn request_redraw(&mut self, key: Self::Key) {
                $backend::Session::request_redraw(self, key)
            }
            fn redraw_pending(&self, key: Self::Key) -> bool {
                $backend::Session::redraw_pending(self, key)
            }
            fn dispatch(&mut self, windows: &mut Windows<Self::Key>) -> Result<(), BoxError> {
                Ok($backend::Session::dispatch(self, windows)?)
            }
            fn wait(
                &mut self,
                windows: &mut Windows<Self::Key>,
                until: Option<Instant>,
            ) -> Result<(), BoxError> {
                Ok($backend::Session::wait_until(self, windows, until)?)
            }
        }
    };
}

#[cfg(all(target_os = "linux", feature = "wayland"))]
use anti_webui_wayland as wayland;
#[cfg(all(target_os = "linux", feature = "wayland"))]
platform_for!(wayland, Wayland);
#[cfg(all(target_os = "linux", feature = "x11"))]
use anti_webui_x11 as x11;
#[cfg(all(target_os = "linux", feature = "x11"))]
platform_for!(x11, X11);
#[cfg(all(target_os = "macos", feature = "macos"))]
use anti_webui_macos as macos;
#[cfg(all(target_os = "macos", feature = "macos"))]
platform_for!(macos, MacOS);
#[cfg(all(windows, feature = "windows"))]
use anti_webui_windows as windows;
#[cfg(all(windows, feature = "windows"))]
platform_for!(windows, Windows);

/// The loop of a thread with windows: it ends when they are all closed, or with the first window
/// of the program.
struct Thread<P: Platform> {
    // Fields drop in this order: the windows first. Their GPU surfaces free their buffers through
    // the session's connection (Vulkan's Wayland swapchain destroys its wl_buffers), so the
    // connection must still be there; dropped after it, they crashed in libwayland.
    windows: Windows<P::Key>,
    session: P,
    /// The program's first window, which ends it.
    first: Option<P::Key>,
    app_id: String,
}

impl<P: Platform> Thread<P> {
    fn new(session: P, app_id: String) -> Self {
        if let Some(waker) = session.waker() {
            anti_webui_core::channel::set_thread_waker(waker);
        }
        Self {
            session,
            windows: Windows {
                entries: Vec::new(),
            },
            first: None,
            app_id,
        }
    }

    /// Puts a `Ui` in a window of the platform.
    fn add(&mut self, ui: Ui, spec: Spec) -> Result<P::Key, BoxError> {
        let id = ui.window();
        let key = match self.session.open(spec) {
            Ok(key) => key,
            Err(e) => {
                ui.close();
                return Err(e);
            }
        };
        if let Some(link) = self.session.link(key)
            && let Ok(mut links) = LINKS.lock()
        {
            links.push((id, link));
        }
        self.windows.entries.push(Entry {
            key,
            id,
            runner: Runner::new(ui),
        });
        Ok(key)
    }

    fn open(&mut self, request: OpenRequest) {
        let mut spec = Spec::of(&request, &self.app_id);
        let mut ui = Ui::open(request);
        let fit = spec.layer.as_ref().map(LayerSpec::fits_content);
        if let (Some(fit), Some(layer)) = (fit, &spec.layer)
            && (fit.0 || fit.1)
        {
            // What it asks for at first, before it knows how much room it has.
            let natural = ui.measure_content();
            if layer.width == LayerSize::Content {
                spec.width = natural.width;
            }
            if layer.height == LayerSize::Content {
                spec.height = natural.height;
            }
        }
        match self.add(ui, spec) {
            Ok(key) => {
                if let (Some(fit), Some(runner)) = (fit, self.windows.find(key)) {
                    runner.set_fit(fit);
                }
            }
            Err(e) => eprintln!("[anti-webui] could not open a window: {e}"),
        }
    }

    /// Closes a window; the error of its runner, if it had one.
    fn close(&mut self, key: P::Key) -> Option<BoxError> {
        let at = self.windows.entries.iter().position(|e| e.key == key)?;
        let Entry { id, mut runner, .. } = self.windows.entries.remove(at);
        // GPU objects go before the surface they render to.
        runner.on_exit();
        if let Ok(mut links) = LINKS.lock() {
            links.retain(|(w, _)| *w != id);
        }
        let error = runner.error.take();
        let Runner { ui, .. } = runner;
        ui.close();
        self.session.close(key);
        error
    }

    /// Closes every window and stops what keeps the thread going without one, for [`quit`].
    ///
    /// [`quit`]: anti_webui_core::window::quit
    fn end(&mut self) {
        let keys: Vec<_> = self.windows.entries.iter().map(|e| e.key).collect();
        for key in keys {
            if let Some(e) = self.close(key) {
                eprintln!("[anti-webui] a window closed: {e}");
            }
        }
        for root in anti_webui_core::window::roots() {
            anti_webui_core::window::stop_root(root);
        }
    }

    /// Runs until the windows are gone, and what keeps the thread going without one. With the
    /// first window of the program, they all go with it.
    fn run(&mut self) -> Result<(), BoxError> {
        loop {
            if anti_webui_core::window::quitting() {
                self.end();
                return Ok(());
            }
            self.session.dispatch(&mut self.windows)?;
            anti_webui_core::window::run_roots(Instant::now());
            for key in self.session.exited() {
                let error = self.close(key);
                if Some(key) == self.first {
                    let keys: Vec<_> = self.windows.entries.iter().map(|e| e.key).collect();
                    for key in keys {
                        self.close(key);
                    }
                    return error.map_or(Ok(()), Err);
                }
                if let Some(e) = error {
                    eprintln!("[anti-webui] a window closed: {e}");
                }
            }
            self.take_requests();
            if self.windows.entries.is_empty() && !anti_webui_core::window::has_roots() {
                return Ok(());
            }
            self.redraw_changed();
            if !anti_webui_core::window::has_pending() {
                let until = anti_webui_core::window::roots_deadline();
                self.session.wait(&mut self.windows, until)?;
            }
        }
    }

    /// Draws the windows that something else on the thread changed, like a signal written in the
    /// handler or timer of another window, or of a root.
    fn redraw_changed(&mut self) {
        if self.windows.entries.len() < 2 && !anti_webui_core::window::has_roots() {
            return;
        }
        for e in &mut self.windows.entries {
            if !self.session.redraw_pending(e.key) && e.runner.ui.needs_redraw() {
                self.session.request_redraw(e.key);
            }
        }
    }

    /// Opens, spawns and closes what was asked for while handling events.
    fn take_requests(&mut self) {
        for request in anti_webui_core::window::take_opens() {
            self.open(request);
        }
        for request in anti_webui_core::window::take_spawns() {
            self.spawn(request);
        }
        for request in anti_webui_core::window::take_threads() {
            self.spawn_thread(request);
        }
        let mut wake = false;
        let mut commands: std::collections::VecDeque<_> =
            anti_webui_core::window::take_commands().into();
        while let Some((id, command)) = commands.pop_front() {
            wake |= self.command(id, command);
            // What a command leads to, like a dialog that closed letting go of its parent, comes
            // before what was asked after it.
            for later in anti_webui_core::window::take_commands().into_iter().rev() {
                commands.push_front(later);
            }
        }
        // What the windows were told needs their handlers, which run when the session wakes.
        if wake && let Some(waker) = self.session.waker() {
            waker();
        }
    }

    /// Does what a handle asked of a window of this thread. Whether its handlers have to run.
    fn command(&mut self, id: WindowId, command: Command) -> bool {
        let Some(at) = self.windows.entries.iter().position(|e| e.id == id) else {
            // Not a window, but maybe what keeps the thread going without one.
            if command == Command::Close {
                anti_webui_core::window::stop_root(id);
            }
            return false;
        };
        let key = self.windows.entries[at].key;
        if command == Command::Close && Some(key) != self.first {
            if let Some(e) = self.close(key) {
                eprintln!("[anti-webui] a window closed: {e}");
            }
            return false;
        }
        // The first window closes through the loop, which ends the program.
        self.windows.entries[at].runner.ui.apply(command);
        true
    }

    fn spawn(&mut self, request: SpawnRequest) {
        if !P::own_threads() {
            self.open(request.start());
            return;
        }
        let handle = request.handle.clone();
        self.start_thread(handle, move |thread| {
            thread.open(request.start());
        });
    }

    fn spawn_thread(&mut self, request: ThreadRequest) {
        let handle = request.handle.clone();
        self.start_thread(handle, move |_| request.start());
    }

    /// A thread with a session of its own on this one's connection, which does `first` and runs.
    fn start_thread(
        &mut self,
        handle: WindowHandle,
        first: impl FnOnce(&mut Thread<P>) + Send + 'static,
    ) {
        let share = self.session.share();
        let app_id = self.app_id.clone();
        let thread = std::thread::Builder::new()
            .name("anti-webui window".into())
            .spawn(move || {
                if let Err(e) = run_spawned::<P>(share, app_id, first) {
                    eprintln!("[anti-webui] a window's thread ended: {e}");
                }
            });
        match thread {
            Ok(thread) => {
                if let Ok(mut threads) = THREADS.lock() {
                    threads.push((handle, thread));
                }
            }
            Err(e) => eprintln!("[anti-webui] could not start a thread for a window: {e}"),
        }
    }
}

/// The body of a thread a window was spawned on.
fn run_spawned<P: Platform>(
    share: P::Share,
    app_id: String,
    first: impl FnOnce(&mut Thread<P>),
) -> Result<(), BoxError> {
    #[cfg(feature = "video")]
    anti_webui_video::install();
    anti_webui_core::window::start_ui_thread(true);
    let mut thread = Thread::new(P::join(share)?, app_id);
    first(&mut thread);
    thread.run()
}

/// Closes the windows on other threads and waits for them, at the end of the program.
fn end_threads<P: Platform>(session: &mut P) {
    loop {
        let threads = std::mem::take(&mut *THREADS.lock().expect("no panics in here"));
        if threads.is_empty() {
            return;
        }
        for (handle, _) in &threads {
            handle.close();
        }
        for (_, thread) in threads {
            while !thread.is_finished() {
                session.idle(std::time::Duration::from_millis(5));
            }
            let _ = thread.join();
        }
    }
}

fn run_on<P: Platform, V: IntoView>(app: App, root: impl FnOnce() -> V) -> Result<(), BoxError> {
    let mut thread = Thread::new(P::connect()?, app.app_id.clone());
    let ui = Ui::new(root);
    let spec = Spec {
        title: app.title,
        app_id: app.app_id,
        width: app.width,
        height: app.height,
        fullscreen: app.fullscreen,
        decorations: app.decorations,
        modal: false,
        parent: None,
        layer: None,
        lock: None,
    };
    let key = thread.add(ui, spec)?;
    thread.first = Some(key);
    let result = thread.run();
    end_threads(&mut thread.session);
    result
}

/// Runs what `root` opens, with no window of its own, until [`quit`]. Wayland only: what it is for
/// are layer surfaces (see `docs/design/37-wayland-shell.md`).
///
/// [`quit`]: anti_webui_core::window::quit
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn run_shell(app: App, root: impl FnOnce()) -> Result<(), BoxError> {
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    {
        anti_webui_core::text::prepare_system_fonts();
        limit_vulkan_drivers(anti_webui_wayland::compositor_main_device());
        #[cfg(feature = "video")]
        anti_webui_video::install();
        anti_webui_core::window::start_ui_thread(false);
        let mut thread = Thread::new(anti_webui_wayland::Session::connect()?, app.app_id);
        let handle = anti_webui_core::window::current_window();
        anti_webui_core::window::start_root(handle, Box::new(root));
        let result = thread.run();
        end_threads(&mut thread.session);
        result
    }
    #[cfg(not(all(target_os = "linux", feature = "wayland")))]
    {
        let _ = (app, root);
        Err("a shell needs Wayland, which isn't built in".into())
    }
}

pub fn run<V: IntoView>(app: App, root: impl FnOnce() -> V) -> Result<(), BoxError> {
    // The main thread has 1 MiB on Windows, and 8 on Linux and macOS: the UI gets as much, on this
    // thread (see "スタック" in `docs/design/39-platform-windows.md`).
    #[cfg(all(windows, feature = "windows"))]
    return anti_webui_windows::with_stack(anti_webui_windows::UI_STACK, || run_here(app, root));
    #[cfg(not(all(windows, feature = "windows")))]
    run_here(app, root)
}

fn run_here<V: IntoView>(app: App, root: impl FnOnce() -> V) -> Result<(), BoxError> {
    // While the platform and the window are set up.
    anti_webui_core::text::prepare_system_fonts();
    let backend = choose_backend()?;
    limit_vulkan_drivers(match backend {
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        Backend::Wayland => anti_webui_wayland::compositor_main_device(),
        #[cfg(all(target_os = "linux", feature = "x11"))]
        Backend::X11 => anti_webui_x11::server_main_device(),
        #[cfg(all(target_os = "macos", feature = "macos"))]
        Backend::MacOS => None,
        #[cfg(all(windows, feature = "windows"))]
        Backend::Windows => None,
    });
    #[cfg(feature = "video")]
    anti_webui_video::install();
    anti_webui_core::window::start_ui_thread(false);
    match backend {
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        Backend::Wayland => run_on::<anti_webui_wayland::Session, V>(app, root),
        #[cfg(all(target_os = "linux", feature = "x11"))]
        Backend::X11 => run_on::<anti_webui_x11::Session, V>(app, root),
        #[cfg(all(target_os = "macos", feature = "macos"))]
        Backend::MacOS => run_on::<anti_webui_macos::Session, V>(app, root),
        #[cfg(all(windows, feature = "windows"))]
        Backend::Windows => run_on::<anti_webui_windows::Session, V>(app, root),
    }
}
