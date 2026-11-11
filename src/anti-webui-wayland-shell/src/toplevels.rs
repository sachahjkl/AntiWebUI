//! The windows of the desktop: `wlr_foreign_toplevel_management`, which can also activate and close
//! them, or `ext_foreign_toplevel_list`, which only tells of them. With both, each wlr window is
//! paired with its ext one, which is what `ext_image_copy_capture` takes.

use crate::ServiceError;
use crate::conn::{State as Conn, link};
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::task::Task;
use std::sync::{Arc, OnceLock};
use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as ext_handle, ext_foreign_toplevel_list_v1 as ext_list,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as wlr_handle, zwlr_foreign_toplevel_manager_v1 as wlr_manager,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Wlr(wlr_handle::ZwlrForeignToplevelHandleV1),
    Ext(ext_handle::ExtForeignToplevelHandleV1),
}

/// A window of any program, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct Toplevel {
    id: u64,
    pub title: String,
    pub app_id: String,
    /// What `ext_foreign_toplevel_list` calls it by, which stays the same for its life.
    pub identifier: Option<String>,
    /// The connector names of the outputs it is on.
    pub outputs: Vec<String>,
    pub maximized: bool,
    pub minimized: bool,
    /// It has the keyboard.
    pub activated: bool,
    pub fullscreen: bool,
    /// The window it belongs to, like a dialog's.
    pub parent: Option<u64>,
    target: Target,
    /// The same window in `ext_foreign_toplevel_list`, when `target` is in the other.
    ext: Option<ext_handle::ExtForeignToplevelHandleV1>,
}

impl Toplevel {
    /// The same while the window is there.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn wlr_handle(&self) -> Option<&wlr_handle::ZwlrForeignToplevelHandleV1> {
        match &self.target {
            Target::Wlr(h) => Some(h),
            Target::Ext(_) => None,
        }
    }

    pub(crate) fn ext_handle(&self) -> Option<&ext_handle::ExtForeignToplevelHandleV1> {
        match &self.target {
            Target::Ext(h) => Some(h),
            Target::Wlr(_) => self.ext.as_ref(),
        }
    }

    fn wlr(
        &self,
        ask: impl FnOnce(&wlr_handle::ZwlrForeignToplevelHandleV1),
    ) -> Task<Result<(), ServiceError>> {
        let (Target::Wlr(h), Some(link)) = (&self.target, link()) else {
            return Task::ready(Err(ServiceError::Unsupported));
        };
        ask(h);
        link.flush();
        Task::ready(Ok(()))
    }

    /// Brings it up and gives it the keyboard.
    pub fn activate(&self) -> Task<Result<(), ServiceError>> {
        let Some(seat) = link().and_then(|l| l.seat.clone()) else {
            return Task::ready(Err(ServiceError::Unavailable));
        };
        self.wlr(|h| h.activate(&seat))
    }

    /// Asks it to close, as its close button would.
    pub fn close(&self) -> Task<Result<(), ServiceError>> {
        self.wlr(|h| h.close())
    }

    pub fn set_minimized(&self, on: bool) -> Task<Result<(), ServiceError>> {
        self.wlr(|h| {
            if on {
                h.set_minimized()
            } else {
                h.unset_minimized()
            }
        })
    }

    pub fn set_maximized(&self, on: bool) -> Task<Result<(), ServiceError>> {
        self.wlr(|h| {
            if on {
                h.set_maximized()
            } else {
                h.unset_maximized()
            }
        })
    }

    pub fn set_fullscreen(&self, on: bool) -> Task<Result<(), ServiceError>> {
        self.wlr(|h| {
            if on {
                h.set_fullscreen(None)
            } else {
                h.unset_fullscreen()
            }
        })
    }
}

/// The windows of the desktop. Cheap to copy; reading tracks, on any thread.
#[derive(Clone, Copy, Debug)]
pub struct Toplevels(Option<&'static Shared>);

/// The windows of the desktop.
pub fn toplevels() -> Toplevels {
    Toplevels(shared())
}

impl Toplevels {
    pub fn available(&self) -> bool {
        self.0.is_some()
    }

    /// Whether windows can be activated, closed and the like, not only read.
    pub fn can_control(&self) -> bool {
        self.0.is_some_and(|s| s.control)
    }

    /// All of them, in the order they opened.
    pub fn all(&self) -> Vec<Toplevel> {
        self.0.map(|s| s.list.get()).unwrap_or_default()
    }

    /// The one with the keyboard.
    pub fn active(&self) -> Option<Toplevel> {
        self.all().into_iter().find(|t| t.activated)
    }
}

#[derive(Debug)]
pub(crate) struct Shared {
    list: SharedSignal<Vec<Toplevel>>,
    control: bool,
    /// An ext list bound beside a wlr manager: its windows only pair with the wlr ones. Known
    /// from the bind, not from the first wlr window: on an empty desktop the compositor may tell
    /// of a new window's ext handle first, which must not become a window of its own.
    pairs: bool,
}

static SHARED: OnceLock<Option<Arc<Shared>>> = OnceLock::new();

fn shared() -> Option<&'static Shared> {
    SHARED
        .get_or_init(|| {
            let link = link()?;
            // Handles get it through `SharedData`, before this is done.
            let list = LIST.get_or_init(|| shared_signal(Vec::new())).clone();
            let wlr = Arc::new(Shared {
                list: list.clone(),
                control: true,
                pairs: false,
            });
            if link
                .bind::<wlr_manager::ZwlrForeignToplevelManagerV1, _>(3, wlr.clone())
                .is_some()
            {
                // Only to pair with, for what takes ext windows.
                let _ = link.bind::<ext_list::ExtForeignToplevelListV1, _>(
                    1,
                    Arc::new(Shared {
                        list: shared_signal(Vec::new()),
                        control: false,
                        pairs: true,
                    }),
                );
                link.sync();
                return Some(wlr);
            }
            let ext = Arc::new(Shared {
                list,
                control: false,
                pairs: false,
            });
            link.bind::<ext_list::ExtForeignToplevelListV1, _>(1, ext.clone())?;
            link.sync();
            Some(ext)
        })
        .as_deref()
}

#[derive(Default)]
pub(crate) struct State {
    next: u64,
    /// Windows as they are being told of, until `done`.
    entries: Vec<Toplevel>,
    /// There are wlr windows, and these are ext ones to pair with them.
    shadows: Option<Vec<Shadow>>,
}

/// An ext window when the list is of wlr ones.
#[derive(Debug)]
struct Shadow {
    handle: ext_handle::ExtForeignToplevelHandleV1,
    title: String,
    app_id: String,
    identifier: Option<String>,
    done: bool,
}

impl State {
    fn add(&mut self, target: Target) {
        self.next += 1;
        self.entries.push(Toplevel {
            id: self.next,
            title: String::new(),
            app_id: String::new(),
            identifier: None,
            outputs: Vec::new(),
            maximized: false,
            minimized: false,
            activated: false,
            fullscreen: false,
            parent: None,
            target,
            ext: None,
        });
    }

    /// Pairs the ext windows nothing has yet with wlr ones: the same app and title, or else the
    /// same app, first come first.
    fn pair(&mut self) {
        let Some(shadows) = &self.shadows else {
            return;
        };
        for s in shadows.iter().filter(|s| s.done) {
            if self
                .entries
                .iter()
                .any(|t| t.ext.as_ref() == Some(&s.handle))
            {
                continue;
            }
            let free: Vec<&Toplevel> = self
                .entries
                .iter()
                .filter(|t| t.ext.is_none() && t.app_id == s.app_id)
                .collect();
            let Some(id) = free
                .iter()
                .find(|t| t.title == s.title)
                .or(free.first())
                .map(|t| t.id)
            else {
                continue;
            };
            if let Some(t) = self.entries.iter_mut().find(|t| t.id == id) {
                t.ext = Some(s.handle.clone());
                if t.identifier.is_none() {
                    t.identifier = s.identifier.clone();
                }
            }
        }
    }

    fn find(&mut self, target: &Target) -> Option<&mut Toplevel> {
        self.entries.iter_mut().find(|t| t.target == *target)
    }

    fn publish(&self, list: &SharedSignal<Vec<Toplevel>>) {
        if list.with_untracked(|l| *l != self.entries) {
            list.set(self.entries.clone());
        }
    }
}

impl Dispatch<wlr_manager::ZwlrForeignToplevelManagerV1, Arc<Shared>> for Conn {
    fn event(
        conn: &mut Self,
        _: &wlr_manager::ZwlrForeignToplevelManagerV1,
        event: wlr_manager::Event,
        _: &Arc<Shared>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr_manager::Event::Toplevel { toplevel } = event {
            conn.toplevels.shadows.get_or_insert_with(Vec::new);
            conn.toplevels.add(Target::Wlr(toplevel));
        }
    }

    event_created_child!(Conn, wlr_manager::ZwlrForeignToplevelManagerV1, [
        wlr_manager::EVT_TOPLEVEL_OPCODE => (wlr_handle::ZwlrForeignToplevelHandleV1, SHARED_DATA.clone()),
    ]);
}

/// The list windows go to, made before the manager is bound.
static LIST: OnceLock<SharedSignal<Vec<Toplevel>>> = OnceLock::new();

/// What new handles get as their user data; the generated code can't hand them the manager's.
static SHARED_DATA: SharedData = SharedData;

#[derive(Clone)]
struct SharedData;

impl SharedData {
    fn list(&self) -> Option<SharedSignal<Vec<Toplevel>>> {
        LIST.get().cloned()
    }
}

impl Dispatch<wlr_handle::ZwlrForeignToplevelHandleV1, SharedData> for Conn {
    fn event(
        conn: &mut Self,
        handle: &wlr_handle::ZwlrForeignToplevelHandleV1,
        event: wlr_handle::Event,
        data: &SharedData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let target = Target::Wlr(handle.clone());
        let output_name =
            |o: &wayland_client::protocol::wl_output::WlOutput, c: &Conn| c.output_name(o);
        match event {
            wlr_handle::Event::Title { title } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    t.title = title;
                }
            }
            wlr_handle::Event::AppId { app_id } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    t.app_id = app_id;
                }
            }
            wlr_handle::Event::OutputEnter { output } => {
                let name = output_name(&output, conn);
                if let (Some(t), Some(name)) = (conn.toplevels.find(&target), name) {
                    t.outputs.push(name);
                }
            }
            wlr_handle::Event::OutputLeave { output } => {
                let name = output_name(&output, conn);
                if let (Some(t), Some(name)) = (conn.toplevels.find(&target), name) {
                    t.outputs.retain(|o| *o != name);
                }
            }
            wlr_handle::Event::State { state } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    let has = |s: wlr_handle::State| {
                        state.chunks_exact(4).any(|c| {
                            u32::from_ne_bytes(c.try_into().expect("four bytes")) == s as u32
                        })
                    };
                    t.maximized = has(wlr_handle::State::Maximized);
                    t.minimized = has(wlr_handle::State::Minimized);
                    t.activated = has(wlr_handle::State::Activated);
                    t.fullscreen = has(wlr_handle::State::Fullscreen);
                }
            }
            wlr_handle::Event::Parent { parent } => {
                let parent =
                    parent.and_then(|p| conn.toplevels.find(&Target::Wlr(p)).map(|t| t.id));
                if let Some(t) = conn.toplevels.find(&target) {
                    t.parent = parent;
                }
            }
            wlr_handle::Event::Done => {
                conn.toplevels.pair();
                if let Some(list) = data.list() {
                    conn.toplevels.publish(&list);
                }
            }
            wlr_handle::Event::Closed => {
                conn.toplevels.entries.retain(|t| t.target != target);
                handle.destroy();
                if let Some(list) = data.list() {
                    conn.toplevels.publish(&list);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_list::ExtForeignToplevelListV1, Arc<Shared>> for Conn {
    fn event(
        conn: &mut Self,
        _: &ext_list::ExtForeignToplevelListV1,
        event: ext_list::Event,
        shared: &Arc<Shared>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_list::Event::Toplevel { toplevel } = event {
            if shared.pairs {
                conn.toplevels
                    .shadows
                    .get_or_insert_with(Vec::new)
                    .push(Shadow {
                        handle: toplevel,
                        title: String::new(),
                        app_id: String::new(),
                        identifier: None,
                        done: false,
                    });
            } else {
                conn.toplevels.add(Target::Ext(toplevel));
            }
        }
    }

    event_created_child!(Conn, ext_list::ExtForeignToplevelListV1, [
        ext_list::EVT_TOPLEVEL_OPCODE => (ext_handle::ExtForeignToplevelHandleV1, SHARED_DATA.clone()),
    ]);
}

impl Dispatch<ext_handle::ExtForeignToplevelHandleV1, SharedData> for Conn {
    fn event(
        conn: &mut Self,
        handle: &ext_handle::ExtForeignToplevelHandleV1,
        event: ext_handle::Event,
        data: &SharedData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(shadows) = &mut conn.toplevels.shadows {
            let closed = matches!(event, ext_handle::Event::Closed);
            shadow_event(shadows, handle, event);
            if closed {
                for t in &mut conn.toplevels.entries {
                    if t.ext.as_ref() == Some(handle) {
                        t.ext = None;
                    }
                }
            }
            conn.toplevels.pair();
            if let Some(list) = data.list() {
                conn.toplevels.publish(&list);
            }
            return;
        }
        let target = Target::Ext(handle.clone());
        match event {
            ext_handle::Event::Title { title } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    t.title = title;
                }
            }
            ext_handle::Event::AppId { app_id } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    t.app_id = app_id;
                }
            }
            ext_handle::Event::Identifier { identifier } => {
                if let Some(t) = conn.toplevels.find(&target) {
                    t.identifier = Some(identifier);
                }
            }
            ext_handle::Event::Done => {
                if let Some(list) = data.list() {
                    conn.toplevels.publish(&list);
                }
            }
            ext_handle::Event::Closed => {
                conn.toplevels.entries.retain(|t| t.target != target);
                handle.destroy();
                if let Some(list) = data.list() {
                    conn.toplevels.publish(&list);
                }
            }
            _ => {}
        }
    }
}

fn shadow_event(
    shadows: &mut Vec<Shadow>,
    handle: &ext_handle::ExtForeignToplevelHandleV1,
    event: ext_handle::Event,
) {
    if let ext_handle::Event::Closed = event {
        shadows.retain(|s| s.handle != *handle);
        handle.destroy();
        return;
    }
    let Some(s) = shadows.iter_mut().find(|s| s.handle == *handle) else {
        return;
    };
    match event {
        ext_handle::Event::Title { title } => s.title = title,
        ext_handle::Event::AppId { app_id } => s.app_id = app_id,
        ext_handle::Event::Identifier { identifier } => s.identifier = Some(identifier),
        ext_handle::Event::Done => s.done = true,
        _ => {}
    }
}
