//! Grabbing things and dragging them about. See `docs/design/23-drag-and-drop.md`.

use crate::event::*;
use crate::geometry::{PointF, PointI, RectI, SizeI};
use crate::paint::{DragContent, DragVisual, paint_selected_text, paint_snapshot};
use crate::platform::{CursorIcon, DragOffer, ImePurpose, PlatformRequest};
use crate::reactive::{Owner, Signal, batch, create_root, signal, untrack};
use crate::scroll::VelocityTracker;
use crate::text::edit::{EditData, EditOutcome};
use crate::text::editor::{EditorData, EditorOutcome};
use crate::text::selection::{self, node_range};
use crate::tree::{Document, Node, NodeId, NodeKind, with_doc};
use crate::ui::{Drag, Press, Ui, can_scroll, edit_point, scroll_node, set_active};
use crate::view::{IntoView, View, div, portal};
use anti_webui_style::Styled;
use std::any::{Any, TypeId};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

/// Logical pixels: what a press may move and still be a click, as for double clicks.
const THRESHOLD: f32 = 4.0;
/// Logical pixels from the edge of what scrolls where dragging scrolls it.
const EDGE: f32 = 40.0;
/// How long to stay near the edge before it scrolls: what is near the edge can be grabbed.
const EDGE_DELAY: f32 = 0.2;
/// Logical pixels per second, right at the edge.
const EDGE_SPEED: f32 = 900.0;
/// What the default preview is shown with: `kDragImageAlpha` in Blink.
const PREVIEW_OPACITY: f32 = 0.75;

const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const FILES_MIME: &str = "text/uri-list";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabPhase {
    /// It moved far enough, and a `Move` follows at once. With no threshold it was pressed.
    Start,
    Move,
    /// It was released.
    End,
    /// Escape was pressed or the window lost the focus: whatever it did should be undone. Nothing
    /// more comes until the next press.
    Cancel,
}

/// Told to what is grabbed. See [`Div::on_grab`](crate::view::Div::on_grab).
#[derive(Debug)]
pub struct GrabEvent {
    pub phase: GrabPhase,
    /// Logical pixels from where it was pressed.
    pub total: PointF,
    /// Logical pixels since the last event.
    pub delta: PointF,
    /// Logical pixels per second, going by the last moments.
    pub velocity: PointF,
    /// In window physical pixels.
    pub position: PointF,
    /// Physical pixels relative to the node's border box.
    pub local: PointF,
    /// The node's border box in window coordinates.
    pub rect: RectI,
    pub modifiers: Modifiers,
}

/// Told to what something is dragged over or dropped on.
#[derive(Debug)]
pub struct DropEvent {
    /// In window physical pixels.
    pub position: PointF,
    /// Physical pixels relative to the node's border box.
    pub local: PointF,
    /// The node's border box in window coordinates.
    pub rect: RectI,
    pub modifiers: Modifiers,
    pub action: DragAction,
}

impl DropEvent {
    /// Where in the box it is, from 0 to 1 along both axes: below 0.5 is before the middle.
    pub fn fraction(&self) -> PointF {
        PointF::new(
            self.local.x / self.rect.width.max(1) as f32,
            self.local.y / self.rect.height.max(1) as f32,
        )
    }
}

/// What is being dragged when it comes from another program: all that is known before the drop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalDrag {
    pub mimes: Vec<String>,
}

impl ExternalDrag {
    pub fn has_files(&self) -> bool {
        self.mimes.iter().any(|m| m == FILES_MIME)
    }

    pub fn has_text(&self) -> bool {
        self.mimes.iter().any(|m| TEXT_MIMES.contains(&m.as_str()))
    }
}

/// What selected text is dragged as: all of it, whatever of it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedText(pub String);

/// Where selected text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextOrigin {
    Document,
    Edit(NodeId),
    /// A `text_editor`.
    Editor(NodeId),
}

/// Whether `p` is on selected text in `target`, which can be dragged. Goes by what is highlighted.
pub(crate) fn pressed_selection(doc: &Document, target: NodeId, p: PointF) -> Option<TextOrigin> {
    let n = doc.get(target).filter(|n| !n.is_disabled())?;
    match &n.kind {
        NodeKind::Text(t) if t.row.is_some() => {
            // A line of a `text_editor`, with what of the editor's selection is on it.
            let row = t.row.as_ref()?;
            let r = row.selected.clone()?;
            let e = doc.get(row.host)?.editor()?;
            if e.preedit.is_some() {
                return None;
            }
            let q = doc.from_window(n, p)?;
            let end = r.end.min(t.text.len().saturating_sub(1)).max(r.start);
            t.layout
                .range_rects(r.start, end, n.abs.x, n.abs.y)
                .iter()
                .any(|r| r.contains(q))
                .then_some(TextOrigin::Editor(row.host))
        }
        NodeKind::Text(t) => {
            let (a, b) = node_range(doc, doc.state.selection.as_ref()?, target)?;
            let q = doc.from_window(n, p)?;
            t.layout
                .range_rects(a, b, n.abs.x, n.abs.y)
                .iter()
                .any(|r| r.contains(q))
                .then_some(TextOrigin::Document)
        }
        NodeKind::Edit(e) => {
            if !e.has_selection() || e.preedit.is_some() || e.purpose == ImePurpose::Password {
                return None;
            }
            let (a, b) = e.selection_range();
            let q = edit_point(doc, target, p);
            e.layout
                .range_rects(a, b, 0, 0)
                .iter()
                .any(|r| r.contains(q))
                .then_some(TextOrigin::Edit(target))
        }
        _ => None,
    }
}

/// Where in the input something dropped at `p` would go.
fn drop_offset(doc: &Document, id: NodeId, p: PointF) -> Option<usize> {
    if doc.get(id)?.is_editor() {
        return crate::text::editor::hit(doc, id, p);
    }
    let at = edit_point(doc, id, p);
    Some(doc.get(id)?.edit()?.hit(at.x, at.y).0)
}

/// The selection of an input or a `text_editor` that can be written in, and its text.
fn writable_selection(n: &Node) -> Option<((usize, usize), &str)> {
    match (n.edit(), n.editor()) {
        (Some(e), _) if !e.read_only => Some((e.selection_range(), e.selected_text())),
        (_, Some(e)) if !e.read_only => Some((e.selection_range(), e.selected_text())),
        _ => None,
    }
}

/// What became of an input or a `text_editor` that was changed.
enum Changed {
    Edit(EditOutcome),
    Editor(EditorOutcome),
}

impl Changed {
    fn changed(&self) -> bool {
        match self {
            Changed::Edit(o) => o.changed,
            Changed::Editor(o) => o.changed,
        }
    }
}

/// Changes an input with `edit`, or a `text_editor` with `editor`.
fn change(
    d: &mut Document,
    id: NodeId,
    edit: &dyn Fn(&mut EditData) -> EditOutcome,
    editor: &dyn Fn(&mut EditorData) -> EditorOutcome,
) -> Option<Changed> {
    let n = d.get_mut(id)?;
    let out = match (n.is_edit(), n.is_editor()) {
        (true, _) => Changed::Edit(edit(n.edit_mut()?)),
        (_, true) => Changed::Editor(editor(n.editor_mut()?)),
        _ => return None,
    };
    if let Changed::Edit(o) = &out
        && o.changed
    {
        d.invalidate_layout(id);
    }
    if let Changed::Editor(o) = &out
        && o.changed
    {
        d.mark_caret_moved();
    }
    Some(out)
}

/// The format an input takes from this, if it takes any.
fn text_mime(mimes: &[String]) -> Option<String> {
    TEXT_MIMES
        .iter()
        .find(|t| mimes.iter().any(|m| m == *t))
        .map(|t| (*t).to_owned())
}

/// What a [`draggable`](crate::view::Div::draggable) carries: a value for this program, and with
/// [`text`](Self::text), [`files`](Self::files) or [`data`](Self::data) what other programs get.
pub struct DragData {
    value: Rc<dyn Any>,
    type_id: TypeId,
    items: Vec<(String, Arc<[u8]>)>,
    actions: Option<DragActions>,
    files: bool,
}

impl Default for DragData {
    fn default() -> Self {
        Self::new(())
    }
}

impl DragData {
    pub fn new<T: 'static>(value: T) -> Self {
        Self {
            value: Rc::new(value),
            type_id: TypeId::of::<T>(),
            items: Vec::new(),
            actions: None,
            files: false,
        }
    }

    /// What [`draggable`](crate::view::Div::draggable)'s closure returned.
    pub(crate) fn from_any<T: 'static>(value: T) -> Self {
        let mut slot = Some(value);
        if let Some(data) = (&mut slot as &mut dyn Any).downcast_mut::<Option<DragData>>() {
            return data.take().expect("just set");
        }
        Self::new(slot.take().expect("just set"))
    }

    /// The value it carries, if that is a `T`.
    pub fn value<T: 'static>(&self) -> Option<&T> {
        (*self.value).downcast_ref()
    }

    /// Offers it as text.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        let bytes: Arc<[u8]> = Arc::from(text.into().into_bytes());
        for mime in TEXT_MIMES {
            self.items.push((mime.to_owned(), bytes.clone()));
        }
        self
    }

    /// Offers it as files. Unless [`actions`](Self::actions) says otherwise they can only be
    /// copied: a file manager that moves them would make the originals go away.
    pub fn files<P: AsRef<Path>>(mut self, files: impl IntoIterator<Item = P>) -> Self {
        let list = uri_list(files);
        self.items
            .push((FILES_MIME.to_owned(), Arc::from(list.into_bytes())));
        self.files = true;
        self
    }

    /// Offers it in a format of your own.
    pub fn data(mut self, mime: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        self.items.push((mime.into(), bytes.into()));
        self
    }

    /// What may be done with it.
    pub fn actions(mut self, actions: DragActions) -> Self {
        self.actions = Some(actions);
        self
    }

    fn allowed(&self) -> DragActions {
        self.actions.unwrap_or(if self.files {
            DragActions::Copy
        } else {
            DragActions::Both
        })
    }
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// `text/uri-list` for these files.
fn uri_list<P: AsRef<Path>>(files: impl IntoIterator<Item = P>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for path in files {
        let path = std::path::absolute(path.as_ref()).unwrap_or_else(|_| path.as_ref().into());
        out.push_str("file://");
        // `C:\a\b` is `file:///C:/a/b`.
        #[cfg(windows)]
        let path = format!("/{}", path.to_string_lossy().replace('\\', "/"));
        #[cfg(windows)]
        let bytes = path.as_bytes();
        #[cfg(not(windows))]
        let bytes = path.as_os_str().as_encoded_bytes();
        for &b in bytes {
            // The colon of a drive stays as it is: `file:///C:/a`.
            if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) || (cfg!(windows) && b == b':') {
                out.push(b as char);
            } else {
                let _ = write!(out, "%{b:02X}");
            }
        }
        out.push_str("\r\n");
    }
    out
}

/// The local files of a `text/uri-list`. What isn't one isn't there.
fn files_of_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("file://")?;
            // The host is empty or this computer.
            let path = &rest[rest.find('/')?..];
            let raw = path.as_bytes();
            let mut out = Vec::with_capacity(raw.len());
            let mut i = 0;
            while i < raw.len() {
                let code = (raw[i] == b'%' && i + 2 < raw.len())
                    .then(|| Some(hex(raw[i + 1])? * 16 + hex(raw[i + 2])?))
                    .flatten();
                match code {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(raw[i]);
                        i += 1;
                    }
                }
            }
            Some(path_of_bytes(out))
        })
        .collect()
}

#[cfg(unix)]
fn path_of_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// `/C:/a/b` is `C:\a\b`, and `//server/share/a` stays a UNC path.
#[cfg(not(unix))]
fn path_of_bytes(bytes: Vec<u8>) -> PathBuf {
    let path = String::from_utf8_lossy(&bytes);
    let drive = path.as_bytes().get(2) == Some(&b':');
    let path = if drive { &path[1..] } else { &path[..] };
    PathBuf::from(path.replace('/', "\\"))
}

type GrabHandler = Rc<dyn Fn(&GrabEvent)>;
type MakeData = Rc<dyn Fn() -> DragData>;
type MakePreview = Rc<dyn Fn(&DragData) -> Option<View>>;

/// What a node does when it is moved with the button held.
#[derive(Default)]
pub(crate) struct GestureConfig {
    pub grab: Option<GrabHandler>,
    pub threshold: Option<f32>,
    pub autoscroll: bool,
    pub data: Option<MakeData>,
    pub preview: Option<MakePreview>,
    pub end: Option<Rc<dyn Fn(DragResult)>>,
}

impl GestureConfig {
    pub fn is_empty(&self) -> bool {
        self.grab.is_none() && self.data.is_none()
    }
}

type TypedHandler = Rc<dyn Fn(&dyn Any, &DropEvent)>;
type TypedFilter = Rc<dyn Fn(&dyn Any) -> bool>;
type BytesHandler = Rc<dyn Fn(&[u8], &DropEvent)>;

/// What a node does with what is dragged onto it.
#[derive(Default)]
pub(crate) struct DropConfig {
    pub drops: Vec<(TypeId, TypedHandler)>,
    pub filters: Vec<(TypeId, TypedFilter)>,
    pub over: Vec<(TypeId, TypedHandler)>,
    pub mimes: Vec<(Vec<String>, BytesHandler)>,
    pub hover: Option<Rc<dyn Fn(bool)>>,
}

impl DropConfig {
    pub fn is_empty(&self) -> bool {
        self.drops.is_empty() && self.mimes.is_empty()
    }

    pub fn typed_drop<T: 'static>(&mut self, f: impl Fn(&T, &DropEvent) + 'static) {
        self.drops.push((
            TypeId::of::<T>(),
            Rc::new(move |v, e| {
                if let Some(v) = v.downcast_ref() {
                    f(v, e);
                }
            }),
        ));
    }

    pub fn typed_over<T: 'static>(&mut self, f: impl Fn(&T, &DropEvent) + 'static) {
        self.over.push((
            TypeId::of::<T>(),
            Rc::new(move |v, e| {
                if let Some(v) = v.downcast_ref() {
                    f(v, e);
                }
            }),
        ));
    }

    pub fn typed_filter<T: 'static>(&mut self, f: impl Fn(&T) -> bool + 'static) {
        self.filters.push((
            TypeId::of::<T>(),
            Rc::new(move |v| v.downcast_ref().is_some_and(&f)),
        ));
    }

    pub fn files(&mut self, f: impl Fn(&[PathBuf], &DropEvent) + 'static) {
        self.mimes.push((
            vec![FILES_MIME.to_owned()],
            Rc::new(move |bytes, e| f(&files_of_uri_list(bytes), e)),
        ));
    }

    pub fn text(&mut self, f: impl Fn(&str, &DropEvent) + 'static) {
        self.mimes.push((
            TEXT_MIMES.map(str::to_owned).to_vec(),
            Rc::new(move |bytes, e| f(&String::from_utf8_lossy(bytes).replace("\r\n", "\n"), e)),
        ));
    }

    pub fn data(&mut self, mime: &str, f: impl Fn(&[u8], &DropEvent) + 'static) {
        self.mimes.push((vec![mime.to_owned()], Rc::new(f)));
    }
}

/// A press on something that can be grabbed or dragged.
pub(crate) struct Gesture {
    node: NodeId,
    start: PointF,
    last: PointF,
    /// Physical pixels.
    threshold: f32,
    started: bool,
    cancelled: bool,
    /// It went further than a click may.
    moved: bool,
    drags: bool,
    autoscroll: bool,
    velocity: VelocityTracker,
    began: Instant,
    shows_grabbing: bool,
}

impl Gesture {
    pub fn eats_click(&self) -> bool {
        self.started && (self.moved || self.cancelled)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// In the window, with the pointer events.
    Inside,
    /// Handed to the platform.
    Platform,
    /// Coming from another program.
    External,
}

#[derive(Clone, PartialEq, Eq)]
enum Accept {
    /// With its value.
    Value,
    /// In this format.
    Mime(String),
    /// As text put in an input, in this format.
    Edit(String),
}

/// Something being dragged.
pub(crate) struct DragSession {
    source: Option<NodeId>,
    /// The input selected text was taken from. Moving it to an input of this window takes it out.
    text_source: Option<NodeId>,
    value: Rc<dyn Any>,
    type_id: TypeId,
    items: Vec<(String, Arc<[u8]>)>,
    mimes: Vec<String>,
    allowed: DragActions,
    mode: Mode,
    /// What it would go to.
    over: Option<(NodeId, Accept)>,
    /// The last thing the platform was told about it.
    told: Option<(String, DragAction)>,
    position: PointF,
    preview: Option<Owner>,
    /// External data is on its way to here.
    awaiting: Option<(NodeId, Accept, DragAction, PointF)>,
    /// It has been dropped and the platform hasn't said it ended.
    dropped: bool,
}

/// Scrolling with something held near the edge.
pub(crate) struct EdgeScroll {
    node: NodeId,
    speed: (f32, f32),
    since: Instant,
    last: Instant,
    rest: (f32, f32),
}

type DraggedValue = Option<Rc<dyn Any>>;

thread_local! {
    static DRAGGED: Cell<Option<Signal<DraggedValue>>> = const { Cell::new(None) };
    /// The drag a window of this thread has handed to the platform, while it runs: its value, its
    /// type and its formats. Another window of the thread it enters takes it as the value it is,
    /// not as another program's data: a shell's panel on a surface of its own drops on its bar.
    static OUTGOING: std::cell::RefCell<Option<(Rc<dyn Any>, TypeId, Vec<String>)>> =
        const { std::cell::RefCell::new(None) };
}

fn dragged_signal() -> Signal<DraggedValue> {
    DRAGGED.with(|s| {
        s.get().unwrap_or_else(|| {
            // Under a root of its own: it outlives whatever asks for it first.
            let (signal, _) = create_root(|| signal(None));
            s.set(Some(signal));
            signal
        })
    })
}

/// What is being dragged, if it is a `T`. Reading it tracks it. It's an [`ExternalDrag`] when it
/// comes from another program.
pub fn dragged<T: Clone + 'static>() -> Option<T> {
    dragged_signal().with(|v| v.as_ref().and_then(|v| (**v).downcast_ref::<T>().cloned()))
}

fn action_for(allowed: DragActions, modifiers: Modifiers) -> DragAction {
    match allowed {
        DragActions::Move => DragAction::Move,
        DragActions::Copy => DragAction::Copy,
        DragActions::Both if modifiers.copies_drag() => DragAction::Copy,
        DragActions::Both => DragAction::Move,
    }
}

fn drop_event(
    position: PointF,
    rect: RectI,
    modifiers: Modifiers,
    action: DragAction,
) -> DropEvent {
    DropEvent {
        position,
        local: PointF::new(position.x - rect.x as f32, position.y - rect.y as f32),
        rect,
        modifiers,
        action,
    }
}

/// Whether the node takes what is dragged, and how.
fn accepts(doc: &Document, id: NodeId, session: &DragSession) -> Option<(Rc<DropConfig>, bool)> {
    let n = doc.get(id).filter(|n| !n.is_disabled())?;
    let drop = n.listeners.drop.clone()?;
    let typed = drop.drops.iter().any(|(t, _)| *t == session.type_id);
    Some((drop, typed))
}

/// Whether an input can be given this as text. `None` says it doesn't come into it, and an inner
/// `None` that it does, but can't be dropped there.
fn edit_accepts(doc: &Document, id: NodeId, session: &DragSession) -> Option<Option<Accept>> {
    let n = doc.get(id).filter(|n| !n.is_disabled())?;
    let ((a, b), _) = writable_selection(n)?;
    let mime = text_mime(&session.mimes)?;
    if session.text_source == Some(id) {
        let at = drop_offset(doc, id, session.position)?;
        if (a..=b).contains(&at) {
            return Some(None);
        }
    }
    Some(Some(Accept::Edit(mime)))
}

fn acceptance(drop: &DropConfig, typed: bool, session: &DragSession) -> Option<Accept> {
    if typed
        && drop
            .filters
            .iter()
            .filter(|(t, _)| *t == session.type_id)
            .all(|(_, f)| untrack(|| f(&*session.value)))
    {
        return Some(Accept::Value);
    }
    drop.mimes.iter().find_map(|(mimes, _)| {
        mimes
            .iter()
            .find(|m| session.mimes.contains(m))
            .map(|m| Accept::Mime(m.clone()))
    })
}

/// What [`start_drag`] asked for.
pub(crate) struct DragRequest {
    data: DragData,
    grip: (f32, f32),
    preview: Box<dyn FnOnce() -> View>,
}

/// Starts dragging right away, for what decides by itself when that is, like a viewer with a text
/// selection of its own. Everything else is better off with
/// [`Div::draggable`](crate::view::Div::draggable), which also knows when a press has moved far
/// enough. It takes the button still being held, and does nothing otherwise.
///
/// `preview` makes what shows with the pointer, which it runs outside of whatever made it. `grip`
/// says where in that the pointer is, from 0 to 1 across and down.
pub fn start_drag<V: IntoView>(
    data: DragData,
    grip: (f32, f32),
    preview: impl FnOnce() -> V + 'static,
) {
    with_doc(|d| {
        d.drag_request = Some(DragRequest {
            data,
            grip,
            preview: Box::new(move || preview().into_view()),
        });
        d.mark_paint();
    });
}

/// Puts what shows for something being dragged in the document, in the style of what it came from.
fn mount_preview(
    inherit: &anti_webui_style::ComputedStyle,
    make: impl FnOnce() -> Option<View>,
) -> Option<(View, Owner)> {
    let (view, owner) = create_root(|| {
        let inner = untrack(make)?;
        let mut outer = portal().child(
            div()
                .absolute()
                .left(0.0)
                .top(0.0)
                .pointer_events_none()
                .child(inner),
        );
        outer.data_mut().preview_of = Some(Box::new(inherit.clone()));
        Some(outer.into_view())
    });
    let Some(view) = view else {
        owner.dispose();
        return None;
    };
    with_doc(|d| {
        if let Some(root) = d.root {
            let mut children = d.nodes[root].children.clone();
            children.push(view.0);
            d.set_children(root, children);
        }
    });
    Some((view, owner))
}

impl Ui {
    /// Says whether the platform can hand drags to the system. Without it, drags stay in the
    /// window whatever they carry.
    pub fn set_platform_drag(&mut self, supported: bool) {
        crate::window::activate(self.window);
        self.platform_drag = supported;
    }

    /// What in `target`'s chain is grabbed by pressing it.
    pub(crate) fn find_gesture(&self, target: NodeId, p: PointF) -> Option<Gesture> {
        with_doc(|d| {
            let (node, config) = d.ancestors(target).into_iter().find_map(|id| {
                let n = d.get(id).filter(|n| !n.is_disabled())?;
                Some((id, n.listeners.gesture.clone()?))
            })?;
            let threshold = d.scale.pxf(config.threshold.unwrap_or(THRESHOLD));
            let shows_grabbing = crate::ui::cursor_for(d, Some(target)) == CursorIcon::Grab;
            Some(Gesture {
                node,
                start: p,
                last: p,
                threshold,
                started: false,
                cancelled: false,
                moved: false,
                drags: config.data.is_some(),
                autoscroll: config.autoscroll,
                velocity: VelocityTracker::default(),
                began: Instant::now(),
                shows_grabbing,
            })
        })
    }

    pub(crate) fn set_cursor(&mut self, cursor: CursorIcon) {
        if self.cursor != Some(cursor) {
            self.cursor = Some(cursor);
            with_doc(|d| d.request(PlatformRequest::SetCursor(cursor)));
        }
    }

    /// The press found something to grab.
    pub(crate) fn gesture_pressed(&mut self, p: PointF) {
        let Some(g) = self.press.as_mut().and_then(|p| p.gesture.as_mut()) else {
            return;
        };
        let (immediate, grabbing) = (!g.drags && g.threshold <= 0.0, g.shows_grabbing);
        if grabbing {
            self.set_cursor(CursorIcon::Grabbing);
        }
        if immediate {
            self.gesture_moved(p);
        }
    }

    /// Tells what is grabbed, if anything.
    fn tell_grab(&mut self, phase: GrabPhase, p: PointF) {
        let modifiers = self.modifiers;
        if let Some(g) = self.press.as_mut().and_then(|p| p.gesture.as_mut()) {
            tell_grab(g, modifiers, phase, p);
        }
    }

    /// The pointer moved while something that can be grabbed was pressed.
    pub(crate) fn gesture_moved(&mut self, p: PointF) {
        let Some(g) = self.press.as_mut().and_then(|p| p.gesture.as_mut()) else {
            return;
        };
        if g.cancelled {
            return;
        }
        let far = (p.x - g.start.x).hypot(p.y - g.start.y);
        let slop = with_doc(|d| d.scale.pxf(THRESHOLD));
        g.moved |= far >= slop;
        if !g.started {
            if far < g.threshold {
                return;
            }
            if g.drags {
                self.start_drag(p);
                return;
            }
            g.started = true;
            let start = g.start;
            self.tell_grab(GrabPhase::Start, start);
        }
        if far > 0.0 {
            self.tell_grab(GrabPhase::Move, p);
        }
        self.update_edge(p, false);
    }

    /// The press ended.
    pub(crate) fn gesture_released(&mut self, press: &mut Press, p: PointF) {
        self.edge = None;
        if let Some(g) = press.gesture.as_mut().filter(|g| g.started && !g.cancelled) {
            tell_grab(g, self.modifiers, GrabPhase::End, p);
        }
    }

    /// The window lost the focus. What the platform carries doesn't depend on that.
    pub(crate) fn cancel_gesture_for_focus(&mut self) {
        if self.drag.as_ref().is_none_or(|s| s.mode == Mode::Inside) {
            self.cancel_gesture();
        }
    }

    /// Calls off what is being grabbed or dragged, if anything. Returns whether something was.
    pub(crate) fn cancel_gesture(&mut self) -> bool {
        if self
            .drag
            .as_ref()
            .is_some_and(|s| matches!(s.mode, Mode::Inside))
        {
            self.end_drag(None);
            return true;
        }
        if self
            .drag
            .as_ref()
            .is_some_and(|s| matches!(s.mode, Mode::Platform) && !s.dropped)
        {
            // Destroying its source is how it ends; the platform says so.
            with_doc(|d| d.request(PlatformRequest::CancelDrag));
            return true;
        }
        let p = self.pointer.unwrap_or_default();
        let Some(g) = self.press.as_mut().and_then(|p| p.gesture.as_mut()) else {
            return false;
        };
        if !g.started || g.cancelled {
            return false;
        }
        g.cancelled = true;
        self.tell_grab(GrabPhase::Cancel, p);
        self.edge = None;
        true
    }

    /// Something was moved far enough to start dragging it.
    fn start_drag(&mut self, p: PointF) {
        let Some(mut press) = self.press.take() else {
            return;
        };
        let Some(g) = press.gesture.take() else {
            return;
        };
        let node = g.node;
        let found = with_doc(|d| {
            let config = d.get(node)?.listeners.gesture.clone()?;
            Some((config, d.window_rect(node)?, d.get(node)?.computed.clone()))
        });
        let Some((config, rect, inherit)) = found else {
            return;
        };
        let Some(make) = config.data.clone() else {
            return;
        };
        let data = untrack(|| make());
        let items = data.items.clone();
        let wants_platform = self.platform_drag && !items.is_empty();
        let grip = (
            ((g.start.x - rect.x as f32) / rect.width.max(1) as f32).clamp(0.0, 1.0),
            ((g.start.y - rect.y as f32) / rect.height.max(1) as f32).clamp(0.0, 1.0),
        );
        // Painted before it starts looking like what is being dragged.
        let now = Instant::now();
        let built = config
            .preview
            .as_ref()
            .and_then(|f| mount_preview(&inherit, || f(&data)));
        let mut preview = None;
        let visual = match built {
            Some((view, owner)) => {
                preview = Some(owner);
                DragVisual {
                    content: DragContent::Live(view.0),
                    position: whole(p),
                    grip,
                    opacity: 1.0,
                    icon: wants_platform,
                    painted: None,
                }
            }
            None => {
                let painted = with_doc(|d| paint_snapshot(d, node, now));
                DragVisual {
                    content: DragContent::Snapshot,
                    position: whole(p),
                    grip,
                    opacity: PREVIEW_OPACITY,
                    icon: wants_platform,
                    painted,
                }
            }
        };
        self.begin_drag(Some(node), None, data, visual, preview, p);
    }

    /// Does what [`start_drag`] asked for, if the button is still being held.
    pub(crate) fn start_requested_drag(&mut self) {
        let Some(request) = with_doc(|d| d.drag_request.take()) else {
            return;
        };
        let Some(press) = self.press.take() else {
            return;
        };
        let inherit = with_doc(|d| {
            press
                .target
                .or(d.root)
                .and_then(|t| d.get(t))
                .map(|n| n.computed.clone())
        });
        let Some(inherit) = inherit else {
            return;
        };
        let p = self.pointer.unwrap_or_default();
        let DragRequest {
            data,
            grip,
            preview,
        } = request;
        let wants_platform = self.platform_drag && !data.items.is_empty();
        let Some((view, owner)) = mount_preview(&inherit, || Some(preview())) else {
            return;
        };
        let visual = DragVisual {
            content: DragContent::Live(view.0),
            position: whole(p),
            grip,
            opacity: 1.0,
            icon: wants_platform,
            painted: None,
        };
        self.edge = None;
        self.begin_drag(None, None, data, visual, Some(owner), p);
    }

    /// The pointer moved with selected text pressed. Returns whether it is being dragged now.
    pub(crate) fn text_drag_moved(&mut self, origin: TextOrigin, start: PointF, p: PointF) -> bool {
        // Per axis, like `DragThresholdExceeded` in Blink.
        let slop = with_doc(|d| d.scale.pxf(THRESHOLD));
        if (p.x - start.x).abs() < slop && (p.y - start.y).abs() < slop {
            return false;
        }
        let edit = match origin {
            TextOrigin::Edit(id) | TextOrigin::Editor(id) => Some(id),
            TextOrigin::Document => None,
        };
        let found = with_doc(|d| {
            let (text, moves) = match origin {
                TextOrigin::Edit(id) => {
                    let e = d.get(id)?.edit()?;
                    (e.selected_text().to_owned(), !e.read_only)
                }
                TextOrigin::Editor(id) => {
                    let e = d.get(id)?.editor()?;
                    (e.selected_text().to_owned(), !e.read_only)
                }
                TextOrigin::Document => (
                    selection::selected_text(d, d.state.selection.as_ref()?),
                    false,
                ),
            };
            (!text.is_empty()).then_some((text, moves))
        });
        let Some((text, moves)) = found else {
            // What was selected has gone, so this is nothing anymore.
            if let Some(press) = &mut self.press {
                press.drag = Drag::None;
            }
            return false;
        };
        let data = DragData::new(DraggedText(text.clone()))
            .text(text)
            .actions(if moves {
                DragActions::Both
            } else {
                DragActions::Copy
            });
        self.ensure_layout();
        let painted = with_doc(|d| paint_selected_text(d, origin));
        let grip = painted.as_ref().map_or((0.0, 0.0), |(_, _, b)| {
            (
                ((start.x - b.x as f32) / b.width.max(1) as f32).clamp(0.0, 1.0),
                ((start.y - b.y as f32) / b.height.max(1) as f32).clamp(0.0, 1.0),
            )
        });
        let visual = DragVisual {
            content: DragContent::Snapshot,
            position: whole(p),
            grip,
            opacity: PREVIEW_OPACITY,
            icon: self.platform_drag,
            painted,
        };
        self.press = None;
        self.begin_drag(None, edit, data, visual, None, p);
        true
    }

    /// Something starts being dragged.
    fn begin_drag(
        &mut self,
        source: Option<NodeId>,
        text_source: Option<NodeId>,
        data: DragData,
        visual: DragVisual,
        preview: Option<Owner>,
        p: PointF,
    ) {
        let items = data.items.clone();
        let wants_platform = self.platform_drag && !items.is_empty();
        let value = data.value.clone();
        let mimes: Vec<String> = items.iter().map(|(m, _)| m.clone()).collect();
        let allowed = data.allowed();
        if wants_platform {
            OUTGOING.with(|o| *o.borrow_mut() = Some((value.clone(), data.type_id, mimes.clone())));
        }
        self.drag = Some(DragSession {
            source,
            text_source,
            value: value.clone(),
            type_id: data.type_id,
            items: items.clone(),
            mimes,
            allowed,
            mode: if wants_platform {
                Mode::Platform
            } else {
                Mode::Inside
            },
            over: None,
            told: None,
            position: p,
            preview,
            awaiting: None,
            dropped: false,
        });
        with_doc(|d| {
            d.drag = Some(visual);
            d.state.dragging = source;
            if let Some(node) = source
                && d.get(node).is_some_and(|n| n.style.dragging.is_some())
            {
                d.mark_style_of(node);
            }
            d.mark_paint();
            if wants_platform {
                d.request(PlatformRequest::StartDrag(DragOffer {
                    items,
                    actions: allowed,
                }));
            }
        });
        set_active(Vec::new());
        self.set_hovered(Vec::new());
        batch(|| dragged_signal().set(Some(value)));
        if !wants_platform {
            self.ensure_layout();
            self.retarget(true);
            self.update_edge(p, true);
        }
    }

    /// The pointer moved. Returns whether something being dragged is all that matters.
    pub(crate) fn drag_moved(&mut self, p: PointF) -> bool {
        let Some(session) = self.drag.as_mut() else {
            return false;
        };
        if session.mode != Mode::Inside {
            return true;
        }
        session.position = p;
        with_doc(|d| {
            if let Some(v) = &mut d.drag {
                v.position = whole(p);
                d.mark_paint();
            }
        });
        self.ensure_layout();
        self.retarget(true);
        self.update_edge(p, true);
        true
    }

    /// A press comes with something dragged inside of the window: the release of that never came.
    pub(crate) fn drop_stale_drag(&mut self) {
        if self.drag.as_ref().is_some_and(|s| s.mode == Mode::Inside) {
            self.end_drag(None);
        }
    }

    /// The button was released with something dragged inside of the window.
    pub(crate) fn drag_released(&mut self, p: PointF) -> bool {
        if !self.drag.as_ref().is_some_and(|s| s.mode == Mode::Inside) {
            return false;
        }
        self.drag_moved(p);
        let action = self.deliver_own_drop(p, None);
        self.end_drag(action);
        true
    }

    /// Calls what it is over, with what is carried here. Returns what was done, if anything.
    fn deliver_own_drop(&mut self, p: PointF, action: Option<DragAction>) -> Option<DragAction> {
        let session = self.drag.as_ref()?;
        let (node, accept) = session.over.clone()?;
        let action = action.unwrap_or_else(|| action_for(session.allowed, self.modifiers));
        let (value, type_id) = (session.value.clone(), session.type_id);
        let bytes = match &accept {
            Accept::Mime(mime) | Accept::Edit(mime) => session
                .items
                .iter()
                .find(|(m, _)| m == mime)
                .map(|(_, b)| b.clone()),
            Accept::Value => None,
        };
        if matches!(accept, Accept::Edit(_)) {
            let source = session.text_source;
            self.drop_into_edit(node, &bytes?, p, action, source);
            return Some(action);
        }
        let modifiers = self.modifiers;
        let found = with_doc(|d| {
            let n = d.get(node)?;
            Some((n.listeners.drop.clone()?, d.window_rect(node)?))
        });
        let (drop, rect) = found?;
        let event = drop_event(p, rect, modifiers, action);
        batch(|| match &accept {
            Accept::Value => {
                for (_, f) in drop.drops.iter().filter(|(t, _)| *t == type_id) {
                    f(&*value, &event);
                }
            }
            Accept::Mime(mime) => {
                if let (Some(bytes), Some((_, f))) =
                    (&bytes, drop.mimes.iter().find(|(m, _)| m.contains(mime)))
                {
                    f(bytes, &event);
                }
            }
            Accept::Edit(_) => {}
        });
        Some(action)
    }

    /// Text was dropped on an input, like `ConcludeEditDrag` in Blink: it ends up selected there.
    /// `source` is the input it came from, if any.
    fn drop_into_edit(
        &mut self,
        id: NodeId,
        bytes: &[u8],
        p: PointF,
        action: DragAction,
        source: Option<NodeId>,
    ) {
        let text = String::from_utf8_lossy(bytes);
        // Only if it's still what was taken: otherwise there is nothing to take out.
        let source = source.filter(|s| {
            action == DragAction::Move
                && with_doc(|d| {
                    d.get(*s)
                        .and_then(writable_selection)
                        .is_some_and(|(_, t)| t == text)
                })
        });
        let (taken, put) = with_doc(|d| {
            let at = drop_offset(d, id, p).unwrap_or_default();
            let (taken, put) = if source == Some(id) {
                let put = change(d, id, &|e| e.move_selection(at), &|e| e.move_selection(at));
                (None, put)
            } else {
                let put = change(d, id, &|e| e.drop_text(at, &text), &|e| {
                    e.drop_text(at, &text)
                });
                let taken = source
                    .filter(|_| put.as_ref().is_some_and(Changed::changed))
                    .and_then(|s| {
                        change(d, s, &|e| e.delete(false, false), &|e| {
                            e.delete(false, false)
                        })
                        .map(|o| (s, o))
                    });
                (taken, put)
            };
            if d.state.selection.take().is_some() {
                d.mark_paint();
            }
            (taken, put)
        });
        self.set_focus(Some(id));
        for (n, outcome) in taken.into_iter().chain(put.map(|o| (id, o))) {
            match outcome {
                Changed::Edit(o) => self.after_edit(n, o),
                Changed::Editor(o) => crate::text::editor::after_edit(n, o),
            }
        }
    }

    /// Everything to do when a drag is over. `done` is what became of it, if it was dropped.
    fn end_drag(&mut self, done: Option<DragAction>) {
        self.set_over(None);
        self.edge = None;
        let Some(session) = self.drag.take() else {
            return;
        };
        let source = session.source;
        let result = match done {
            Some(DragAction::Move) => DragResult::Moved,
            Some(DragAction::Copy) => DragResult::Copied,
            None => DragResult::Cancelled,
        };
        with_doc(|d| {
            d.drag = None;
            if let Some(id) = d.state.dragging.take()
                && d.get(id).is_some_and(|n| n.style.dragging.is_some())
            {
                d.mark_style_of(id);
            }
            d.mark_paint();
        });
        if let Some(owner) = session.preview {
            owner.dispose();
        }
        // The window that started a platform drag ends it for the thread. Another window that only
        // received it leaves the value to that one, which still drags it.
        if session.mode == Mode::Platform {
            OUTGOING.with(|o| o.borrow_mut().take());
        }
        if OUTGOING.with(|o| o.borrow().is_none()) {
            batch(|| dragged_signal().set(None));
        }
        if session.mode == Mode::Platform {
            with_doc(|d| d.request(PlatformRequest::CloseDragIcon));
        }
        let told =
            source.and_then(|id| with_doc(|d| d.get(id)?.listeners.gesture.as_ref()?.end.clone()));
        if let Some(f) = told {
            batch(|| f(result));
        }
        if session.mode == Mode::Inside {
            self.ensure_layout();
            if let Some(p) = self.pointer {
                self.update_hover(p);
            }
        }
    }

    /// Who it goes to now, and whatever follows from that.
    fn set_over(&mut self, next: Option<(NodeId, Accept)>) {
        let Some(session) = self.drag.as_mut() else {
            return;
        };
        let was = session.over.as_ref().map(|o| o.0);
        let now = next.as_ref().map(|o| o.0);
        session.over = next;
        if was == now {
            return;
        }
        let tell = with_doc(|d| {
            d.state.drag_over = now;
            set_drop_caret(d, was, None);
            let mut tell = Vec::new();
            for (id, on) in [(was, false), (now, true)] {
                let Some(n) = id.and_then(|id| d.get(id)) else {
                    continue;
                };
                let id = id.expect("just had a node");
                if n.style.drag_over.is_some() {
                    d.mark_style_of(id);
                }
                if let Some(f) = d.nodes[id]
                    .listeners
                    .drop
                    .as_ref()
                    .and_then(|c| c.hover.clone())
                {
                    tell.push((f, on));
                }
            }
            tell
        });
        if !tell.is_empty() {
            batch(|| {
                for (f, on) in tell {
                    f(on);
                }
            });
        }
    }

    /// Finds out what the dragged thing is over, after the pointer or what is under it moved.
    /// `moved` says to tell that about the position.
    pub(crate) fn retarget(&mut self, moved: bool) {
        let Some(session) = self.drag.as_ref() else {
            return;
        };
        if session.dropped || session.awaiting.is_some() {
            return;
        }
        let p = session.position;
        let chain = self
            .hit_test(p)
            .map(|t| with_doc(|d| d.ancestors(t)))
            .unwrap_or_default();
        let mut next = None;
        for id in chain {
            let session = self.drag.as_ref().expect("checked");
            if let Some(found) = with_doc(|d| accepts(d, id, session))
                && let Some(how) = acceptance(&found.0, found.1, session)
            {
                next = Some((id, how, Some(found.0)));
                break;
            }
            if let Some(takes) = with_doc(|d| edit_accepts(d, id, session)) {
                next = takes.map(|how| (id, how, None));
                break;
            }
        }
        let over = next.as_ref().map(|(id, how, _)| (*id, how.clone()));
        self.set_over(over);
        if let Some((id, Accept::Edit(_), _)) = &next {
            with_doc(|d| {
                let at = drop_offset(d, *id, p);
                set_drop_caret(d, Some(*id), at);
            });
        }
        let session = self.drag.as_ref().expect("checked");
        // Blink takes text from elsewhere as a copy: moving it is the source's business.
        let action =
            if session.mode == Mode::External && matches!(next, Some((_, Accept::Edit(_), _))) {
                DragAction::Copy
            } else {
                action_for(session.allowed, self.modifiers)
            };
        if session.mode == Mode::Inside {
            let cursor = match (&next, action) {
                (Some(_), DragAction::Copy) => CursorIcon::Copy,
                _ => CursorIcon::Grabbing,
            };
            self.set_cursor(cursor);
        } else {
            self.tell_platform_accept(action);
        }
        let session = self.drag.as_ref().expect("checked");
        if moved && let Some((id, _, Some(drop))) = &next {
            let handlers: Vec<_> = drop
                .over
                .iter()
                .filter(|(t, _)| *t == session.type_id)
                .map(|(_, f)| f.clone())
                .collect();
            if !handlers.is_empty() {
                let rect = with_doc(|d| d.window_rect(*id)).unwrap_or_default();
                let event = drop_event(p, rect, self.modifiers, action);
                let value = session.value.clone();
                batch(|| {
                    for f in handlers {
                        f(&*value, &event);
                    }
                });
            }
        }
    }

    fn tell_platform_accept(&mut self, action: DragAction) {
        let Some(session) = self.drag.as_mut() else {
            return;
        };
        let wanted = session.over.as_ref().and_then(|(_, how)| {
            let mime = match how {
                Accept::Mime(m) | Accept::Edit(m) => m.clone(),
                // Only so that it can be dropped here: nothing is read.
                Accept::Value => session.mimes.first()?.clone(),
            };
            Some((mime, action))
        });
        if session.told != wanted {
            session.told = wanted.clone();
            with_doc(|d| d.request(PlatformRequest::AcceptDrag(wanted)));
        }
    }

    pub(crate) fn drag_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::DragEntered {
                position,
                mimes,
                modifiers,
            } => {
                self.modifiers = modifiers;
                match &mut self.drag {
                    Some(session) => {
                        session.position = position;
                        session.told = None;
                    }
                    None => {
                        // A drag of another window of this thread, in the same formats: its own
                        // value, so that a typed drop target here takes it. Else another program's.
                        let own = OUTGOING.with(|o| {
                            o.borrow()
                                .as_ref()
                                .filter(|(_, _, own_mimes)| {
                                    own_mimes.len() == mimes.len()
                                        && own_mimes.iter().all(|m| mimes.contains(m))
                                })
                                .map(|(value, type_id, _)| (value.clone(), *type_id))
                        });
                        let (value, type_id): (Rc<dyn Any>, TypeId) = match own {
                            Some(own) => own,
                            None => (
                                Rc::new(ExternalDrag {
                                    mimes: mimes.clone(),
                                }),
                                TypeId::of::<ExternalDrag>(),
                            ),
                        };
                        self.drag = Some(DragSession {
                            source: None,
                            text_source: None,
                            value: value.clone(),
                            type_id,
                            items: Vec::new(),
                            mimes,
                            allowed: DragActions::Both,
                            mode: Mode::External,
                            over: None,
                            told: None,
                            position,
                            preview: None,
                            awaiting: None,
                            dropped: false,
                        });
                        batch(|| dragged_signal().set(Some(value)));
                    }
                }
                self.ensure_layout();
                self.retarget(true);
                self.update_edge(position, true);
            }
            WindowEvent::DragMoved {
                position,
                modifiers,
            } => {
                self.modifiers = modifiers;
                if let Some(session) = &mut self.drag {
                    session.position = position;
                }
                self.ensure_layout();
                self.retarget(true);
                self.update_edge(position, true);
            }
            WindowEvent::DragLeft => {
                self.edge = None;
                self.set_over(None);
                if let Some(session) = &mut self.drag {
                    session.told = None;
                }
                if self.drag.as_ref().is_some_and(|s| s.mode == Mode::External) {
                    self.end_drag(None);
                }
            }
            WindowEvent::DragDropped { position, action } => {
                self.edge = None;
                let Some(session) = self.drag.as_mut() else {
                    return;
                };
                session.position = position;
                let external = session.mode == Mode::External;
                if external
                    && let Some((node, how)) = session.over.clone()
                    && let Accept::Mime(mime) | Accept::Edit(mime) = how.clone()
                {
                    session.awaiting = Some((node, how, action, position));
                    self.set_over(None);
                    with_doc(|d| d.request(PlatformRequest::ReceiveDrop(mime)));
                    return;
                }
                session.dropped = true;
                self.deliver_own_drop(position, Some(action));
                self.set_over(None);
                with_doc(|d| d.request(PlatformRequest::FinishDrop));
                if external {
                    self.end_drag(None);
                }
            }
            WindowEvent::DropData { mime, bytes } => {
                let awaiting = self.drag.as_mut().and_then(|s| s.awaiting.take());
                if let Some((node, Accept::Edit(_), action, position)) = awaiting {
                    if !bytes.is_empty() {
                        self.drop_into_edit(node, &bytes, position, action, None);
                    }
                } else if let Some((node, _, action, position)) = awaiting {
                    let found = with_doc(|d| {
                        let n = d.get(node)?;
                        Some((n.listeners.drop.clone()?, d.window_rect(node)?))
                    });
                    if let Some((drop, rect)) = found
                        && !bytes.is_empty()
                        && let Some((_, f)) = drop.mimes.iter().find(|(m, _)| m.contains(&mime))
                    {
                        let event = drop_event(position, rect, self.modifiers, action);
                        batch(|| f(&bytes, &event));
                    }
                }
                self.end_drag(None);
            }
            WindowEvent::DragSourceEnded(result)
                if self.drag.as_ref().is_some_and(|s| s.mode == Mode::Platform) =>
            {
                self.end_drag(match result {
                    DragResult::Moved => Some(DragAction::Move),
                    DragResult::Copied => Some(DragAction::Copy),
                    DragResult::Cancelled => None,
                });
            }
            _ => {}
        }
    }

    /// What scrolls for something held at `p`, and how fast. `over` says it goes by what is under
    /// the pointer, rather than by what is grabbed.
    fn update_edge(&mut self, p: PointF, over: bool) {
        let wants = if over {
            self.drag.is_some()
        } else {
            self.press
                .as_ref()
                .and_then(|p| p.gesture.as_ref())
                .is_some_and(|g| g.autoscroll && g.started && !g.cancelled)
        };
        if !wants {
            self.edge = None;
            return;
        }
        let chain = if over {
            self.hit_test(p)
                .map(|t| with_doc(|d| d.ancestors(t)))
                .unwrap_or_default()
        } else {
            let node = self.press.as_ref().and_then(|p| p.target);
            node.map(|t| with_doc(|d| d.ancestors(t)))
                .unwrap_or_default()
        };
        let found = with_doc(|d| {
            chain
                .into_iter()
                .find_map(|id| edge_speed(d, id, p, EDGE).map(|s| (id, s)))
        });
        let now = Instant::now();
        match found {
            Some((node, speed)) => match &mut self.edge {
                Some(e) if e.node == node => e.speed = speed,
                _ => {
                    self.edge = Some(EdgeScroll {
                        node,
                        speed,
                        since: now,
                        last: now,
                        rest: (0.0, 0.0),
                    })
                }
            },
            None => self.edge = None,
        }
    }

    /// Keeps a text selection going when the pointer has left what scrolls.
    pub(crate) fn update_selection_edge(&mut self, p: PointF) {
        let chain = self
            .press
            .as_ref()
            .and_then(|p| p.target)
            .map(|t| with_doc(|d| d.ancestors(t)))
            .unwrap_or_default();
        let found = with_doc(|d| {
            chain
                .into_iter()
                .find_map(|id| edge_speed(d, id, p, 0.0).map(|s| (id, s)))
        });
        let now = Instant::now();
        self.edge = found.map(|(node, speed)| match self.edge.take() {
            Some(e) if e.node == node => EdgeScroll { speed, ..e },
            // Nothing can be grabbed by accident out there.
            _ => EdgeScroll {
                node,
                speed,
                since: now - std::time::Duration::from_secs_f32(EDGE_DELAY),
                last: now,
                rest: (0.0, 0.0),
            },
        });
    }

    /// Scrolls what is held near the edge. Part of a frame.
    pub(crate) fn scroll_edge(&mut self, now: Instant) {
        let Some(e) = &mut self.edge else {
            return;
        };
        let elapsed = now
            .saturating_duration_since(e.last)
            .as_secs_f32()
            .min(0.05);
        e.last = now;
        if now.saturating_duration_since(e.since).as_secs_f32() < EDGE_DELAY {
            return;
        }
        let (node, speed) = (e.node, e.speed);
        let mut rest = e.rest;
        let (dx, dy) = (speed.0 * elapsed, speed.1 * elapsed);
        let scrolled = with_doc(|d| {
            let dx = d.scale.pxf(dx);
            let dy = d.scale.pxf(dy);
            scroll_node(d, node, dx, dy, &mut rest)
        });
        if let Some(e) = &mut self.edge {
            e.rest = rest;
        }
        if !scrolled {
            let still = with_doc(|d| can_scroll(d, node, speed.0, speed.1));
            if !still {
                self.edge = None;
            }
            return;
        }
        self.ensure_layout();
        let p = self.pointer.unwrap_or_default();
        if self.drag.is_some() {
            self.retarget(true);
        } else if self
            .press
            .as_ref()
            .is_some_and(|p| matches!(p.drag, crate::ui::Drag::Select | crate::ui::Drag::Editor(_)))
        {
            self.pointer_moved(p);
        } else {
            self.tell_grab(GrabPhase::Move, p);
        }
    }
}

/// Says where what is dragged over an input would go, if anywhere.
fn set_drop_caret(doc: &mut Document, id: Option<NodeId>, at: Option<usize>) {
    if let Some(id) = id
        && doc.get(id).is_some_and(Node::is_editor)
    {
        crate::text::editor::set_drop_caret(doc, id, at);
        return;
    }
    if let Some(id) = id
        && let Some(e) = doc.get_mut(id).and_then(Node::edit_mut)
        && e.drop_caret != at
    {
        e.drop_caret = at;
        doc.mark_paint();
    }
}

fn whole(p: PointF) -> PointI {
    PointI::new(p.x.round() as i32, p.y.round() as i32)
}

/// Tells the node about its being grabbed, if it is still there.
fn tell_grab(g: &mut Gesture, modifiers: Modifiers, phase: GrabPhase, p: PointF) {
    let elapsed = g.began.elapsed().as_millis() as u32;
    if phase == GrabPhase::Start || phase == GrabPhase::Move {
        g.velocity.record(elapsed, p.x - g.last.x, p.y - g.last.y);
    }
    let speed = g.velocity.velocity(elapsed).unwrap_or((0.0, 0.0));
    let (node, start, last) = (g.node, g.start, g.last);
    g.last = p;
    let found = with_doc(|d| {
        let n = d.get(node).filter(|n| !n.is_disabled())?;
        let handler = n.listeners.gesture.as_ref()?.grab.clone()?;
        Some((handler, d.window_rect(node)?, d.scale))
    });
    let Some((handler, rect, scale)) = found else {
        return;
    };
    let logical = |x: f32, y: f32| PointF::new(scale.to_logical(x), scale.to_logical(y));
    let event = GrabEvent {
        phase,
        total: logical(p.x - start.x, p.y - start.y),
        delta: logical(p.x - last.x, p.y - last.y),
        velocity: logical(speed.0, speed.1),
        position: p,
        local: PointF::new(p.x - rect.x as f32, p.y - rect.y as f32),
        rect,
        modifiers,
    };
    batch(|| handler(&event));
}

/// How fast `id` should scroll with something held at `p`, in logical pixels per second, if it
/// should. `band` is how far from its edge counts, in logical pixels.
fn edge_speed(doc: &Document, id: NodeId, p: PointF, band: f32) -> Option<(f32, f32)> {
    let n = doc.get(id)?;
    let rect = doc.visible_window_rect(id)?;
    let band = doc.scale.pxf(band);
    let along = |at: f32, lo: f32, hi: f32| -> f32 {
        let near = |depth: f32| -> f32 {
            if band <= 0.0 {
                return if depth < 0.0 { 1.0 } else { 0.0 };
            }
            let t = ((band - depth) / band).clamp(0.0, 1.0);
            t * t
        };
        (near(hi - at) - near(at - lo)) * EDGE_SPEED
    };
    use anti_webui_style::Overflow;
    let dx = if n.computed.overflow_x == Overflow::Scroll {
        along(p.x, rect.x as f32, rect.right() as f32)
    } else {
        0.0
    };
    let dy = if n.computed.overflow_y == Overflow::Scroll || n.edit().is_some_and(|e| e.multiline) {
        along(p.y, rect.y as f32, rect.bottom() as f32)
    } else {
        0.0
    };
    ((dx != 0.0 || dy != 0.0) && can_scroll(doc, id, dx, dy)).then_some((dx, dy))
}

/// The size a drag icon's surface gets for a box, and its buffer's: see `PopupParams`.
pub(crate) fn icon_sizes(size: SizeI, scale: crate::geometry::Scale) -> (SizeI, SizeI) {
    let logical = SizeI::new(
        (scale.to_logical(size.width as f32)).ceil() as i32,
        (scale.to_logical(size.height as f32)).ceil() as i32,
    );
    let buffer = SizeI::new(
        (logical.width as f64 * scale.as_f64()).round() as i32,
        (logical.height as f64 * scale.as_f64()).round() as i32,
    );
    (logical, buffer)
}

/// The same in the other direction: logical pixels for physical ones, at the same place.
pub(crate) fn logical_point(p: PointI, scale: crate::geometry::Scale) -> PointI {
    PointI::new(
        scale.to_logical(p.x as f32).round() as i32,
        scale.to_logical(p.y as f32).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_survive_the_round_trip() {
        #[cfg(not(windows))]
        let (files, first) = (
            ["/tmp/a b.png", "/tmp/日本語/%.txt", "/x/#y?z"],
            "file:///tmp/a%20b.png\r\n",
        );
        #[cfg(windows)]
        let (files, first) = (
            [r"C:\tmp\a b.png", r"C:\tmp\日本語\%.txt", r"D:\x\#y"],
            "file:///C:/tmp/a%20b.png\r\n",
        );
        let list = uri_list(files);
        assert!(list.contains(first), "{list}");
        let back = files_of_uri_list(list.as_bytes());
        let want: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        assert_eq!(back, want);
    }

    #[test]
    fn uri_lists_only_give_local_files() {
        let list = "# comment\r\nhttp://example.com/a\r\nfile://localhost/b\r\nfile:///c%2\r\n";
        let back = files_of_uri_list(list.as_bytes());
        assert_eq!(back, [PathBuf::from("/b"), PathBuf::from("/c%2")]);
    }

    #[test]
    fn it_can_only_copy_files_unless_told() {
        let copies = DragData::default().files(["/a"]);
        assert_eq!(copies.allowed(), DragActions::Copy);
        assert_eq!(
            DragData::default()
                .files(["/a"])
                .actions(DragActions::Both)
                .allowed(),
            DragActions::Both
        );
        assert_eq!(DragData::new(1).allowed(), DragActions::Both);
    }

    #[test]
    fn what_was_built_is_not_wrapped_again() {
        let data = DragData::from_any(DragData::new(5u8));
        assert_eq!(data.value::<u8>(), Some(&5));
        let data = DragData::from_any(7u8);
        assert_eq!(data.value::<u8>(), Some(&7));
    }
}
