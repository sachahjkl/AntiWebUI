//! Pictures of windows and outputs, for thumbnails: `ext_image_copy_capture_v1`, or Hyprland's
//! `hyprland_toplevel_export_v1`. See "ウィンドウのサムネイル" in `docs/design/37-wayland-shell.md`.
//!
//! What the compositor tells comes in on the connection's thread and goes to one thread of its own,
//! which asks for frames, waits between them and makes them into bitmaps.

use crate::conn::{State as Conn, link, output_object};
use crate::output::{Output, OutputTransform};
use crate::protocols::hyprland_toplevel_export::{
    hyprland_toplevel_export_frame_v1 as hypr_frame,
    hyprland_toplevel_export_manager_v1 as hypr_manager,
};
use crate::toplevels::Toplevel;
use anti_webui_core::images::Bitmap;
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::view::{BitmapView, bitmap_view};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as toplevel_sources,
    ext_image_capture_source_v1 as source,
    ext_output_image_capture_source_manager_v1 as output_sources,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as ext_frame, ext_image_copy_capture_manager_v1 as ext_manager,
    ext_image_copy_capture_session_v1 as session,
};

/// What takes the pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CaptureSource {
    /// `ext_image_copy_capture_v1`, which most compositors that can have.
    Ext,
    /// `hyprland_toplevel_export_v1`: windows only.
    Hyprland,
}

/// What to take pictures of.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureTarget {
    Toplevel(Toplevel),
    Output(Output),
    /// A window by its address in Hyprland's IPC, like `0x55d1e7b0`.
    HyprlandAddress(u64),
}

impl From<Toplevel> for CaptureTarget {
    fn from(t: Toplevel) -> Self {
        Self::Toplevel(t)
    }
}

impl From<&Toplevel> for CaptureTarget {
    fn from(t: &Toplevel) -> Self {
        Self::Toplevel(t.clone())
    }
}

impl From<Output> for CaptureTarget {
    fn from(o: Output) -> Self {
        Self::Output(o)
    }
}

impl From<&Output> for CaptureTarget {
    fn from(o: &Output) -> Self {
        Self::Output(o.clone())
    }
}

#[cfg(feature = "hyprland")]
impl From<&crate::hyprland::Client> for CaptureTarget {
    fn from(c: &crate::hyprland::Client) -> Self {
        let hex = c.address.trim_start_matches("0x");
        Self::HyprlandAddress(u64::from_str_radix(hex, 16).unwrap_or(0))
    }
}

/// How to take pictures.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct CaptureOptions {
    /// The largest the picture may be, in pixels; what is larger is made smaller to fit, keeping
    /// its shape. For a box of some logical size, that times the output's scale.
    pub max_size: Option<(u32, u32)>,
    /// Takes another picture this long after the last, once what is shown has changed. `None`
    /// takes one.
    pub every: Option<Duration>,
    /// With the pointer drawn in.
    pub cursor: bool,
    /// What to take them with when there is a choice: [`CaptureSource::Ext`] unless this says
    /// otherwise.
    pub prefer: Option<CaptureSource>,
}

impl CaptureOptions {
    pub fn max_size(mut self, width: u32, height: u32) -> Self {
        self.max_size = Some((width.max(1), height.max(1)));
        self
    }

    pub fn every(mut self, interval: Duration) -> Self {
        self.every = Some(interval);
        self
    }

    pub fn cursor(mut self, on: bool) -> Self {
        self.cursor = on;
        self
    }

    pub fn prefer(mut self, source: CaptureSource) -> Self {
        self.prefer = Some(source);
        self
    }
}

/// Pictures of a window or an output, kept as the options say. Cheap to clone; it stops taking them
/// when the last clone goes. Reading it tracks it, on any thread.
#[derive(Clone, Debug)]
pub struct Capture(Arc<Handle>);

#[derive(Debug)]
struct Handle {
    job: Arc<Job>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        send(Msg::Stop(self.job.clone()));
    }
}

/// Starts taking pictures of `target`. When it can't be, [`Capture::available`] says so and there
/// are none.
pub fn capture(target: impl Into<CaptureTarget>, options: CaptureOptions) -> Capture {
    let job = Arc::new(Job {
        picture: shared_signal(None),
        size: shared_signal(None),
        stopped: shared_signal(false),
        source: None,
        options,
        state: Mutex::new(JobState::default()),
    });
    let job = match start(target.into(), job) {
        Ok(job) => job,
        Err(job) => {
            job.stopped.set(true);
            job
        }
    };
    Capture(Arc::new(Handle { job }))
}

/// Shows pictures of `target` as [`capture`] takes them, for as long as the view is there. The size
/// of the box is up to the style: with `object_fit`, the shape of the window can be kept.
pub fn thumbnail(target: impl Into<CaptureTarget>, options: CaptureOptions) -> BitmapView {
    let shot = capture(target, options);
    bitmap_view(move || shot.picture())
}

impl Capture {
    /// Whether there is something to take them with.
    pub fn available(&self) -> bool {
        self.0.job.source.is_some()
    }

    pub fn source(&self) -> Option<CaptureSource> {
        self.0.job.source
    }

    /// The last picture, made smaller to fit if it had to be.
    pub fn picture(&self) -> Option<Arc<Bitmap>> {
        self.0.job.picture.get()
    }

    /// How big what is pictured is, in pixels, before it was made smaller.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.0.job.size.get()
    }

    /// No more pictures will come: the window closed, or the compositor won't.
    pub fn stopped(&self) -> bool {
        self.0.job.stopped.get()
    }

    /// Takes another picture now, without waiting for the interval.
    pub fn refresh(&self) {
        send(Msg::Refresh(self.0.job.clone()));
    }
}

/// What the compositor has to take pictures with.
struct Globals {
    shm: Option<wl_shm::WlShm>,
    ext: Option<ext_manager::ExtImageCopyCaptureManagerV1>,
    toplevel_sources: Option<toplevel_sources::ExtForeignToplevelImageCaptureSourceManagerV1>,
    output_sources: Option<output_sources::ExtOutputImageCaptureSourceManagerV1>,
    hyprland: Option<hypr_manager::HyprlandToplevelExportManagerV1>,
    worker: Mutex<Sender<Msg>>,
}

static GLOBALS: OnceLock<Option<Globals>> = OnceLock::new();

fn globals() -> Option<&'static Globals> {
    GLOBALS
        .get_or_init(|| {
            let link = link()?;
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name("anti-webui capture".into())
                .spawn(move || Worker::default().run(rx))
                .ok()?;
            Some(Globals {
                shm: link.bind(1, ()),
                ext: link.bind(1, ()),
                toplevel_sources: link.bind(1, ()),
                output_sources: link.bind(1, ()),
                hyprland: link.bind(2, ()),
                worker: Mutex::new(tx),
            })
        })
        .as_ref()
}

fn send(msg: Msg) {
    if let Some(g) = globals()
        && let Ok(tx) = g.worker.lock()
    {
        let _ = tx.send(msg);
    }
}

/// How a job takes its pictures.
#[derive(Debug)]
enum Way {
    /// Through a session made from this.
    Ext(source::ExtImageCaptureSourceV1),
    HyprlandWlr(wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1),
    /// The low 32 bits of the address are what Hyprland goes by.
    HyprlandAddress(u32),
}

/// Picks how to picture `target`, and starts.
fn start(target: CaptureTarget, mut job: Arc<Job>) -> Result<Arc<Job>, Arc<Job>> {
    let (Some(g), Some(link)) = (globals(), link()) else {
        return Err(job);
    };
    if g.shm.is_none() {
        return Err(job);
    }
    // A window that closed, or an output that went, leaves a dead object, and a request made
    // with one gives an inert object. Neither may be the argument of a request: wayland-backend
    // panics then, with its lock held, and every later request of the connection panics too.
    let ext = |make: &dyn Fn() -> Option<source::ExtImageCaptureSourceV1>| {
        g.ext
            .as_ref()
            .and_then(|_| make())
            .filter(|source| source.is_alive())
            .map(Way::Ext)
    };
    let by_ext = || match &target {
        CaptureTarget::Toplevel(t) => ext(&|| {
            let (m, h) = (g.toplevel_sources.as_ref()?, t.ext_handle()?);
            h.is_alive().then(|| m.create_source(h, &link.qh, ()))
        }),
        CaptureTarget::Output(o) => ext(&|| {
            let (m, out) = (g.output_sources.as_ref()?, output_object(&o.name)?);
            out.is_alive().then(|| m.create_source(&out, &link.qh, ()))
        }),
        CaptureTarget::HyprlandAddress(_) => None,
    };
    let by_hyprland = || {
        g.hyprland.as_ref()?;
        match &target {
            CaptureTarget::Toplevel(t) => {
                let h = t.wlr_handle().filter(|h| h.is_alive())?;
                (g.hyprland.as_ref()?.version() >= 2).then(|| Way::HyprlandWlr(h.clone()))
            }
            CaptureTarget::HyprlandAddress(a) => Some(Way::HyprlandAddress(*a as u32)),
            CaptureTarget::Output(_) => None,
        }
    };
    let way = if job.options.prefer == Some(CaptureSource::Hyprland) {
        by_hyprland().or_else(by_ext)
    } else {
        by_ext().or_else(by_hyprland)
    };
    let Some(way) = way else {
        return Err(job);
    };
    let j = Arc::get_mut(&mut job).expect("nothing else has it yet");
    j.source = Some(match way {
        Way::Ext(_) => CaptureSource::Ext,
        _ => CaptureSource::Hyprland,
    });
    {
        let mut st = job.state.lock().expect("no panics in here");
        // Made under the lock, so that what the session says waits until it is kept.
        if let (Way::Ext(source), Some(m)) = (&way, &g.ext) {
            let options = if job.options.cursor {
                ext_manager::Options::PaintCursors
            } else {
                ext_manager::Options::empty()
            };
            st.session = Some(m.create_session(source, options, &link.qh, job.clone()));
        }
        st.way = Some(way);
    }
    link.flush();
    send(Msg::Start(job.clone()));
    Ok(job)
}

/// A capture, as both threads see it.
#[derive(Debug)]
struct Job {
    picture: SharedSignal<Option<Arc<Bitmap>>>,
    size: SharedSignal<Option<(u32, u32)>>,
    stopped: SharedSignal<bool>,
    source: Option<CaptureSource>,
    options: CaptureOptions,
    state: Mutex<JobState>,
}

#[derive(Debug)]
enum Frame {
    Ext(ext_frame::ExtImageCopyCaptureFrameV1),
    Hyprland(hypr_frame::HyprlandToplevelExportFrameV1),
}

impl Frame {
    fn destroy(self) {
        match self {
            Frame::Ext(f) => f.destroy(),
            Frame::Hyprland(f) => f.destroy(),
        }
    }
}

/// A buffer for the compositor to write into: shm in a memfd.
#[derive(Debug)]
struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    map: *mut u8,
    layout: Layout,
}

// SAFETY: the mapping is only read on the capture thread, while the compositor is done with it.
unsafe impl Send for ShmBuffer {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl Layout {
    fn len(&self) -> usize {
        self.stride as usize * self.height as usize
    }
}

impl ShmBuffer {
    fn new(layout: Layout) -> Option<Self> {
        let (g, link) = (globals()?, link()?);
        let shm = g.shm.as_ref()?;
        let format = wl_shm::Format::try_from(layout.format).ok()?;
        let len = layout.len();
        // SAFETY: plain calls; the fd is owned from here on and the mapping freed on drop.
        let (fd, map) = unsafe {
            let fd = libc::memfd_create(c"anti-webui-capture".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), len as libc::off_t) != 0 {
                return None;
            }
            let map = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            );
            if map == libc::MAP_FAILED {
                return None;
            }
            (fd, map.cast::<u8>())
        };
        let pool = shm.create_pool(fd.as_fd(), len as i32, &link.qh, ());
        let buffer = pool.create_buffer(
            0,
            layout.width as i32,
            layout.height as i32,
            layout.stride as i32,
            format,
            &link.qh,
            (),
        );
        pool.destroy();
        let made = Self {
            buffer,
            map,
            layout,
        };
        // An inert buffer may not be the argument of `attach_buffer` or `copy` (see `start`).
        made.buffer.is_alive().then_some(made)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: mapped for `len` bytes until drop.
        unsafe { std::slice::from_raw_parts(self.map, self.layout.len()) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        // SAFETY: mapped in `new` with this length.
        unsafe { libc::munmap(self.map.cast(), self.layout.len()) };
    }
}

#[derive(Debug, Default)]
struct JobState {
    way: Option<Way>,
    session: Option<session::ExtImageCopyCaptureSessionV1>,
    /// What the session is saying, until `done`.
    told_size: Option<(u32, u32)>,
    told_formats: Vec<u32>,
    /// What it said last: the size and the format to use.
    constraints: Option<(u32, u32, u32)>,
    /// What Hyprland says the frame wants.
    hyprland_layout: Option<Layout>,
    frame: Option<Frame>,
    buffer: Option<ShmBuffer>,
    y_invert: bool,
    transform: OutputTransform,
    /// A picture has come, so the next may wait for a change.
    captured: bool,
    /// The next one shouldn't wait for a change.
    now: bool,
    /// Another is to be asked for once this one is done.
    again: bool,
    failures: u32,
    over: bool,
}

/// What the capture thread hears.
#[derive(Debug)]
enum Msg {
    Start(Arc<Job>),
    /// The session said what buffers it takes.
    Constraints(Arc<Job>),
    /// Hyprland said what buffer the frame takes.
    BufferInfo(Arc<Job>),
    Ready(Arc<Job>),
    /// It failed; for good, if the flag says so; or the buffer won't do any more.
    Failed(Arc<Job>, Failure),
    Refresh(Arc<Job>),
    Stop(Arc<Job>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Once,
    Constraints,
    Stopped,
}

/// After this many failures in a row, a job stops.
const FAILURES: u32 = 3;
/// How long to wait to try again when there is no interval.
const RETRY: Duration = Duration::from_millis(500);

/// The shm formats this reads, in the order of preference.
const FORMATS: [u32; 4] = [ARGB8888, XRGB8888, ABGR8888, XBGR8888];
const ARGB8888: u32 = 0;
const XRGB8888: u32 = 1;
const ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
const XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

#[derive(Default)]
struct Worker {
    /// When to ask jobs for another picture.
    timers: Vec<(Instant, Arc<Job>)>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let next = self.timers.iter().map(|t| t.0).min();
            let msg = match next {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers)
                .into_iter()
                .partition(|t| t.0 <= now);
            self.timers = rest;
            for (_, job) in due {
                self.begin(&job);
            }
            if let Some(link) = link() {
                link.flush();
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Start(job) => self.begin(&job),
            Msg::Constraints(job) => {
                let waiting = {
                    let mut st = lock(&job);
                    let waiting = st.again && st.frame.is_none();
                    st.again = false;
                    waiting || !st.captured
                };
                if waiting {
                    self.begin(&job);
                }
            }
            Msg::BufferInfo(job) => copy_hyprland(&job),
            Msg::Ready(job) => self.ready(&job),
            Msg::Failed(job, why) => self.failed(&job, why),
            Msg::Refresh(job) => {
                let mut st = lock(&job);
                st.now = true;
                // Hyprland's frame may be waiting for a change that won't come: start over.
                if let Some(Frame::Hyprland(_)) = &st.frame
                    && let Some(f) = st.frame.take()
                {
                    f.destroy();
                }
                drop(st);
                self.begin(&job);
            }
            Msg::Stop(job) => {
                self.timers.retain(|t| !Arc::ptr_eq(&t.1, &job));
                end(&job);
            }
        }
    }

    /// Asks for a picture, unless one is on the way.
    fn begin(&mut self, job: &Arc<Job>) {
        let Some(link) = link() else {
            return;
        };
        let mut st = lock(job);
        if st.over {
            return;
        }
        if st.frame.is_some() {
            st.again = true;
            return;
        }
        match &st.way {
            Some(Way::Ext(_)) => {
                let Some((w, h, format)) = st.constraints else {
                    // It comes when the session says what it takes.
                    st.again = true;
                    return;
                };
                let layout = Layout {
                    width: w,
                    height: h,
                    stride: w * 4,
                    format,
                };
                if !ensure_buffer(&mut st, layout) {
                    drop(st);
                    self.failed(job, Failure::Stopped);
                    return;
                }
                let (Some(session), Some(buffer)) = (&st.session, &st.buffer) else {
                    return;
                };
                let frame = session.create_frame(&link.qh, job.clone());
                frame.attach_buffer(&buffer.buffer);
                frame.damage_buffer(0, 0, w as i32, h as i32);
                frame.capture();
                st.frame = Some(Frame::Ext(frame));
            }
            Some(Way::HyprlandWlr(handle)) => {
                let Some(m) = globals().and_then(|g| g.hyprland.as_ref()) else {
                    return;
                };
                let cursor = job.options.cursor as i32;
                let frame = m.capture_toplevel_with_wlr_toplevel_handle(
                    cursor,
                    handle,
                    &link.qh,
                    job.clone(),
                );
                st.frame = Some(Frame::Hyprland(frame));
            }
            Some(Way::HyprlandAddress(address)) => {
                let Some(m) = globals().and_then(|g| g.hyprland.as_ref()) else {
                    return;
                };
                let cursor = job.options.cursor as i32;
                let frame = m.capture_toplevel(cursor, *address, &link.qh, job.clone());
                st.frame = Some(Frame::Hyprland(frame));
            }
            None => {}
        }
        st.y_invert = false;
        st.transform = OutputTransform::Normal;
    }

    fn ready(&mut self, job: &Arc<Job>) {
        let mut st = lock(job);
        if let Some(f) = st.frame.take() {
            f.destroy();
        }
        if st.over {
            return;
        }
        st.captured = true;
        st.failures = 0;
        if let Some(buffer) = &st.buffer {
            let l = buffer.layout;
            let raw = Raw {
                data: buffer.bytes(),
                width: l.width,
                height: l.height,
                stride: l.stride,
                format: l.format,
                y_invert: st.y_invert,
                transform: st.transform,
            };
            if let Some((w, h, pixels, opaque)) = convert(&raw, job.options.max_size) {
                let size = if st.transform.is_sideways() {
                    (l.height, l.width)
                } else {
                    (l.width, l.height)
                };
                if job.size.with_untracked(|s| *s != Some(size)) {
                    job.size.set(Some(size));
                }
                job.picture.set(Some(Arc::new(Bitmap::from_premultiplied(
                    w, h, pixels, opaque,
                ))));
            }
        }
        let again = std::mem::take(&mut st.again) || st.now;
        drop(st);
        if again {
            self.begin(job);
        } else if let Some(every) = job.options.every {
            self.timers.push((Instant::now() + every, job.clone()));
        }
    }

    fn failed(&mut self, job: &Arc<Job>, why: Failure) {
        let mut st = lock(job);
        if let Some(f) = st.frame.take() {
            f.destroy();
        }
        if st.over {
            return;
        }
        match why {
            // A new size or format comes, and with it `Constraints`.
            Failure::Constraints => st.again = true,
            Failure::Stopped => {
                drop(st);
                end(job);
            }
            Failure::Once => {
                st.failures += 1;
                if st.failures >= FAILURES {
                    drop(st);
                    end(job);
                    return;
                }
                drop(st);
                let wait = job.options.every.unwrap_or(RETRY);
                self.timers.push((Instant::now() + wait, job.clone()));
            }
        }
    }
}

fn lock(job: &Job) -> std::sync::MutexGuard<'_, JobState> {
    job.state.lock().expect("no panics in here")
}

/// Has a buffer of this layout ready, making one if the last was another.
fn ensure_buffer(st: &mut JobState, layout: Layout) -> bool {
    if st.buffer.as_ref().is_some_and(|b| b.layout == layout) {
        return true;
    }
    st.buffer = ShmBuffer::new(layout);
    st.buffer.is_some()
}

/// Hyprland said what the frame takes: copy into that.
fn copy_hyprland(job: &Arc<Job>) {
    let mut st = lock(job);
    let Some(layout) = st.hyprland_layout else {
        return;
    };
    if !FORMATS.contains(&layout.format) || !ensure_buffer(&mut st, layout) {
        return;
    }
    let ignore_damage = !st.captured || std::mem::take(&mut st.now);
    if let (Some(Frame::Hyprland(frame)), Some(buffer)) = (&st.frame, &st.buffer) {
        frame.copy(&buffer.buffer, ignore_damage as i32);
    }
    if let Some(link) = link() {
        link.flush();
    }
}

/// Stops for good, letting go of everything.
fn end(job: &Job) {
    let mut st = lock(job);
    if st.over {
        return;
    }
    st.over = true;
    if let Some(f) = st.frame.take() {
        f.destroy();
    }
    if let Some(s) = st.session.take() {
        s.destroy();
    }
    if let Some(Way::Ext(source)) = st.way.take() {
        source.destroy();
    }
    st.buffer = None;
    drop(st);
    job.stopped.set(true);
    if let Some(link) = link() {
        link.flush();
    }
}

/// A picture as the compositor left it.
struct Raw<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    y_invert: bool,
    transform: OutputTransform,
}

/// Makes premultiplied RGBA of it the right way up, no larger than `max`: its width, height,
/// pixels and whether all are opaque.
fn convert(raw: &Raw, max: Option<(u32, u32)>) -> Option<(u32, u32, Vec<u8>, bool)> {
    let (w, h) = (raw.width, raw.height);
    if w == 0 || h == 0 || raw.data.len() < raw.stride as usize * (h as usize - 1) + w as usize * 4
    {
        return None;
    }
    let (bgr, opaque) = match raw.format {
        ARGB8888 => (true, false),
        XRGB8888 => (true, true),
        ABGR8888 => (false, false),
        XBGR8888 => (false, true),
        _ => return None,
    };
    let sideways = raw.transform.is_sideways();
    // The size it shows at, upright, and the size to shrink to before turning it.
    let upright = if sideways { (h, w) } else { (w, h) };
    let (fw, fh) = fit(upright, max);
    let (sw, sh) = if sideways { (fh, fw) } else { (fw, fh) };

    // Area averages, in the buffer's orientation.
    let cols: Vec<(usize, usize)> = (0..sw).map(|x| span(x, sw, w)).collect();
    let mut small = vec![0u8; sw as usize * sh as usize * 4];
    for y in 0..sh {
        let (y0, y1) = span(y, sh, h);
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let mut sum = [0u32; 4];
            for row in y0..y1 {
                let row = if raw.y_invert {
                    h as usize - 1 - row
                } else {
                    row
                };
                let line = &raw.data[row * raw.stride as usize..];
                for px in line[x0 * 4..x1 * 4].chunks_exact(4) {
                    for c in 0..4 {
                        sum[c] += px[c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let at = (y as usize * sw as usize + x) * 4;
            let [a, b, c, d] = sum.map(|s| ((s + n / 2) / n) as u8);
            small[at..at + 4].copy_from_slice(&if bgr { [c, b, a, d] } else { [a, b, c, d] });
            if opaque {
                small[at + 3] = 255;
            }
        }
    }
    if raw.transform == OutputTransform::Normal {
        return Some((fw, fh, small, opaque));
    }
    let mut out = vec![0u8; fw as usize * fh as usize * 4];
    for y in 0..fh {
        for x in 0..fw {
            let (bx, by) = buffer_point(raw.transform, (x, y), (fw, fh));
            let from = (by as usize * sw as usize + bx as usize) * 4;
            let to = (y as usize * fw as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(&small[from..from + 4]);
        }
    }
    Some((fw, fh, out, opaque))
}

/// The largest size no larger than `max` with the shape of `size`, or `size` if it fits.
fn fit(size: (u32, u32), max: Option<(u32, u32)>) -> (u32, u32) {
    let Some((mw, mh)) = max else {
        return size;
    };
    let (w, h) = size;
    if w <= mw && h <= mh {
        return size;
    }
    let scale = (mw as f64 / w as f64).min(mh as f64 / h as f64);
    (
        ((w as f64 * scale).round() as u32).clamp(1, mw),
        ((h as f64 * scale).round() as u32).clamp(1, mh),
    )
}

/// The source pixels the `i`th of `n` covers, of `len`.
fn span(i: u32, n: u32, len: u32) -> (usize, usize) {
    let a = (i as u64 * len as u64 / n as u64) as usize;
    let b = ((i as u64 + 1) * len as u64 / n as u64) as usize;
    (a, b.max(a + 1).min(len as usize))
}

/// Where a point of the upright picture of `size` is in a buffer the compositor turned by
/// `transform`, as `wl_surface.set_buffer_transform` means it.
fn buffer_point(transform: OutputTransform, (x, y): (u32, u32), (w, h): (u32, u32)) -> (u32, u32) {
    use OutputTransform as T;
    let (rx, ry) = (w - 1 - x, h - 1 - y);
    match transform {
        T::Normal => (x, y),
        T::Flipped => (rx, y),
        T::Rotated90 => (ry, x),
        T::Flipped90 => (ry, rx),
        T::Rotated180 => (rx, ry),
        T::Flipped180 => (x, ry),
        T::Rotated270 => (y, rx),
        T::Flipped270 => (y, x),
    }
}

impl Dispatch<session::ExtImageCopyCaptureSessionV1, Arc<Job>> for Conn {
    fn event(
        _: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        event: session::Event,
        job: &Arc<Job>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut st = lock(job);
        match event {
            session::Event::BufferSize { width, height } => st.told_size = Some((width, height)),
            session::Event::ShmFormat { format } => st.told_formats.push(raw_format(format)),
            session::Event::Done => {
                let formats = std::mem::take(&mut st.told_formats);
                let format = FORMATS.iter().find(|f| formats.contains(f));
                match (st.told_size, format) {
                    (Some((w, h)), Some(&format)) if w > 0 && h > 0 => {
                        st.constraints = Some((w, h, format));
                        drop(st);
                        send(Msg::Constraints(job.clone()));
                    }
                    _ => {
                        drop(st);
                        send(Msg::Failed(job.clone(), Failure::Stopped));
                    }
                }
            }
            session::Event::Stopped => {
                drop(st);
                send(Msg::Failed(job.clone(), Failure::Stopped));
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_frame::ExtImageCopyCaptureFrameV1, Arc<Job>> for Conn {
    fn event(
        _: &mut Self,
        _: &ext_frame::ExtImageCopyCaptureFrameV1,
        event: ext_frame::Event,
        job: &Arc<Job>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_frame::Event::Transform {
                transform: WEnum::Value(t),
            } => lock(job).transform = OutputTransform::from_wayland(t),
            ext_frame::Event::Ready => send(Msg::Ready(job.clone())),
            ext_frame::Event::Failed { reason } => {
                let why = match reason {
                    WEnum::Value(ext_frame::FailureReason::BufferConstraints) => {
                        Failure::Constraints
                    }
                    WEnum::Value(ext_frame::FailureReason::Stopped) => Failure::Stopped,
                    _ => Failure::Once,
                };
                send(Msg::Failed(job.clone(), why));
            }
            _ => {}
        }
    }
}

impl Dispatch<hypr_frame::HyprlandToplevelExportFrameV1, Arc<Job>> for Conn {
    fn event(
        _: &mut Self,
        _: &hypr_frame::HyprlandToplevelExportFrameV1,
        event: hypr_frame::Event,
        job: &Arc<Job>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            hypr_frame::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                lock(job).hyprland_layout = Some(Layout {
                    width,
                    height,
                    stride,
                    format: raw_format(format),
                });
            }
            hypr_frame::Event::BufferDone => send(Msg::BufferInfo(job.clone())),
            hypr_frame::Event::Flags { flags } => {
                let bits = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(b) => b,
                };
                lock(job).y_invert = bits & 1 != 0;
            }
            hypr_frame::Event::Ready { .. } => send(Msg::Ready(job.clone())),
            hypr_frame::Event::Failed => send(Msg::Failed(job.clone(), Failure::Once)),
            _ => {}
        }
    }
}

fn raw_format(format: WEnum<wl_shm::Format>) -> u32 {
    match format {
        WEnum::Value(f) => f.into(),
        WEnum::Unknown(f) => f,
    }
}

wayland_client::delegate_noop!(Conn: ext_manager::ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(Conn: toplevel_sources::ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(Conn: output_sources::ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(Conn: source::ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(Conn: hypr_manager::HyprlandToplevelExportManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(data: &[u8], w: u32, h: u32, format: u32, transform: OutputTransform) -> Raw<'_> {
        Raw {
            data,
            width: w,
            height: h,
            stride: w * 4,
            format,
            y_invert: false,
            transform,
        }
    }

    #[test]
    fn colors_come_out_as_rgba() {
        // Blue, green, red, x in memory.
        let px = [10, 20, 30, 0];
        let (w, h, out, opaque) =
            convert(&raw(&px, 1, 1, XRGB8888, OutputTransform::Normal), None).unwrap();
        assert_eq!((w, h, opaque), (1, 1, true));
        assert_eq!(out, [30, 20, 10, 255]);
        let (.., out, opaque) = convert(
            &raw(&[1, 2, 3, 4], 1, 1, ABGR8888, OutputTransform::Normal),
            None,
        )
        .unwrap();
        assert_eq!((out, opaque), (vec![1, 2, 3, 4], false));
    }

    #[test]
    fn large_pictures_shrink_by_averaging() {
        // 4x2: left half black, right half white.
        let mut data = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 0 } else { 200 };
                data.extend_from_slice(&[v, v, v, 0]);
            }
        }
        let (w, h, out, _) = convert(
            &raw(&data, 4, 2, XRGB8888, OutputTransform::Normal),
            Some((2, 2)),
        )
        .unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, [0, 0, 0, 255, 200, 200, 200, 255]);
        assert_eq!(fit((1920, 1080), Some((480, 480))), (480, 270));
        assert_eq!(fit((100, 50), Some((480, 480))), (100, 50));
    }

    #[test]
    fn turned_buffers_come_out_upright() {
        // A buffer 1 wide, 2 high, of a picture 2 wide, 1 high turned by 90: the picture's
        // left is the buffer's top, as in Weston's `weston_transformed_coord`.
        let data = [1, 1, 1, 0, 2, 2, 2, 0];
        let (w, h, out, _) = convert(
            &raw(&data, 1, 2, XRGB8888, OutputTransform::Rotated90),
            None,
        )
        .unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, [1, 1, 1, 255, 2, 2, 2, 255]);
        let (.., out, _) = convert(
            &raw(&data, 1, 2, XRGB8888, OutputTransform::Rotated270),
            None,
        )
        .unwrap();
        assert_eq!(out, [2, 2, 2, 255, 1, 1, 1, 255]);
        for t in [
            OutputTransform::Rotated90,
            OutputTransform::Rotated270,
            OutputTransform::Flipped90,
            OutputTransform::Flipped180,
        ] {
            let mut seen = std::collections::HashSet::new();
            for y in 0..2 {
                for x in 0..3 {
                    seen.insert(buffer_point(t, (x, y), (3, 2)));
                }
            }
            assert_eq!(seen.len(), 6, "{t:?} sends two points to one");
        }
    }

    #[test]
    fn y_inverted_buffers_come_out_upright() {
        let data = [1, 1, 1, 0, 2, 2, 2, 0];
        let mut r = raw(&data, 1, 2, XRGB8888, OutputTransform::Normal);
        r.y_invert = true;
        let (.., out, _) = convert(&r, None).unwrap();
        assert_eq!(out, [2, 2, 2, 255, 1, 1, 1, 255]);
    }
}
