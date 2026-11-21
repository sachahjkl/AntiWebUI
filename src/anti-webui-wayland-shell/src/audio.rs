//! Audio devices and the sound of each program, through PipeWire. libpipewire is loaded when this
//! is first used, as FFmpeg is for video; without it there is nothing here. See "音声" in
//! `docs/design/37-wayland-shell.md`.
//!
//! PipeWire runs a thread of its own, which tells of nodes, links, cards and the default devices;
//! what comes in goes to signals there. Changing something takes that thread's lock for a moment.
//!
//! The sound of a device of a card is kept in the route the card uses for it, as WirePlumber keeps
//! it, and the node's own volume may say something else; so it is read from and written to that.

mod ffi;
mod pod;

use crate::ServiceError;
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::task::Task;
use ffi::{
    CoreEvents, CoreMethods, DeviceEvents, DeviceInfo, DeviceMethods, Lib, MetadataEvents,
    MetadataMethods, NodeEvents, NodeInfo, NodeMethods, Object, RegistryEvents, RegistryMethods,
    SpaDict, SpaHook, SpaPod, methods,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
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
    /// amplifying, and it can go above. The loudest channel's.
    pub volume: f64,
    pub muted: bool,
    pub channels: u32,
    /// The volume of each channel, on the scale of `volume`.
    pub volumes: Vec<f64>,
    /// Where each channel goes, like `FL` and `FR`, in the order of `volumes`, when known.
    pub positions: Vec<String>,
    /// The card it is on (an [`AudioCard::id`]), when it is on one.
    pub card: Option<u32>,
    /// The ports of its card it can use, like headphones and speakers, in the card's profile.
    pub ports: Vec<AudioPort>,
    /// The [`AudioPort::index`] of the port in use.
    pub port: Option<u32>,
}

/// A port of a card's device, like headphones, speakers or HDMI, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioPort {
    /// Its index among the card's ports.
    pub index: u32,
    /// What the card calls it by, like `analog-output-headphones`.
    pub name: String,
    /// What to show, like `Headphones`.
    pub description: String,
    /// Something is plugged into it (`Some(true)`), nothing is (`Some(false)`), or the card
    /// can't tell.
    pub available: Option<bool>,
    pub priority: i32,
}

/// A sound card, as it was when read: what it is set up to do (its profile).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioCard {
    /// PipeWire's id, the same while it is there.
    pub id: u32,
    /// What PipeWire calls it by, like `alsa_card.pci-0000_00_1f.3`.
    pub name: String,
    /// What to show, like `Built-in Audio`.
    pub description: String,
    /// The name of an icon in the icon theme, when the card says.
    pub icon_name: String,
    /// What it can be set up to do, in the card's order.
    pub profiles: Vec<AudioProfile>,
    /// The [`AudioProfile::index`] of the profile in use.
    pub profile: Option<u32>,
}

/// A way a card can be set up, like `Analog Stereo Duplex`, `Digital Stereo (HDMI)` or `Off`.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioProfile {
    pub index: u32,
    /// What the card calls it by, like `output:analog-stereo+input:analog-stereo`.
    pub name: String,
    /// What to show.
    pub description: String,
    /// It can be used now (`Some(true)`), it can't (`Some(false)`: nothing plugged in), or the
    /// card can't tell.
    pub available: Option<bool>,
    pub priority: i32,
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
    cards: SharedSignal<Vec<AudioCard>>,
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
        cards: shared_signal(Vec::new()),
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

    /// The sound cards, with their profiles.
    pub fn cards(&self) -> Vec<AudioCard> {
        self.0.cards.get()
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
        Task::ready(with_pw(|pw| {
            pw.set_metadata(0, key, Some(c"Spa:String:JSON"), Some(value.as_str()))
        }))
    }
}

impl AudioDevice {
    /// Sets the volume, on the scale of [`AudioDevice::volume`]: the loudest channel gets it, and
    /// the others keep their share of it, so the balance stays.
    pub fn set_volume(&self, volume: f64) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_volume(self.id, volume)))
    }

    /// Sets the volume of each channel, in the order of [`AudioDevice::volumes`].
    pub fn set_volumes(&self, volumes: &[f64]) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_volumes(self.id, volumes)))
    }

    pub fn set_muted(&self, muted: bool) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_muted(self.id, muted)))
    }

    /// Between the left channels and the right ones: -1 is all left, 0 even, 1 all right. 0 when
    /// it has no channel on one side.
    pub fn balance(&self) -> f64 {
        balance_of(&self.volumes, &self.positions)
    }

    /// Sets the balance, as [`AudioDevice::balance`] reads it, at the same volume.
    pub fn set_balance(&self, balance: f64) -> Task<Result<(), ServiceError>> {
        self.set_volumes(&with_balance(&self.volumes, &self.positions, balance))
    }

    /// Uses the port [`AudioPort::index`] of its card, like headphones rather than speakers.
    pub fn set_port(&self, index: u32) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_port(self.id, index)))
    }
}

impl AudioStream {
    /// Sets the volume, as [`AudioDevice::set_volume`] does.
    pub fn set_volume(&self, volume: f64) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_volume(self.id, volume)))
    }

    pub fn set_muted(&self, muted: bool) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_muted(self.id, muted)))
    }

    /// Sends it to `device`, or takes it from there when it records: WirePlumber links it again,
    /// and remembers the choice for the program.
    pub fn move_to(&self, device: &AudioDevice) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.move_stream(self.id, device.id)))
    }
}

impl AudioCard {
    /// Sets the card up as its profile [`AudioProfile::index`] says.
    pub fn set_profile(&self, index: u32) -> Task<Result<(), ServiceError>> {
        Task::ready(with_pw(|pw| pw.set_profile(self.id, index)))
    }
}

/// Which side a channel position is on: `Some(false)` left, `Some(true)` right.
fn side(position: &str) -> Option<bool> {
    const LEFT: [&str; 13] = [
        "FL", "SL", "FLC", "RL", "TFL", "TRL", "RLC", "FLW", "FLH", "TFLC", "TSL", "LLFE", "BLC",
    ];
    const RIGHT: [&str; 13] = [
        "FR", "SR", "FRC", "RR", "TFR", "TRR", "RRC", "FRW", "FRH", "TFRC", "TSR", "RLFE", "BRC",
    ];
    if LEFT.contains(&position) {
        Some(false)
    } else if RIGHT.contains(&position) {
        Some(true)
    } else {
        None
    }
}

/// The balance of volumes, as PulseAudio reckons it: the quieter side's share of the louder.
fn balance_of(volumes: &[f64], positions: &[String]) -> f64 {
    let (mut left, mut right) = (0.0f64, 0.0f64);
    let (mut lefts, mut rights) = (0u32, 0u32);
    for (&v, p) in volumes.iter().zip(positions) {
        match side(p) {
            Some(false) => (left, lefts) = (left + v, lefts + 1),
            Some(true) => (right, rights) = (right + v, rights + 1),
            None => {}
        }
    }
    if lefts == 0 || rights == 0 {
        return 0.0;
    }
    let (left, right) = (left / lefts as f64, right / rights as f64);
    if left == right {
        0.0
    } else if left > right {
        right / left - 1.0
    } else {
        1.0 - left / right
    }
}

/// `volumes` with the balance given, at the loudest of them.
fn with_balance(volumes: &[f64], positions: &[String], balance: f64) -> Vec<f64> {
    let balance = balance.clamp(-1.0, 1.0);
    let loudest = volumes.iter().copied().fold(0.0, f64::max);
    volumes
        .iter()
        .enumerate()
        .map(|(i, _)| match positions.get(i).and_then(|p| side(p)) {
            Some(false) if balance > 0.0 => loudest * (1.0 - balance),
            Some(true) if balance < 0.0 => loudest * (1.0 + balance),
            _ => loudest,
        })
        .collect()
}

/// Positions as PipeWire's `audio.position` lists them, like `FL,FR` or `[ FL FR ]`.
fn positions_in(list: &str) -> Vec<String> {
    list.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

fn availability(value: u32) -> Option<bool> {
    match value {
        pod::AVAILABLE_YES => Some(true),
        pod::AVAILABLE_NO => Some(false),
        _ => None,
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
    /// Where each channel goes, from its `Props`.
    positions: Vec<String>,
    state: *mut State,
}

/// What the params of a device of cards say.
#[derive(Clone, Default)]
struct CardParams {
    /// Its profiles (`EnumProfile`), by index, and the one in use (`Profile`).
    profiles: BTreeMap<u32, pod::Profile>,
    profile: Option<u32>,
    /// Its routes (`EnumRoute`), the ports, by index.
    ports: BTreeMap<i32, pod::Route>,
    /// The route each of its devices uses now (`Route`), which keeps that device's sound.
    routes: Vec<pod::Route>,
}

impl CardParams {
    /// Forgets what the param `id` said, as it is read again.
    fn clear(&mut self, id: u32) {
        match id {
            pod::PARAM_ENUM_PROFILE => self.profiles.clear(),
            pod::PARAM_PROFILE => self.profile = None,
            pod::PARAM_ENUM_ROUTE => self.ports.clear(),
            _ => self.routes.clear(),
        }
    }

    /// Takes in a pod of the param `id`, given with its header.
    fn take(&mut self, id: u32, bytes: &[u8]) {
        match id {
            pod::PARAM_ENUM_PROFILE => {
                if let Some(p) = pod::parse_profile(bytes) {
                    self.profiles.insert(p.index, p);
                }
            }
            pod::PARAM_PROFILE => {
                if let Some(p) = pod::parse_profile(bytes) {
                    self.profile = Some(p.index);
                }
            }
            pod::PARAM_ENUM_ROUTE => {
                if let Some(r) = pod::parse_route(bytes) {
                    self.ports.insert(r.index, r);
                }
            }
            _ => {
                // A route read twice, when its params changed again before the sync came back,
                // is the later.
                if let Some(route) = pod::parse_route(bytes) {
                    self.routes.retain(|r| r.device != route.device);
                    self.routes.push(route);
                }
            }
        }
    }
}

/// A device of cards, bound to hear of its profiles and routes. Boxed, as nodes are.
struct Device {
    proxy: *mut Object,
    hook: Box<SpaHook>,
    props: HashMap<String, String>,
    params: CardParams,
    /// The params being read, and the sync that ends it.
    incoming: CardParams,
    sync: Option<c_int>,
    /// The flags each param had when last told, to read again only those that changed.
    param_flags: HashMap<u32, u32>,
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
    core: *mut Object,
    registry: *mut Object,
    nodes: HashMap<u32, Box<Node>>,
    devices: HashMap<u32, Box<Device>>,
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
    done: Some(core_done),
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

static DEVICE_EVENTS: DeviceEvents = DeviceEvents {
    version: 0,
    info: Some(device_info),
    param: Some(device_param),
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
                core,
                registry,
                nodes: HashMap::new(),
                devices: HashMap::new(),
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

    fn node(&self, id: u32) -> Result<&Node, ServiceError> {
        self.nodes
            .get(&id)
            .map(|n| &**n)
            .ok_or(ServiceError::Failed("no such node".into()))
    }

    /// The device of cards a node is on, and the node's device index on it.
    fn card_of(&self, n: &Node) -> Option<(&Device, i32)> {
        let device = self.devices.get(&prop(n, "device.id").parse().ok()?)?;
        let at: i32 = prop(n, "card.profile.device").parse().ok()?;
        Some((device, at))
    }

    /// The route the sound of a node is kept in, and its device's proxy, when it has one.
    fn route(&self, n: &Node) -> Option<(&pod::Route, *mut Object)> {
        let (device, at) = self.card_of(n)?;
        let route = device.params.routes.iter().find(|r| r.device == at)?;
        Some((route, device.proxy))
    }

    fn set_volume(&mut self, id: u32, volume: f64) -> Result<(), ServiceError> {
        let node = self.node(id)?;
        let route = self.route(node).map(|(r, _)| r);
        let now: Vec<f64> = volumes(node, route).iter().map(|&v| cubic(v)).collect();
        let loudest = now.iter().copied().fold(0.0, f64::max);
        let volume = volume.max(0.0);
        let volumes: Vec<f64> = if loudest > 1e-4 {
            now.iter().map(|v| v * volume / loudest).collect()
        } else {
            vec![volume; now.len().max(1)]
        };
        self.set_volumes(id, &volumes)
    }

    fn set_volumes(&mut self, id: u32, volumes: &[f64]) -> Result<(), ServiceError> {
        let node = self.node(id)?;
        if volumes.is_empty() {
            return Ok(());
        }
        let linear: Vec<f32> = volumes.iter().map(|v| v.max(0.0).powi(3) as f32).collect();
        if let Some((route, device)) = self.route(node) {
            return set_device_param(
                device,
                pod::PARAM_ROUTE,
                &pod::build_route(route, Some(&linear), None),
            );
        }
        set_props(node.proxy, &pod::build_props(Some(&linear), None))
    }

    fn set_muted(&mut self, id: u32, muted: bool) -> Result<(), ServiceError> {
        let node = self.node(id)?;
        if let Some((route, device)) = self.route(node) {
            return set_device_param(
                device,
                pod::PARAM_ROUTE,
                &pod::build_route(route, None, Some(muted)),
            );
        }
        set_props(node.proxy, &pod::build_props(None, Some(muted)))
    }

    /// Sets `key` of `subject` in the `default` metadata, or takes it away when `value` is None.
    fn set_metadata(
        &mut self,
        subject: u32,
        key: &str,
        kind: Option<&CStr>,
        value: Option<&str>,
    ) -> Result<(), ServiceError> {
        let meta = self.metadata.as_ref().ok_or(ServiceError::Unsupported)?;
        let key = cstring(key)?;
        let value = value.map(cstring).transpose()?;
        // SAFETY: a live metadata proxy, used under PipeWire's lock.
        let res = unsafe {
            let (m, data) = methods::<MetadataMethods>(meta.proxy);
            (m.set_property)(
                data,
                subject,
                key.as_ptr(),
                kind.map_or(std::ptr::null(), CStr::as_ptr),
                value.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            )
        };
        result(res)
    }

    /// Asks WirePlumber to link the stream `id` to the device `device`, as pipewire-pulse does
    /// for `move-sink-input`: `target.object` names the device by its serial.
    fn move_stream(&mut self, id: u32, device: u32) -> Result<(), ServiceError> {
        self.node(id)?;
        let target = self.node(device)?;
        let serial = prop(target, "object.serial");
        let (kind, value) = if serial.is_empty() {
            (None, prop(target, "node.name"))
        } else {
            (Some(c"Spa:Id"), serial)
        };
        self.set_metadata(id, "target.node", None, None)?;
        self.set_metadata(id, "target.object", kind, Some(value.as_str()))
    }

    fn set_port(&mut self, id: u32, index: u32) -> Result<(), ServiceError> {
        let node = self.node(id)?;
        let (device, at) = self.card_of(node).ok_or(ServiceError::Unsupported)?;
        set_device_param(
            device.proxy,
            pod::PARAM_ROUTE,
            &pod::build_port(index as i32, at),
        )
    }

    fn set_profile(&mut self, id: u32, index: u32) -> Result<(), ServiceError> {
        let device = self
            .devices
            .get(&id)
            .ok_or(ServiceError::Failed("no such card".into()))?;
        set_device_param(device.proxy, pod::PARAM_PROFILE, &pod::build_profile(index))
    }

    /// The ports of a card's device going the way of the node, in the card's profile, and the one
    /// in use.
    fn ports(&self, node: &Node) -> (Vec<AudioPort>, Option<u32>) {
        let Some((device, at)) = self.card_of(node) else {
            return (Vec::new(), None);
        };
        let direction = if node.kind == Kind::Sink {
            pod::DIRECTION_OUTPUT
        } else {
            0
        };
        let profile = device.params.profile.map(|p| p as i32);
        let ports = device
            .params
            .ports
            .values()
            .filter(|r| {
                r.direction == direction
                    && r.devices.contains(&at)
                    && (r.profiles.is_empty() || profile.is_none_or(|p| r.profiles.contains(&p)))
            })
            .map(|r| AudioPort {
                index: r.index as u32,
                name: r.name.clone(),
                description: if r.description.is_empty() {
                    r.name.clone()
                } else {
                    r.description.clone()
                },
                available: availability(r.available),
                priority: r.priority,
            })
            .collect();
        let port = self.route(node).map(|(r, _)| r.index as u32);
        (ports, port)
    }

    /// Puts what is known into the signals.
    fn publish(&self) {
        let shared = self.shared();
        let mut nodes: Vec<&Node> = self.nodes.values().map(|n| &**n).collect();
        nodes.sort_by_key(|n| n.id);
        let device = |n: &Node| {
            let route = self.route(n).map(|(r, _)| r);
            let linear = volumes(n, route);
            let (ports, port) = self.ports(n);
            let map = route.and_then(|r| r.props.channel_map.as_ref());
            AudioDevice {
                id: n.id,
                name: prop(n, "node.name"),
                description: ["node.description", "node.nick", "node.name"]
                    .iter()
                    .map(|k| prop(n, k))
                    .find(|d| !d.is_empty())
                    .unwrap_or_default(),
                volume: volume(linear),
                muted: route.and_then(|r| r.props.mute).unwrap_or(n.muted),
                channels: linear.len() as u32,
                volumes: linear.iter().map(|&v| cubic(v)).collect(),
                positions: match map {
                    Some(map) if map.len() == linear.len() => map
                        .iter()
                        .map(|&p| pod::channel_name(p).to_owned())
                        .collect(),
                    _ if n.positions.len() == linear.len() => n.positions.clone(),
                    _ => positions_in(&prop(n, "audio.position")),
                },
                card: prop(n, "device.id").parse().ok(),
                ports,
                port,
            }
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
                    volume: volume(&n.volumes),
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
        let mut devices: Vec<(&u32, &Device)> =
            self.devices.iter().map(|(id, d)| (id, &**d)).collect();
        devices.sort_by_key(|&(id, _)| *id);
        let cards = devices
            .iter()
            .map(|&(&id, d)| {
                let prop = |k: &str| d.props.get(k).cloned().unwrap_or_default();
                AudioCard {
                    id,
                    name: prop("device.name"),
                    description: ["device.description", "device.nick", "device.name"]
                        .iter()
                        .map(|k| prop(k))
                        .find(|d| !d.is_empty())
                        .unwrap_or_default(),
                    icon_name: prop("device.icon-name"),
                    profiles: d
                        .params
                        .profiles
                        .values()
                        .map(|p| AudioProfile {
                            index: p.index,
                            name: p.name.clone(),
                            description: if p.description.is_empty() {
                                p.name.clone()
                            } else {
                                p.description.clone()
                            },
                            available: availability(p.available),
                            priority: p.priority,
                        })
                        .collect(),
                    profile: d.params.profile,
                }
            })
            .collect();
        put(&shared.cards, cards);
        put(&shared.default_sink, self.default_sink.clone());
        put(&shared.default_source, self.default_source.clone());
    }
}

fn prop(n: &Node, key: &str) -> String {
    n.props.get(key).cloned().unwrap_or_default()
}

/// The linear volume of each channel of a node: its route's when the route says, else its own.
fn volumes<'a>(n: &'a Node, route: Option<&'a pod::Route>) -> &'a [f32] {
    route
        .and_then(|r| r.props.volumes.as_deref())
        .unwrap_or(&n.volumes)
}

/// The loudest channel's volume, as PulseAudio shows a device's.
fn volume(volumes: &[f32]) -> f64 {
    if volumes.is_empty() {
        return 1.0;
    }
    cubic(volumes.iter().copied().fold(0.0, f32::max))
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

/// Sets a param of a device of cards, like its profile or a route.
fn set_device_param(proxy: *mut Object, id: u32, pod: &[u64]) -> Result<(), ServiceError> {
    // SAFETY: a live device proxy, used under PipeWire's lock; the pod outlives the call, which
    // copies it.
    let res = unsafe {
        let (m, data) = methods::<DeviceMethods>(proxy);
        (m.set_param)(data, id, 0, pod.as_ptr().cast::<SpaPod>())
    };
    result(res)
}

/// The params of a device of cards this reads.
const CARD_PARAMS: [u32; 4] = [
    pod::PARAM_ENUM_PROFILE,
    pod::PARAM_PROFILE,
    pod::PARAM_ENUM_ROUTE,
    pod::PARAM_ROUTE,
];

impl Device {
    /// Reads the params `ids` again; what comes back is taken when a sync after them comes back,
    /// so that what is no longer there goes too.
    fn ask(&mut self, ids: &[u32]) {
        if self.sync.is_none() {
            self.incoming = self.params.clone();
        }
        // SAFETY: a live device proxy and the state's core, on PipeWire's thread.
        unsafe {
            let (m, proxy) = methods::<DeviceMethods>(self.proxy);
            for &id in ids {
                self.incoming.clear(id);
                (m.enum_params)(proxy, 0, id, 0, u32::MAX, std::ptr::null());
            }
            let (m, core) = methods::<CoreMethods>((*self.state).core);
            self.sync = Some((m.sync)(core, ffi::ID_CORE, 0));
        }
    }
}

unsafe extern "C" fn core_done(data: *mut c_void, id: u32, seq: c_int) {
    if id != ffi::ID_CORE {
        return;
    }
    // SAFETY: `data` is the state, on PipeWire's thread.
    let state = unsafe { &mut *data.cast::<State>() };
    let mut read = false;
    for device in state.devices.values_mut() {
        if device.sync == Some(seq) {
            device.sync = None;
            device.params = std::mem::take(&mut device.incoming);
            read = true;
        }
    }
    if read {
        state.publish();
    }
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
            positions: Vec::new(),
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
    } else if kind == ffi::INTERFACE_DEVICE {
        if props.get("media.class").map(String::as_str) != Some("Audio/Device") {
            return;
        }
        // SAFETY: as for nodes.
        let proxy = unsafe {
            let (m, registry) = methods::<RegistryMethods>(state.registry);
            (m.bind)(
                registry,
                id,
                ffi::INTERFACE_DEVICE.as_ptr().cast(),
                ffi::VERSION_DEVICE,
                0,
            )
        };
        if proxy.is_null() {
            return;
        }
        let mut device = Box::new(Device {
            proxy,
            hook: SpaHook::new(),
            props,
            params: CardParams::default(),
            incoming: CardParams::default(),
            sync: None,
            param_flags: HashMap::new(),
            state,
        });
        let device_ptr: *mut Device = &mut *device;
        // SAFETY: as for nodes. Its profiles and routes are read when it tells of its params.
        unsafe {
            let (m, data) = methods::<DeviceMethods>(proxy);
            (m.add_listener)(data, &mut *device.hook, &DEVICE_EVENTS, device_ptr.cast());
        }
        state.devices.insert(id, device);
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
    } else if let Some(mut device) = state.devices.remove(&id) {
        // SAFETY: as for nodes.
        unsafe {
            device.hook.remove();
            (state.lib.proxy_destroy)(device.proxy);
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
    if let Some(map) = props.channel_map.filter(|m| !m.is_empty()) {
        node.positions = map
            .into_iter()
            .map(|p| pod::channel_name(p).to_owned())
            .collect();
    }
    // SAFETY: the state outlives its nodes.
    unsafe { &*node.state }.publish();
}

unsafe extern "C" fn device_info(data: *mut c_void, info: *const DeviceInfo) {
    // SAFETY: `data` is a device of the state, on PipeWire's thread; `info` is PipeWire's for the
    // length of the call, with `n_params` params.
    let (device, info) = unsafe { (&mut *data.cast::<Device>(), &*info) };
    if info.change_mask & ffi::DEVICE_CHANGE_MASK_PROPS != 0 {
        // SAFETY: as above.
        device.props.extend(unsafe { ffi::dict(info.props) });
        // SAFETY: the state outlives its devices.
        unsafe { &*device.state }.publish();
    }
    if info.change_mask & ffi::DEVICE_CHANGE_MASK_PARAMS == 0 || info.params.is_null() {
        return;
    }
    // Its params changed: those of them this reads whose flags changed (the server flips a flag
    // each time one changes) are read again.
    // SAFETY: as above.
    let params = unsafe { std::slice::from_raw_parts(info.params, info.n_params as usize) };
    let changed: Vec<u32> = params
        .iter()
        .filter(|p| CARD_PARAMS.contains(&p.id) && p.flags & ffi::PARAM_INFO_READ != 0)
        .filter(|p| device.param_flags.insert(p.id, p.flags) != Some(p.flags))
        .map(|p| p.id)
        .collect();
    if !changed.is_empty() {
        device.ask(&changed);
    }
}

unsafe extern "C" fn device_param(
    data: *mut c_void,
    _seq: c_int,
    id: u32,
    _index: u32,
    _next: u32,
    param: *const SpaPod,
) {
    if param.is_null() || !CARD_PARAMS.contains(&id) {
        return;
    }
    // SAFETY: `data` is a device of the state, on PipeWire's thread; the pod is its header and
    // `size` bytes after it.
    let (device, bytes) = unsafe {
        let size = (*param).size as usize;
        (
            &mut *data.cast::<Device>(),
            std::slice::from_raw_parts(param.cast::<u8>(), 8 + size),
        )
    };
    device.incoming.take(id, bytes);
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
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(audio.available());
        assert!(!audio.sinks().is_empty());
        println!("default sink: {:?}", audio.default_sink());
        for stream in audio.streams() {
            println!("{stream:?}");
        }
        for card in audio.cards() {
            println!("{card:?}");
        }
    }

    #[test]
    fn balance_goes_both_ways() {
        let stereo = vec!["FL".to_owned(), "FR".to_owned()];
        assert_eq!(balance_of(&[0.8, 0.8], &stereo), 0.0);
        assert!((balance_of(&[0.8, 0.4], &stereo) + 0.5).abs() < 1e-9);
        assert!((balance_of(&[0.4, 0.8], &stereo) - 0.5).abs() < 1e-9);
        let set = with_balance(&[0.8, 0.8], &stereo, -0.5);
        assert_eq!(set, vec![0.8, 0.4]);
        assert!((balance_of(&set, &stereo) + 0.5).abs() < 1e-9);
        // Mono has no sides.
        assert_eq!(balance_of(&[0.5], &["MONO".to_owned()]), 0.0);
        assert_eq!(positions_in("[ FL, FR ]"), stereo);
    }

    #[test]
    fn classes_are_sorted_into_kinds() {
        assert_eq!(Kind::of("Audio/Sink"), Some(Kind::Sink));
        assert_eq!(Kind::of("Audio/Source/Virtual"), Some(Kind::Source));
        assert_eq!(Kind::of("Stream/Input/Audio"), Some(Kind::Recording));
        assert_eq!(Kind::of("Video/Source"), None);
    }
}
