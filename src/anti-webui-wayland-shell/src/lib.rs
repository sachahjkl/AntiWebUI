//! Desktop shells on Wayland: bars, wallpapers, launchers and the like as layer surfaces, on as
//! many threads as there are outputs if wanted, with what a shell shows. See
//! `docs/design/37-wayland-shell.md`. Linux only: elsewhere the crate is empty.
//!
//! ```no_run,ignore-apple,ignore-windows
//! use anti_webui_core::prelude::*;
//! use anti_webui_wayland_shell::prelude::*;
//!
//! Shell::new()
//!     .namespace("my-shell")
//!     .run(|| {
//!         spawn_for_each_output(|output| {
//!             open_layer(LayerOptions::bar(Anchor::TOP, 32), move || {
//!                 text(move || format!("{} {}", output.name, local_time(Every::Minute).format("%H:%M")))
//!             });
//!         });
//!     })
//!     .unwrap();
//! ```

#![cfg(target_os = "linux")]

#[cfg(feature = "apps")]
mod apps;
#[cfg(feature = "audio")]
mod audio;
#[cfg(feature = "battery")]
mod battery;
#[cfg(feature = "bluetooth")]
mod bluetooth;
mod capture;
mod clipboard;
mod conn;
#[cfg(feature = "dbus")]
mod dbus;
#[cfg(feature = "hyprland")]
pub mod hyprland;
mod idle;
#[cfg(any(feature = "hyprland", feature = "niri", feature = "sway"))]
mod ipc;
#[cfg(any(
    feature = "hyprland",
    feature = "niri",
    feature = "sway",
    feature = "session"
))]
mod listeners;
mod lock;
#[cfg(feature = "media")]
mod media;
#[cfg(feature = "network")]
mod network;
#[cfg(feature = "niri")]
pub mod niri;
#[cfg(feature = "notifications")]
mod notifications;
mod output;
#[cfg(feature = "power-profiles")]
mod power_profiles;
#[cfg(feature = "process")]
mod process;
mod protocols;
#[cfg(feature = "session")]
mod session;
#[cfg(feature = "sway")]
pub mod sway;
#[cfg(feature = "system")]
mod system;
mod time;
mod toplevels;
#[cfg(feature = "tray")]
mod tray;
mod workspaces;

#[cfg(feature = "apps")]
pub use apps::{AppAction, Application, Apps, apps, icon_path};
#[cfg(feature = "audio")]
pub use audio::{Audio, AudioDevice, AudioStream, audio};
#[cfg(feature = "battery")]
pub use battery::{Battery, BatteryDevice, BatteryKind, BatteryState, battery};
#[cfg(feature = "bluetooth")]
pub use bluetooth::{Bluetooth, BluetoothAdapter, BluetoothDevice, bluetooth};
pub use capture::{Capture, CaptureOptions, CaptureSource, CaptureTarget, capture, thumbnail};
pub use clipboard::{Clipboard, clipboard};
pub use idle::{Idle, idle};
pub use lock::{LockState, SessionLock, lock_session, lock_state, spawn_lock_session};
#[cfg(feature = "media")]
pub use media::{Media, PlaybackStatus, Player, media};
#[cfg(feature = "network")]
pub use network::{
    AccessPoint, ActivationState, ConnectionKind, ConnectionSettings, Connectivity, DeviceKind,
    DeviceState, IpConfig, IpMethod, IpSettings, NetError, Network, NetworkDevice, SavedConnection,
    Security, Wired, network,
};
#[cfg(feature = "notifications")]
pub use notifications::{Notification, NotificationServer, Urgency, notification_server};
#[cfg(feature = "power-profiles")]
pub use power_profiles::{PowerProfiles, power_profiles};
#[cfg(feature = "process")]
pub use process::{Process, RunningProcess};
#[cfg(any(feature = "hyprland", feature = "niri", feature = "sway"))]
pub use serde_json;
#[cfg(feature = "session")]
pub use session::{Session, SessionAction, session};
#[cfg(feature = "system")]
pub use system::{Disk, Memory, NetSpeed, System, Temperature, system, system_every};
pub use toplevels::{Toplevel, Toplevels, toplevels};
#[cfg(feature = "tray")]
pub use tray::{
    MenuToggle, Tray, TrayIcon, TrayItem, TrayMenuItem, TrayStatus, TrayTooltip, tray, tray_menu,
};
pub use workspaces::{Workspace, WorkspaceAction, WorkspaceSource, Workspaces, workspaces};

pub use anti_webui_core::layer::{
    Anchor, Exclusive, InputRegion, Keyboard, Layer, LayerSize, Margin,
};
pub use output::{
    Output, OutputTransform, current_output, for_each_output, outputs, spawn_for_each_output,
};
pub use time::{Every, LocalTime, local_time};

use anti_webui_app::App;
use anti_webui_core::layer::{LayerChange, LayerSpec};
use anti_webui_core::reactive::{Owner, on_cleanup};
use anti_webui_core::settings::Settings;
use anti_webui_core::view::IntoView;
use anti_webui_core::window::{
    Command, Role, WindowHandle, WindowOptions, open_window, spawn_window,
};
use std::sync::Mutex;

pub mod prelude {
    pub use crate::{
        Anchor, Capture, CaptureOptions, CaptureSource, CaptureTarget, Clipboard, Every, Exclusive,
        Idle, InputRegion, Keyboard, Layer, LayerHandle, LayerOptions, LayerSize, LocalTime,
        Margin, Output, OutputTransform, ServiceError, Shell, ShellError, Toplevel, Toplevels,
        Workspace, WorkspaceAction, WorkspaceSource, Workspaces, capture, clipboard,
        current_output, for_each_output, idle, local_time, open_layer, outputs,
        spawn_for_each_output, spawn_layer, thumbnail, toplevels, workspaces,
    };
    #[cfg(feature = "apps")]
    pub use crate::{Application, Apps, apps};
    #[cfg(feature = "audio")]
    pub use crate::{Audio, AudioDevice, audio};
    #[cfg(feature = "battery")]
    pub use crate::{Battery, BatteryState, battery};
    #[cfg(feature = "bluetooth")]
    pub use crate::{Bluetooth, bluetooth};
    #[cfg(feature = "network")]
    pub use crate::{Connectivity, Network, network};
    pub use crate::{LockState, SessionLock, lock_session, spawn_lock_session};
    #[cfg(feature = "media")]
    pub use crate::{Media, PlaybackStatus, Player, media};
    #[cfg(feature = "notifications")]
    pub use crate::{Notification, NotificationServer, notification_server};
    #[cfg(feature = "power-profiles")]
    pub use crate::{PowerProfiles, power_profiles};
    #[cfg(feature = "process")]
    pub use crate::{Process, RunningProcess};
    #[cfg(feature = "session")]
    pub use crate::{Session, SessionAction, session};
    #[cfg(feature = "system")]
    pub use crate::{System, system, system_every};
    #[cfg(feature = "tray")]
    pub use crate::{Tray, TrayItem, tray, tray_menu};
    pub use anti_webui_core::window::quit;
}

/// The namespace layers get unless they say otherwise.
static NAMESPACE: Mutex<String> = Mutex::new(String::new());

fn default_namespace() -> String {
    let n = NAMESPACE.lock().map(|n| n.clone()).unwrap_or_default();
    if n.is_empty() { "anti-webui".into() } else { n }
}

/// Why a shell can't run.
#[derive(Debug)]
pub enum ShellError {
    /// What the compositor would need and doesn't have, like `zwlr_layer_shell_v1` on GNOME.
    Unsupported {
        missing: &'static str,
    },
    Failed(Box<dyn std::error::Error>),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::Unsupported { missing } => write!(f, "the shell needs {missing}"),
            ShellError::Failed(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ShellError {}

/// Why something asked of a service didn't happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceError {
    /// What would do it isn't there, like a compositor without the protocol.
    Unavailable,
    /// It is there, but can't do this.
    Unsupported,
    /// What this would take the name of on the bus already has an owner, like another
    /// notification daemon.
    NameTaken,
    Failed(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Unavailable => f.write_str("not available here"),
            ServiceError::Unsupported => f.write_str("not supported here"),
            ServiceError::NameTaken => f.write_str("another program has the name on the bus"),
            ServiceError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ServiceError {}

/// A program with no window of its own, which opens layer surfaces and runs until
/// [`quit`](anti_webui_core::window::quit).
pub struct Shell {
    app: App,
    namespace: Option<String>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    pub fn new() -> Self {
        Self {
            app: App::new(),
            namespace: None,
        }
    }

    /// What compositors tell the layers of this shell by, in their rules. `anti-webui` otherwise.
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// For the windows it opens besides layers.
    pub fn app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app = self.app.app_id(app_id);
        self
    }

    /// As [`App::settings`].
    pub fn settings(mut self, f: impl FnOnce(&mut Settings) + 'static) -> Self {
        self.app = self.app.settings(f);
        self
    }

    /// Runs `root`, which opens layers and windows, until
    /// [`quit`](anti_webui_core::window::quit).
    pub fn run(self, root: impl FnOnce()) -> Result<(), ShellError> {
        if !anti_webui_wayland::can_connect() {
            return Err(ShellError::Unsupported {
                missing: "a Wayland compositor",
            });
        }
        if !anti_webui_wayland::supports_layer_shell() {
            return Err(ShellError::Unsupported {
                missing: "zwlr_layer_shell_v1",
            });
        }
        if let (Some(n), Ok(mut namespace)) = (self.namespace, NAMESPACE.lock()) {
            *namespace = n;
        }
        self.app.run_shell(root).map_err(ShellError::Failed)
    }
}

/// Which output a layer goes on.
#[derive(Clone, Debug, Default, PartialEq)]
enum OutputChoice {
    /// The one of the [`for_each_output`] around it, if any; otherwise the compositor's choice.
    #[default]
    Current,
    Named(String),
    Any,
}

/// How a layer surface opens. Sizes and margins are in logical pixels.
///
/// Along an axis with no size given, it stretches from edge to edge if anchored to both, and
/// otherwise follows the size of what is in it.
#[derive(Clone, Debug)]
pub struct LayerOptions {
    spec: LayerSpec,
    width: Option<LayerSize>,
    height: Option<LayerSize>,
    output: OutputChoice,
    namespace: Option<String>,
}

impl Default for LayerOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerOptions {
    pub fn new() -> Self {
        Self {
            spec: LayerSpec::default(),
            width: None,
            height: None,
            output: OutputChoice::Current,
            namespace: None,
        }
    }

    /// Along one edge, from end to end, keeping windows away by its thickness.
    pub fn bar(edge: Anchor, thickness: i32) -> Self {
        let (across, along) = if edge == Anchor::LEFT || edge == Anchor::RIGHT {
            (Anchor::TOP | Anchor::BOTTOM, true)
        } else {
            (Anchor::LEFT | Anchor::RIGHT, false)
        };
        let options = Self::new().anchor(edge | across).exclusive(Exclusive::Auto);
        if along {
            options.width(thickness)
        } else {
            options.height(thickness)
        }
    }

    /// All of the output, under everything.
    pub fn background() -> Self {
        Self::new()
            .layer(Layer::Background)
            .anchor(Anchor::ALL)
            .exclusive(Exclusive::Ignore)
    }

    /// Over everything, letting every click through.
    pub fn overlay() -> Self {
        Self::new()
            .layer(Layer::Overlay)
            .input_region(InputRegion::Empty)
    }

    pub fn layer(mut self, layer: Layer) -> Self {
        self.spec.layer = layer;
        self
    }

    /// The edges it is anchored to; none puts it in the middle.
    pub fn anchor(mut self, edges: Anchor) -> Self {
        self.spec.anchor = edges;
        self
    }

    pub fn width(mut self, width: i32) -> Self {
        self.width = Some(LayerSize::Fixed(width));
        self
    }

    pub fn height(mut self, height: i32) -> Self {
        self.height = Some(LayerSize::Fixed(height));
        self
    }

    pub fn size(self, width: i32, height: i32) -> Self {
        self.width(width).height(height)
    }

    /// Follows the size of what is in it along the axes with no size given, even where it is
    /// anchored to both edges.
    pub fn fit_content(mut self) -> Self {
        self.width.get_or_insert(LayerSize::Content);
        self.height.get_or_insert(LayerSize::Content);
        self
    }

    pub fn exclusive(mut self, exclusive: Exclusive) -> Self {
        self.spec.exclusive = exclusive;
        self
    }

    /// For a layer anchored to a corner: which of its edges [`Exclusive`] applies to.
    pub fn exclusive_edge(mut self, edge: Anchor) -> Self {
        self.spec.exclusive_edge = Some(edge);
        self
    }

    pub fn margin(mut self, margin: Margin) -> Self {
        self.spec.margin = margin;
        self
    }

    pub fn keyboard(mut self, keyboard: Keyboard) -> Self {
        self.spec.keyboard = keyboard;
        self
    }

    pub fn input_region(mut self, region: InputRegion) -> Self {
        self.spec.input_region = region;
        self
    }

    /// Every click goes to what is below.
    pub fn click_through(self) -> Self {
        self.input_region(InputRegion::Empty)
    }

    pub fn output(mut self, output: &Output) -> Self {
        self.output = OutputChoice::Named(output.name.clone());
        self
    }

    /// Leaves the output to the compositor, even in a [`for_each_output`].
    pub fn any_output(mut self) -> Self {
        self.output = OutputChoice::Any;
        self
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// What is asked for, with what the protocol wouldn't take put right: a compositor ends the
    /// connection over it.
    fn spec(self) -> LayerSpec {
        let mut spec = self.spec;
        let anchor = spec.anchor;
        let axis = |given: Option<LayerSize>, spans: bool, name: &str| match given {
            Some(LayerSize::Fixed(n)) if n <= 0 => {
                warn(&format!("a {name} of {n} isn't a size"));
                if spans {
                    LayerSize::Stretch
                } else {
                    LayerSize::Content
                }
            }
            Some(LayerSize::Stretch) if !spans => {
                warn(&format!(
                    "stretching the {name} needs both of its edges anchored"
                ));
                LayerSize::Content
            }
            Some(size) => size,
            None if spans => LayerSize::Stretch,
            None => LayerSize::Content,
        };
        spec.width = axis(self.width, anchor.spans_width(), "width");
        spec.height = axis(self.height, anchor.spans_height(), "height");
        if let Some(edge) = spec.exclusive_edge
            && (edge.bits().count_ones() != 1 || !anchor.contains(edge))
        {
            warn("the exclusive edge has to be one of the edges it is anchored to");
            spec.exclusive_edge = None;
        }
        if spec.exclusive == Exclusive::Auto
            && spec.exclusive_edge.or(anchor.exclusive_edge()).is_none()
        {
            warn("no one edge to keep windows away from; anchor to one, or give exclusive_edge");
            spec.exclusive = Exclusive::None;
        }
        spec.output = match self.output {
            OutputChoice::Current => current_output().map(|o| o.name),
            OutputChoice::Named(name) => Some(name),
            OutputChoice::Any => None,
        };
        spec.namespace = self.namespace.unwrap_or_else(default_namespace);
        spec
    }

    fn window(self) -> WindowOptions {
        let spec = self.spec();
        WindowOptions::new()
            .title(spec.namespace.clone())
            .role(Role::Layer(Box::new(spec)))
    }
}

fn warn(what: &str) {
    if cfg!(debug_assertions) {
        eprintln!("[anti-webui] layer: {what}");
    }
}

/// A layer surface: what [`WindowHandle`] does, and changes to the surface. It can be used from
/// any thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerHandle(WindowHandle);

impl std::ops::Deref for LayerHandle {
    type Target = WindowHandle;
    fn deref(&self) -> &WindowHandle {
        &self.0
    }
}

impl LayerHandle {
    pub fn window(&self) -> &WindowHandle {
        &self.0
    }

    fn change(&self, change: LayerChange) {
        self.0.command(Command::Layer(change));
    }

    pub fn set_size(&self, width: LayerSize, height: LayerSize) {
        self.change(LayerChange::Size(width, height));
    }

    pub fn set_anchor(&self, edges: Anchor) {
        self.change(LayerChange::Anchor(edges));
    }

    pub fn set_margin(&self, margin: Margin) {
        self.change(LayerChange::Margin(margin));
    }

    pub fn set_exclusive(&self, exclusive: Exclusive) {
        self.change(LayerChange::Exclusive(exclusive));
    }

    /// Like taking the keyboard while a launcher is open.
    pub fn set_keyboard(&self, keyboard: Keyboard) {
        self.change(LayerChange::Keyboard(keyboard));
    }

    pub fn set_layer(&self, layer: Layer) {
        self.change(LayerChange::Layer(layer));
    }

    pub fn set_input_region(&self, region: InputRegion) {
        self.change(LayerChange::InputRegion(region));
    }
}

/// Closes the layer with the owner it was opened under, like what a view makes.
fn tie(handle: WindowHandle) -> LayerHandle {
    if Owner::current().is_some() {
        let h = handle.clone();
        on_cleanup(move || h.close());
    }
    LayerHandle(handle)
}

/// Opens a layer surface on this thread, as [`open_window`] does a window: `root` can use the
/// signals of this thread. It closes with the owner it was opened under, and with the window or
/// [`for_each_output`] it was opened in.
pub fn open_layer<V: IntoView>(
    options: LayerOptions,
    root: impl FnOnce() -> V + 'static,
) -> LayerHandle {
    tie(open_window(options.window(), root))
}

/// Opens a layer surface on a thread of its own, as [`spawn_window`] does a window.
pub fn spawn_layer<V: IntoView>(
    options: LayerOptions,
    root: impl FnOnce() -> V + Send + 'static,
) -> LayerHandle {
    tie(spawn_window(options.window(), root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_axis_without_a_size_stretches_between_its_edges_or_follows_what_is_in_it() {
        let bar = LayerOptions::bar(Anchor::TOP, 32).any_output().spec();
        assert_eq!(bar.width, LayerSize::Stretch);
        assert_eq!(bar.height, LayerSize::Fixed(32));
        assert_eq!(bar.anchor, Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        assert_eq!(bar.exclusive, Exclusive::Auto);

        let side = LayerOptions::bar(Anchor::RIGHT, 48).any_output().spec();
        assert_eq!(
            (side.width, side.height),
            (LayerSize::Fixed(48), LayerSize::Stretch)
        );

        let osd = LayerOptions::overlay()
            .anchor(Anchor::BOTTOM)
            .any_output()
            .spec();
        assert_eq!(
            (osd.width, osd.height),
            (LayerSize::Content, LayerSize::Content)
        );

        let wall = LayerOptions::background().fit_content().any_output().spec();
        assert_eq!(wall.width, LayerSize::Content);
    }

    #[test]
    fn what_the_protocol_would_refuse_is_put_right() {
        let corner = LayerOptions::new()
            .anchor(Anchor::TOP | Anchor::RIGHT)
            .exclusive(Exclusive::Auto)
            .any_output()
            .spec();
        assert_eq!(corner.exclusive, Exclusive::None);

        let told = LayerOptions::new()
            .anchor(Anchor::TOP | Anchor::RIGHT)
            .exclusive(Exclusive::Auto)
            .exclusive_edge(Anchor::BOTTOM)
            .any_output()
            .spec();
        assert_eq!(told.exclusive_edge, None);

        let zero = LayerOptions::new().width(0).any_output().spec();
        assert_eq!(zero.width, LayerSize::Content);
    }

    #[test]
    fn layers_take_the_shell_s_namespace_unless_given_one() {
        let spec = LayerOptions::new().namespace("osd").any_output().spec();
        assert_eq!(spec.namespace, "osd");
        assert!(!LayerOptions::new().any_output().spec().namespace.is_empty());
    }
}
