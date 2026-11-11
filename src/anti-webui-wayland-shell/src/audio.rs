//! Audio devices and the sound of each program, through PipeWire. libpipewire is loaded when this
//! is first used, as FFmpeg is for video; without it there is nothing here. See "音声" in
//! `docs/design/37-wayland-shell.md`.
//!
//! PipeWire runs a thread of its own, which tells of nodes, links and the default devices; what
//! comes in goes to signals there. Changing something takes that thread's lock for a moment.

mod ffi;
mod pod;

use crate::ServiceError;
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::task::Task;
use ffi::{
    CoreEvents, CoreMethods, Lib, MetadataEvents, MetadataMethods, NodeEvents, NodeInfo,
    NodeMethods, Object, RegistryEvents, RegistryMethods, SpaDict, SpaHook, SpaPod, methods,
};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::OnceLock;

/// A device sound goes to or comes from, like speakers or a microphone, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDevice {
    /// PipeWire's id, the same while it is there.
    pub id: u32,
    /// What PipeWire calls it by, like `alsa_output.pci-0000_00_1f.3.analog-stereo`.
    pub name: String,
    /// What to show, like `Built-in Audio Analog Stereo`.
    pub description: String,
    /// On the scale volume controls show, as WirePlumber's: 1 is as loud as it goes without
    /// amplifying, and it can go above.
    pub volume: f64,
    pub muted: bool,
    pub channels: u32,
}

/// The sound of a program, playing or recording, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioStream {
    pub id: u32,
    pub app_name: String,
    /// What it plays, like the title of a video, when the program says.
    pub media_name: String,
    /// The name of an icon in the icon theme, when the program says.
    pub icon_name: String,
    /// The file name of the program's executable, like `firefox`, when PipeWire knows it: what
    /// finds the program's `.desktop` file when it gives no icon.
    pub binary: String,
    /// What the sound is for, like `Music`, or `Notification` and `Event` for a system sound,
    /// when the program says.
    pub role: String,
    /// As [`AudioDevice::volume`].
    pub volume: f64,
    pub muted: bool,
    /// It records rather than plays.
    pub input: bool,
    /// The device it plays to or records from, when it is linked to one.
    pub target: Option<u32>,
}

/// Sound. Cheap to copy; reading tracks, on any thread.
#[derive(Clone, Copy, Debug)]
pub struct Audio(&'static Shared);

#[derive(Debug)]
struct Shared {
    available: SharedSignal<bool>,
    sinks: SharedSignal<Vec<AudioDevice>>,
    sources: SharedSignal<Vec<AudioDevice>>,
    streams: SharedSignal<Vec<AudioStream>>,
    default_sink: SharedSignal<Option<String>>,
    default_source: SharedSignal<Option<String>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
/// PipeWire, once started; it points at `SHARED`, so it starts after that is there.
static PW: OnceLock<Option<Pw>> = OnceLock::new();

/// Sound: its devices and the programs making it. It starts following them the first time.
pub fn audio() -> Audio {
    let shared = SHARED.get_or_init(|| Shared {
        available: shared_signal(false),
        sinks: shared_signal(Vec::new()),
        sources: shared_signal(Vec::new()),
        streams: shared_signal(Vec::new()),
        default_sink: shared_signal(None),
        default_source: shared_signal(None),
    });
    PW.get_or_init(|| {
        Pw::start(shared)
            .map_err(|e| eprintln!("[anti-webui] no audio: {e}"))
            .ok()
    });
    Audio(shared)
}

impl Audio {
    /// Whether PipeWire is there.
    pub fn available(&self) -> bool {
        self.0.available.get()
    }

    /// What sound goes to unless a program says otherwise.
    pub fn default_sink(&self) -> Option<AudioDevice> {
        let name = self.0.default_sink.get()?;
        self.sinks().into_iter().find(|d| d.name == name)
    }

    /// What sound comes from unless a program says otherwise.
    pub fn default_source(&self) -> Option<AudioDevice> {
        let name = self.0.default_source.get()?;
        self.sources().into_iter().find(|d| d.name == name)
    }

    /// Devices sound goes to, like speakers and headphones.
    pub fn sinks(&self) -> Vec<AudioDevice> {
        self.0.sinks.get()
    }

    /// Devices sound comes from, like microphones.
    pub fn sources(&self) -> Vec<AudioDevice> {
        self.0.sources.get()
    }

    /// The programs playing or recording.
    pub fn streams(&self) -> Vec<AudioStream> {
        self.0.streams.get()
    }

    /// Makes a device the default one, for sound going to it or coming from it, as it is.
    pub fn set_default(&self, device: &AudioDevice) -> Task<Result<(), ServiceError>> {
        let source = self.sources().iter().any(|d| d.id == device.id);
        let key = if source {
            "default.configured.audio.source"
        } else {
            "default.configured.audio.sink"
        };
        let value = format!("{{\"name\":{}}}", json_string(&device.name));
        Task::ready(with_pw(|pw| pw.set_default(key, &value)))
    }
}

impl AudioDevice {
    /// Sets the volume of every channel, on the scale of [`AudioDevice::volume`].
    pub fn set_volume(&self, volume: f64) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_volume(self.id, volume)))
    }

    pub fn set_muted(&self, muted: bool) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_muted(self.id, muted)))
    }
}

impl AudioStream {
    /// Sets the volume of every channel, on the scale of [`AudioDevice::volume`].
    pub fn set_volume(&self, volume: f64) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_volume(self.id, volume)))
    }

    pub fn set_muted(&self, muted: bool) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_muted(self.id, muted)))
    }
}

fn with_pw(f: impl FnOnce(&mut State) -> Result<(), ServiceError>) -> Result<(), ServiceError> {
    audio();
    let pw = PW
        .get()
        .and_then(Option::as_ref)
        .ok_or(ServiceError::Unavailable)?;
    pw.locked(f)
}

/// A string in JSON.
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `name` in what the `default` metadata says, like `{"name":"alsa_output.…"}`.
fn name_in(json: &str) -> Option<String> {
    let rest = &json[json.find("\"name\"")? + 6..];
    let rest = rest
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

/// On WirePlumber's scale, from linear.
fn cubic(linear: f32) -> f64 {
    (linear.max(0.0) as f64).cbrt()
}

/// What a node is, of those this follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sink,
    Source,
    Playback,
    Recording,
}

impl Kind {
    fn of(class: &str) -> Option<Self> {
        match class {
            "Audio/Sink" | "Audio/Duplex" => Some(Self::Sink),
            "Audio/Source" | "Audio/Source/Virtual" => Some(Self::Source),
            "Stream/Output/Audio" => Some(Self::Playback),
            "Stream/Input/Audio" => Some(Self::Recording),
            _ => None,
        }
    }
}

/// A node bound to hear of its volume. Boxed, so that PipeWire can keep pointers to it.
struct Node {
    id: u32,
    kind: Kind,
    proxy: *mut Object,
    hook: Box<SpaHook>,
    props: HashMap<String, String>,
    volumes: Vec<f32>,
    muted: bool,
    state: *mut State,
}

struct Metadata {
    proxy: *mut Object,
    /// Where PipeWire keeps its listener; never read here.
    _hook: Box<SpaHook>,
}

/// What PipeWire has told, kept on its thread or under its lock.
struct State {
    lib: &'static Lib,
    registry: *mut Object,
    nodes: HashMap<u32, Box<Node>>,
    /// Links by id: the node sound comes out of and the one it goes into.
    links: HashMap<u32, (u32, u32)>,
    metadata: Option<Metadata>,
    /// The device names the `default` metadata says.
    default_sink: Option<String>,
    default_source: Option<String>,
    shared: *const Shared,
}

/// PipeWire, running: its thread loop and what it tells, from any thread.
struct Pw {
    lib: &'static Lib,
    thread_loop: *mut Object,
    state: *mut State,
    _hooks: (Box<SpaHook>, Box<SpaHook>),
}

impl std::fmt::Debug for Pw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pw")
    }
}

// SAFETY: what the pointers point at is only touched on PipeWire's thread, or under its lock.
unsafe impl Send for Pw {}
// SAFETY: as above.
unsafe impl Sync for Pw {}

static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

static CORE_EVENTS: CoreEvents = CoreEvents {
    version: 0,
    info: None,
    done: None,
    ping: None,
    error: Some(core_error),
    remove_id: None,
    bound_id: None,
    add_mem: None,
    remove_mem: None,
};

static REGISTRY_EVENTS: RegistryEvents = RegistryEvents {
    version: 0,
    global: Some(global),
    global_remove: Some(global_remove),
};

static NODE_EVENTS: NodeEvents = NodeEvents {
    version: 0,
    info: Some(node_info),
    param: Some(node_param),
};

static METADATA_EVENTS: MetadataEvents = MetadataEvents {
    version: 0,
    property: Some(metadata_property),
};

impl Pw {
    fn start(shared: &'static Shared) -> Result<Self, String> {
        let lib = LIB.get_or_init(Lib::load).as_ref().map_err(Clone::clone)?;
        // SAFETY: PipeWire's own calls in the order its documentation gives; nothing runs on its
        // thread until `thread_loop_start`, so nothing needs the lock before then.
        unsafe {
            (lib.init)(std::ptr::null_mut(), std::ptr::null_mut());
            let name = c"anti-webui audio";
            let thread_loop = (lib.thread_loop_new)(name.as_ptr(), std::ptr::null());
            if thread_loop.is_null() {
                return Err("no thread loop".into());
            }
            let pw_loop = (lib.thread_loop_get_loop)(thread_loop);
            let context = (lib.context_new)(pw_loop, std::ptr::null_mut(), 0);
            if context.is_null() {
                return Err("no context".into());
            }
            let core = (lib.context_connect)(context, std::ptr::null_mut(), 0);
            if core.is_null() {
                return Err("can't connect to PipeWire".into());
            }
            let (core_methods, core_data) = methods::<CoreMethods>(core);
            let registry = (core_methods.get_registry)(core_data, ffi::VERSION_REGISTRY, 0);
            if registry.is_null() {
                return Err("no registry".into());
            }
            let state = Box::into_raw(Box::new(State {
                lib,
                registry,
                nodes: HashMap::new(),
                links: HashMap::new(),
                metadata: None,
                default_sink: None,
                default_source: None,
                shared,
            }));
            let mut core_hook = SpaHook::new();
            (core_methods.add_listener)(core_data, &mut *core_hook, &CORE_EVENTS, state.cast());
            let mut registry_hook = SpaHook::new();
            let (registry_methods, registry_data) = methods::<RegistryMethods>(registry);
            (registry_methods.add_listener)(
                registry_data,
                &mut *registry_hook,
                &REGISTRY_EVENTS,
                state.cast(),
            );
            if (lib.thread_loop_start)(thread_loop) < 0 {
                return Err("can't start PipeWire's thread".into());
            }
            shared.available.set(true);
            Ok(Self {
                lib,
                thread_loop,
                state,
                _hooks: (core_hook, registry_hook),
            })
        }
    }

    /// Calls `f` with what PipeWire told, holding its lock.
    fn locked<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        // SAFETY: the lock keeps PipeWire's thread out of `state` while `f` runs.
        unsafe {
            (self.lib.thread_loop_lock)(self.thread_loop);
            let r = f(&mut *self.state);
            (self.lib.thread_loop_unlock)(self.thread_loop);
            r
        }
    }
}

impl State {
    fn shared(&self) -> &Shared {
        // SAFETY: `Shared` is in a static that is never dropped, and was put there before
        // PipeWire's thread started.
        unsafe { &*self.shared }
    }

    fn set_volume(&mut self, id: u32, volume: f64) -> Result<(), ServiceError> {
        let node = self
            .nodes
            .get(&id)
            .ok_or(ServiceError::Failed("no such node".into()))?;
        let linear = volume.max(0.0).powi(3) as f32;
        let volumes = vec![linear; node.volumes.len().max(1)];
        set_props(node.proxy, &pod::build_props(Some(&volumes), None))
    }

    fn set_muted(&mut self, id: u32, muted: bool) -> Result<(), ServiceError> {
        let node = self
            .nodes
            .get(&id)
            .ok_or(ServiceError::Failed("no such node".into()))?;
        set_props(node.proxy, &pod::build_props(None, Some(muted)))
    }

    fn set_default(&mut self, key: &str, value: &str) -> Result<(), ServiceError> {
        let meta = self.metadata.as_ref().ok_or(ServiceError::Unsupported)?;
        let (key, kind, value) = (cstring(key)?, c"Spa:String:JSON", cstring(value)?);
        // SAFETY: a live metadata proxy, used under PipeWire's lock.
        let res = unsafe {
            let (m, data) = methods::<MetadataMethods>(meta.proxy);
            (m.set_property)(data, 0, key.as_ptr(), kind.as_ptr(), value.as_ptr())
        };
        result(res)
    }

    /// Puts what is known into the signals.
    fn publish(&self) {
        let shared = self.shared();
        let mut nodes: Vec<&Node> = self.nodes.values().map(|n| &**n).collect();
        nodes.sort_by_key(|n| n.id);
        let device = |n: &Node| AudioDevice {
            id: n.id,
            name: prop(n, "node.name"),
            description: ["node.description", "node.nick", "node.name"]
                .iter()
                .map(|k| prop(n, k))
                .find(|d| !d.is_empty())
                .unwrap_or_default(),
            volume: volume(n),
            muted: n.muted,
            channels: n.volumes.len() as u32,
        };
        let of = |kind| {
            nodes
                .iter()
                .filter(|n| n.kind == kind)
                .map(|n| device(n))
                .collect()
        };
        put(&shared.sinks, of(Kind::Sink));
        put(&shared.sources, of(Kind::Source));
        let streams = nodes
            .iter()
            .filter(|n| matches!(n.kind, Kind::Playback | Kind::Recording))
            .map(|n| {
                let input = n.kind == Kind::Recording;
                AudioStream {
                    id: n.id,
                    app_name: ["application.name", "node.description", "node.name"]
                        .iter()
                        .map(|k| prop(n, k))
                        .find(|d| !d.is_empty())
                        .unwrap_or_default(),
                    media_name: prop(n, "media.name"),
                    icon_name: prop(n, "application.icon-name"),
                    binary: prop(n, "application.process.binary"),
                    role: prop(n, "media.role"),
                    volume: volume(n),
                    muted: n.muted,
                    input,
                    target: self.links.values().find_map(|&(out, into)| match input {
                        false if out == n.id => Some(into),
                        true if into == n.id => Some(out),
                        _ => None,
                    }),
                }
            })
            .collect();
        put(&shared.streams, streams);
        put(&shared.default_sink, self.default_sink.clone());
        put(&shared.default_source, self.default_source.clone());
    }
}

fn prop(n: &Node, key: &str) -> String {
    n.props.get(key).cloned().unwrap_or_default()
}

fn volume(n: &Node) -> f64 {
    if n.volumes.is_empty() {
        return 1.0;
    }
    cubic(n.volumes.iter().sum::<f32>() / n.volumes.len() as f32)
}

fn put<T: PartialEq + Send + 'static>(signal: &SharedSignal<T>, value: T) {
    if signal.with_untracked(|v| *v != value) {
        signal.set(value);
    }
}

fn cstring(s: &str) -> Result<CString, ServiceError> {
    CString::new(s).map_err(|e| ServiceError::Failed(e.to_string()))
}

fn result(res: c_int) -> Result<(), ServiceError> {
    if res < 0 {
        Err(ServiceError::Failed(
            std::io::Error::from_raw_os_error(-res).to_string(),
        ))
    } else {
        Ok(())
    }
}

/// Sets the `Props` of a node.
fn set_props(proxy: *mut Object, pod: &[u64]) -> Result<(), ServiceError> {
    // SAFETY: a live node proxy, used under PipeWire's lock; the pod outlives the call, which
    // copies it.
    let res = unsafe {
        let (m, data) = methods::<NodeMethods>(proxy);
        (m.set_param)(data, pod::PARAM_PROPS, 0, pod.as_ptr().cast::<SpaPod>())
    };
    result(res)
}

unsafe extern "C" fn core_error(
    data: *mut c_void,
    id: u32,
    _seq: c_int,
    res: c_int,
    _: *const c_char,
) {
    // An error about the core itself with a broken pipe: PipeWire went away.
    if id == 0 && res == -libc::EPIPE {
        // SAFETY: `data` is the state, on PipeWire's thread.
        let state = unsafe { &mut *data.cast::<State>() };
        state.shared().available.set(false);
    }
}

unsafe extern "C" fn global(
    data: *mut c_void,
    id: u32,
    _permissions: u32,
    kind: *const c_char,
    _version: u32,
    props: *const SpaDict,
) {
    // SAFETY: `data` is the state, on PipeWire's thread; the strings and dict are PipeWire's for
    // the length of the call.
    let (state, kind, props) = unsafe {
        (
            &mut *data.cast::<State>(),
            std::ffi::CStr::from_ptr(kind).to_bytes_with_nul(),
            ffi::dict(props),
        )
    };
    let props: HashMap<String, String> = props.into_iter().collect();
    if kind == ffi::INTERFACE_LINK {
        let node = |k: &str| props.get(k).and_then(|v| v.parse().ok());
        if let (Some(out), Some(into)) = (node("link.output.node"), node("link.input.node")) {
            state.links.insert(id, (out, into));
            state.publish();
        }
    } else if kind == ffi::INTERFACE_NODE {
        let Some(kind) = props.get("media.class").and_then(|c| Kind::of(c)) else {
            return;
        };
        // SAFETY: binding through the registry, on PipeWire's thread.
        let proxy = unsafe {
            let (m, registry) = methods::<RegistryMethods>(state.registry);
            (m.bind)(
                registry,
                id,
                ffi::INTERFACE_NODE.as_ptr().cast(),
                ffi::VERSION_NODE,
                0,
            )
        };
        if proxy.is_null() {
            return;
        }
        let mut node = Box::new(Node {
            id,
            kind,
            proxy,
            hook: SpaHook::new(),
            props,
            volumes: Vec::new(),
            muted: false,
            state,
        });
        let node_ptr: *mut Node = &mut *node;
        // SAFETY: a node proxy just bound; the hook and the node are boxed and stay until the
        // proxy is destroyed.
        unsafe {
            let (m, data) = methods::<NodeMethods>(proxy);
            (m.add_listener)(data, &mut *node.hook, &NODE_EVENTS, node_ptr.cast());
            let mut ids = [pod::PARAM_PROPS];
            (m.subscribe_params)(data, ids.as_mut_ptr(), 1);
        }
        state.nodes.insert(id, node);
        state.publish();
    } else if kind == ffi::INTERFACE_METADATA {
        if props.get("metadata.name").map(String::as_str) != Some("default")
            || state.metadata.is_some()
        {
            return;
        }
        // SAFETY: as for nodes.
        unsafe {
            let (m, registry) = methods::<RegistryMethods>(state.registry);
            let proxy = (m.bind)(
                registry,
                id,
                ffi::INTERFACE_METADATA.as_ptr().cast(),
                ffi::VERSION_METADATA,
                0,
            );
            if proxy.is_null() {
                return;
            }
            let mut hook = SpaHook::new();
            let (m, meta) = methods::<MetadataMethods>(proxy);
            (m.add_listener)(meta, &mut *hook, &METADATA_EVENTS, data);
            state.metadata = Some(Metadata { proxy, _hook: hook });
        }
    }
}

unsafe extern "C" fn global_remove(data: *mut c_void, id: u32) {
    // SAFETY: `data` is the state, on PipeWire's thread.
    let state = unsafe { &mut *data.cast::<State>() };
    if let Some(mut node) = state.nodes.remove(&id) {
        // SAFETY: the hook is in the proxy's list until removed here, then the proxy goes.
        unsafe {
            node.hook.remove();
            (state.lib.proxy_destroy)(node.proxy);
        }
    } else if state.links.remove(&id).is_none() {
        return;
    }
    state.publish();
}

unsafe extern "C" fn node_info(data: *mut c_void, info: *const NodeInfo) {
    // SAFETY: `data` is a node of the state, on PipeWire's thread; `info` is PipeWire's for the
    // length of the call.
    let (node, info) = unsafe { (&mut *data.cast::<Node>(), &*info) };
    if info.change_mask & ffi::NODE_CHANGE_MASK_PROPS == 0 {
        return;
    }
    // SAFETY: as above.
    node.props.extend(unsafe { ffi::dict(info.props) });
    // SAFETY: the state outlives its nodes.
    unsafe { &*node.state }.publish();
}

unsafe extern "C" fn node_param(
    data: *mut c_void,
    _seq: c_int,
    id: u32,
    _index: u32,
    _next: u32,
    param: *const SpaPod,
) {
    if id != pod::PARAM_PROPS || param.is_null() {
        return;
    }
    // SAFETY: `data` is a node of the state, on PipeWire's thread; the pod is its header and
    // `size` bytes after it.
    let (node, bytes) = unsafe {
        let size = (*param).size as usize;
        (
            &mut *data.cast::<Node>(),
            std::slice::from_raw_parts(param.cast::<u8>(), 8 + size),
        )
    };
    let Some(props) = pod::parse_props(bytes) else {
        return;
    };
    if let Some(volumes) = props.volumes.filter(|v| !v.is_empty()) {
        node.volumes = volumes;
    } else if let Some(v) = props.volume
        && node.volumes.is_empty()
    {
        node.volumes = vec![v];
    }
    if let Some(muted) = props.mute {
        node.muted = muted;
    }
    // SAFETY: the state outlives its nodes.
    unsafe { &*node.state }.publish();
}

unsafe extern "C" fn metadata_property(
    data: *mut c_void,
    subject: u32,
    key: *const c_char,
    _kind: *const c_char,
    value: *const c_char,
) -> c_int {
    // SAFETY: `data` is the state, on PipeWire's thread; the strings are PipeWire's for the call.
    let (state, key, value) = unsafe {
        (
            &mut *data.cast::<State>(),
            ffi::string(key),
            ffi::string(value),
        )
    };
    if subject != 0 {
        return 0;
    }
    let name = value.as_deref().and_then(name_in);
    match key.as_deref() {
        Some("default.audio.sink") => state.default_sink = name,
        Some("default.audio.source") => state.default_source = name,
        // Everything was cleared.
        None => {
            state.default_sink = None;
            state.default_source = None;
        }
        _ => return 0,
    }
    state.publish();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_are_on_wireplumbers_scale() {
        assert!((cubic(0.125) - 0.5).abs() < 1e-6);
        assert!((cubic(1.0) - 1.0).abs() < 1e-9);
        assert_eq!(cubic(-1.0), 0.0);
    }

    #[test]
    fn default_names_are_read_and_written_as_json() {
        assert_eq!(
            name_in(r#"{ "name": "alsa_output.pci-0000_00_1f.3.analog-stereo" }"#).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        assert_eq!(name_in(r#"{"name":"a\"bA"}"#).as_deref(), Some("a\"bA"));
        assert_eq!(name_in("{}"), None);
        let written = format!("{{\"name\":{}}}", json_string("x\"y\\z"));
        assert_eq!(name_in(&written).as_deref(), Some("x\"y\\z"));
    }

    /// Reads the PipeWire of the machine: `cargo test -p anti-webui-wayland-shell --features
    /// audio -- --ignored --nocapture real_pipewire`.
    #[test]
    #[ignore = "needs a running PipeWire"]
    fn real_pipewire_is_read() {
        let audio = audio();
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(audio.available());
        assert!(!audio.sinks().is_empty());
        println!("default sink: {:?}", audio.default_sink());
        for stream in audio.streams() {
            println!("{stream:?}");
        }
    }

    #[test]
    fn classes_are_sorted_into_kinds() {
        assert_eq!(Kind::of("Audio/Sink"), Some(Kind::Sink));
        assert_eq!(Kind::of("Audio/Source/Virtual"), Some(Kind::Source));
        assert_eq!(Kind::of("Stream/Input/Audio"), Some(Kind::Recording));
        assert_eq!(Kind::of("Video/Source"), None);
    }
}
