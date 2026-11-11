//! Networks through NetworkManager: whether the internet can be reached, Wi-Fi and its access
//! points, the wired connection, the devices with their addresses, and the saved connections,
//! which can be joined, left, forgotten and edited.

use crate::ServiceError;
use crate::dbus::{self, Props, get, put, string};
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::task::Task;
use futures_lite::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, OnceLock};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const WIRED: &str = "org.freedesktop.NetworkManager.Device.Wired";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4: &str = "org.freedesktop.NetworkManager.IP4Config";
const IP6: &str = "org.freedesktop.NetworkManager.IP6Config";

/// `DeviceType` of an ethernet and a Wi-Fi device.
const ETHERNET: u32 = 1;
const WIFI: u32 = 2;
/// `State` of a device that is connected.
const ACTIVATED: u32 = 100;

/// How far out a connection reaches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Connectivity {
    #[default]
    Unknown,
    /// Not connected at all.
    None,
    /// Behind a login page, like a hotel's.
    Portal,
    /// Connected, but the internet can't be reached.
    Limited,
    Full,
}

impl Connectivity {
    fn of(n: u32) -> Self {
        match n {
            1 => Self::None,
            2 => Self::Portal,
            3 => Self::Limited,
            4 => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// What it takes to join a Wi-Fi network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Security {
    #[default]
    Open,
    Wep,
    /// WPA or WPA2 with a password.
    Wpa,
    /// WPA3 with a password (SAE).
    Wpa3,
    /// 802.1X, with an account rather than a password.
    Enterprise,
}

impl Security {
    fn of(p: &Props) -> Self {
        let flags: u32 = get(p, "Flags").unwrap_or(0);
        let keys = get::<u32>(p, "WpaFlags").unwrap_or(0) | get::<u32>(p, "RsnFlags").unwrap_or(0);
        if keys & 0x200 != 0 {
            Self::Enterprise
        } else if keys & 0x100 != 0 {
            Self::Wpa
        } else if keys & 0x400 != 0 {
            Self::Wpa3
        } else if keys & 0x800 != 0 || keys & 0x1000 != 0 {
            // Enhanced open: encrypted, but without a password.
            Self::Open
        } else if keys != 0 {
            Self::Wpa
        } else if flags & 1 != 0 {
            Self::Wep
        } else {
            Self::Open
        }
    }
}

/// A Wi-Fi network as an access point gives it, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessPoint {
    pub ssid: String,
    /// From 0 to 100.
    pub strength: u8,
    /// In MHz.
    pub frequency: u32,
    pub security: Security,
    /// A connection to it is saved, so it joins without asking.
    pub known: bool,
    /// It is the one connected to.
    pub active: bool,
    path: String,
}

/// The wired connection, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct Wired {
    /// Like `enp3s0`.
    pub interface: String,
    pub connected: bool,
    /// In Mb/s, 0 when not known.
    pub speed: u32,
}

/// Why joining a network didn't happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// It needs a password, which wasn't given or isn't saved.
    NeedsSecret,
    Service(ServiceError),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::NeedsSecret => f.write_str("a password is needed"),
            NetError::Service(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for NetError {}

impl From<ServiceError> for NetError {
    fn from(e: ServiceError) -> Self {
        NetError::Service(e)
    }
}

/// What a network device is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Ethernet,
    Wifi,
    /// Loopback, tunnels, bridges and the rest.
    #[default]
    Other,
}

impl DeviceKind {
    fn of(n: u32) -> Self {
        match n {
            ETHERNET => Self::Ethernet,
            WIFI => Self::Wifi,
            _ => Self::Other,
        }
    }
}

/// Where a network device is in connecting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeviceState {
    #[default]
    Unknown,
    /// NetworkManager leaves it alone.
    Unmanaged,
    /// It can't connect now: Wi-Fi is off, or no cable is in.
    Unavailable,
    Disconnected,
    /// From preparing to getting its addresses.
    Connecting,
    Connected,
    Disconnecting,
    Failed,
}

impl DeviceState {
    fn of(n: u32) -> Self {
        match n {
            10 => Self::Unmanaged,
            20 => Self::Unavailable,
            30 => Self::Disconnected,
            40..=90 => Self::Connecting,
            100 => Self::Connected,
            110 => Self::Disconnecting,
            120 => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// The addresses of a device on one IP version, as it has them now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpConfig {
    /// Each as `address/prefix`, like `192.168.1.20/24`.
    pub addresses: Vec<String>,
    /// "" when there is none.
    pub gateway: String,
    pub dns: Vec<String>,
}

/// A network device, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkDevice {
    /// Like `enp3s0` or `wlan0`.
    pub interface: String,
    pub kind: DeviceKind,
    pub state: DeviceState,
    /// The MAC address, like `A0:AD:9F:BA:C6:32`.
    pub hardware_address: String,
    /// In Mb/s, 0 when not known: the link's speed, or Wi-Fi's bit rate.
    pub speed: u32,
    /// Whether it connects by itself. Disconnecting it turns this off.
    pub autoconnect: bool,
    /// The name of the connection it is on.
    pub connection: Option<String>,
    pub ipv4: IpConfig,
    pub ipv6: IpConfig,
    path: String,
}

/// What a saved connection is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ConnectionKind {
    Ethernet,
    Wifi,
    /// A VPN plugin's, or WireGuard.
    Vpn,
    #[default]
    Other,
}

impl ConnectionKind {
    fn of(name: &str) -> Self {
        match name {
            "802-3-ethernet" => Self::Ethernet,
            "802-11-wireless" => Self::Wifi,
            "vpn" | "wireguard" => Self::Vpn,
            _ => Self::Other,
        }
    }
}

/// Where a saved connection is in being used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ActivationState {
    #[default]
    Inactive,
    Activating,
    Activated,
    Deactivating,
}

impl ActivationState {
    fn of(n: u32) -> Self {
        match n {
            1 => Self::Activating,
            2 => Self::Activated,
            3 => Self::Deactivating,
            _ => Self::Inactive,
        }
    }
}

/// A connection NetworkManager keeps, as it was when read.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedConnection {
    pub name: String,
    pub uuid: String,
    pub kind: ConnectionKind,
    /// NetworkManager's name of its type, like `802-3-ethernet`, `vpn` or `loopback`.
    pub type_name: String,
    /// The network, for Wi-Fi.
    pub ssid: Option<String>,
    pub autoconnect: bool,
    pub state: ActivationState,
    /// The interface it is bound to; "" for any.
    pub interface: String,
    /// When it was last connected, in seconds since 1970; 0 for never.
    pub last_used: u64,
    path: String,
    /// Its active connection, while there is one.
    active: Option<String>,
}

/// How a connection gets its addresses on one IP version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IpMethod {
    /// DHCP, or router advertisements for IPv6.
    #[default]
    Automatic,
    Manual,
    LinkLocal,
    /// Shares the connection with other computers.
    Shared,
    Disabled,
}

impl IpMethod {
    fn of(name: &str) -> Self {
        match name {
            "manual" => Self::Manual,
            "link-local" => Self::LinkLocal,
            "shared" => Self::Shared,
            "disabled" | "ignore" => Self::Disabled,
            _ => Self::Automatic,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Automatic => "auto",
            Self::Manual => "manual",
            Self::LinkLocal => "link-local",
            Self::Shared => "shared",
            Self::Disabled => "disabled",
        }
    }
}

/// What a connection is set to on one IP version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpSettings {
    pub method: IpMethod,
    /// Each as `address/prefix`; a bare address takes /24 (IPv4) or /64 (IPv6). Only
    /// [`IpMethod::Manual`] keeps them.
    pub addresses: Vec<String>,
    /// "" for none. Only [`IpMethod::Manual`] keeps it.
    pub gateway: String,
    /// The DNS servers, used with those the network gives.
    pub dns: Vec<String>,
}

/// The common settings of a saved connection, to read and to change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionSettings {
    pub name: String,
    pub autoconnect: bool,
    /// How a Wi-Fi connection is secured; [`Security::Open`] for the others.
    pub security: Security,
    /// A new key for a secured Wi-Fi network; `None` keeps the saved one. Reading always gives
    /// `None`: NetworkManager doesn't give saved keys out.
    pub key: Option<String>,
    pub ipv4: IpSettings,
    pub ipv6: IpSettings,
}

/// The networks. Cheap to copy; reading tracks, on any thread.
#[derive(Clone, Copy, Debug)]
pub struct Network(&'static Shared);

#[derive(Debug)]
struct Shared {
    available: SharedSignal<bool>,
    connectivity: SharedSignal<Connectivity>,
    wifi_enabled: SharedSignal<bool>,
    wifi: SharedSignal<Option<AccessPoint>>,
    access_points: SharedSignal<Vec<AccessPoint>>,
    wired: SharedSignal<Option<Wired>>,
    devices: SharedSignal<Vec<NetworkDevice>>,
    connections: SharedSignal<Vec<SavedConnection>>,
    /// The Wi-Fi device, and the saved connection of each SSID, for joining.
    wifi_device: Mutex<Option<String>>,
    saved: Mutex<HashMap<String, String>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// The networks. It starts following them the first time.
pub fn network() -> Network {
    Network(dbus::init_then_follow(&SHARED, follow, || Shared {
        available: shared_signal(false),
        connectivity: shared_signal(Connectivity::Unknown),
        wifi_enabled: shared_signal(false),
        wifi: shared_signal(None),
        access_points: shared_signal(Vec::new()),
        wired: shared_signal(None),
        devices: shared_signal(Vec::new()),
        connections: shared_signal(Vec::new()),
        wifi_device: Mutex::new(None),
        saved: Mutex::new(HashMap::new()),
    }))
}

impl Network {
    /// Whether NetworkManager is there.
    pub fn available(&self) -> bool {
        self.0.available.get()
    }

    pub fn connectivity(&self) -> Connectivity {
        self.0.connectivity.get()
    }

    /// Whether Wi-Fi is on, which turning airplane mode on turns off.
    pub fn wifi_enabled(&self) -> bool {
        self.0.wifi_enabled.get()
    }

    pub fn set_wifi_enabled(&self, on: bool) -> Task<Result<(), ServiceError>> {
        dbus::run(async move {
            let conn = dbus::system().await?;
            dbus::set_property(&conn, NM, ROOT, NM, "WirelessEnabled", Value::from(on)).await
        })
    }

    /// The access point Wi-Fi is connected to.
    pub fn wifi(&self) -> Option<AccessPoint> {
        self.0.wifi.get()
    }

    /// The Wi-Fi networks around, each once, the strongest first.
    pub fn access_points(&self) -> Vec<AccessPoint> {
        self.0.access_points.get()
    }

    /// Looks for networks again; what is found comes in [`Network::access_points`].
    pub fn scan(&self) -> Task<Result<(), ServiceError>> {
        let device = self.wifi_device();
        dbus::run(async move {
            let device = device.ok_or(ServiceError::Unavailable)?;
            let conn = dbus::system().await?;
            let options: HashMap<&str, Value> = HashMap::new();
            dbus::call(&conn, NM, &device, WIRELESS, "RequestScan", &(options,)).await
        })
    }

    /// Joins a network: with its saved connection if there is one, or else with `password`,
    /// saving it.
    pub fn connect(
        &self,
        ap: &AccessPoint,
        password: Option<String>,
    ) -> Task<Result<(), NetError>> {
        let device = self.wifi_device();
        let saved = self
            .0
            .saved
            .lock()
            .ok()
            .and_then(|s| s.get(&ap.ssid).cloned());
        let ap = ap.clone();
        let (task, done) = Task::pending();
        dbus::spawn(async move { done.complete(join(device, saved, ap, password).await) });
        task
    }

    /// Leaves the Wi-Fi network, staying off it until asked to join one.
    pub fn disconnect(&self) -> Task<Result<(), ServiceError>> {
        let device = self.wifi_device();
        dbus::run(async move {
            let device = device.ok_or(ServiceError::Unavailable)?;
            let conn = dbus::system().await?;
            dbus::call(&conn, NM, &device, DEVICE, "Disconnect", &()).await
        })
    }

    /// The first wired device.
    pub fn wired(&self) -> Option<Wired> {
        self.0.wired.get()
    }

    /// The network devices: wired ones first, then Wi-Fi, then the rest, each by its interface.
    pub fn devices(&self) -> Vec<NetworkDevice> {
        self.0.devices.get()
    }

    /// Lets a device connect, to its best saved connection, or disconnects it and keeps it off
    /// until asked again.
    pub fn set_device_enabled(
        &self,
        device: &NetworkDevice,
        on: bool,
    ) -> Task<Result<(), ServiceError>> {
        let device = device.path.clone();
        dbus::run(async move {
            let conn = dbus::system().await?;
            if !on {
                return dbus::call(&conn, NM, &device, DEVICE, "Disconnect", &()).await;
            }
            dbus::set_property(&conn, NM, &device, DEVICE, "Autoconnect", Value::from(true))
                .await?;
            let body = (path("/")?, path(&device)?, path("/")?);
            dbus::call::<_, OwnedObjectPath>(&conn, NM, ROOT, NM, "ActivateConnection", &body)
                .await
                .map(drop)
        })
    }

    /// The saved connections, by name.
    pub fn connections(&self) -> Vec<SavedConnection> {
        self.0.connections.get()
    }

    /// Connects a saved connection, on the device that suits it best.
    pub fn activate(&self, connection: &SavedConnection) -> Task<Result<(), NetError>> {
        let saved = connection.path.clone();
        let (task, done) = Task::pending();
        dbus::spawn(async move {
            let joined = async {
                let conn = dbus::system().await?;
                let body = (path(&saved)?, path("/")?, path("/")?);
                call_joining::<_, OwnedObjectPath>(&conn, "ActivateConnection", &body)
                    .await
                    .map(drop)
            };
            done.complete(joined.await);
        });
        task
    }

    /// Disconnects a saved connection that is in use. Its device doesn't connect by itself
    /// again until asked.
    pub fn deactivate(&self, connection: &SavedConnection) -> Task<Result<(), ServiceError>> {
        let active = connection.active.clone();
        dbus::run(async move {
            let Some(active) = active else {
                return Ok(());
            };
            let conn = dbus::system().await?;
            let body = (path(&active)?,);
            dbus::call(&conn, NM, ROOT, NM, "DeactivateConnection", &body).await
        })
    }

    /// Deletes a saved connection, disconnecting it first if it is in use.
    pub fn forget(&self, connection: &SavedConnection) -> Task<Result<(), ServiceError>> {
        let saved = connection.path.clone();
        dbus::run(async move {
            let conn = dbus::system().await?;
            dbus::call(&conn, NM, &saved, CONNECTION, "Delete", &()).await
        })
    }

    /// Whether a saved connection connects by itself.
    pub fn set_autoconnect(
        &self,
        connection: &SavedConnection,
        on: bool,
    ) -> Task<Result<(), ServiceError>> {
        let saved = connection.path.clone();
        dbus::run(async move {
            let conn = dbus::system().await?;
            let mut settings = read_settings(&conn, &saved).await?;
            settings
                .entry("connection".into())
                .or_default()
                .insert("autoconnect".into(), owned(Value::from(on))?);
            write_settings(&conn, &saved, settings).await
        })
    }

    /// The common settings of a saved connection.
    pub fn settings(
        &self,
        connection: &SavedConnection,
    ) -> Task<Result<ConnectionSettings, ServiceError>> {
        let saved = connection.path.clone();
        dbus::run(async move {
            let conn = dbus::system().await?;
            Ok(common_settings(&read_settings(&conn, &saved).await?))
        })
    }

    /// Changes the common settings of a saved connection, keeping the rest of them. A
    /// connection in use takes them when it connects again.
    pub fn save_settings(
        &self,
        connection: &SavedConnection,
        changed: ConnectionSettings,
    ) -> Task<Result<(), ServiceError>> {
        let saved = connection.path.clone();
        dbus::run(async move {
            let conn = dbus::system().await?;
            let mut settings = read_settings(&conn, &saved).await?;
            apply_settings(&mut settings, &changed)?;
            write_settings(&conn, &saved, settings).await
        })
    }

    /// Joins a Wi-Fi network that doesn't say its name, saving it. `password` is needed for
    /// any security but [`Security::Open`].
    pub fn join_hidden(
        &self,
        ssid: &str,
        security: Security,
        password: Option<String>,
    ) -> Task<Result<(), NetError>> {
        let device = self.wifi_device();
        let ssid = ssid.to_owned();
        let (task, done) = Task::pending();
        dbus::spawn(async move {
            let joined = async {
                let device = device.ok_or(ServiceError::Unavailable)?;
                let conn = dbus::system().await?;
                let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::from([
                    (
                        "connection",
                        HashMap::from([
                            ("id", Value::from(ssid.clone())),
                            ("type", Value::from("802-11-wireless")),
                        ]),
                    ),
                    (
                        "802-11-wireless",
                        HashMap::from([
                            ("ssid", Value::from(ssid.into_bytes())),
                            ("hidden", Value::from(true)),
                        ]),
                    ),
                ]);
                let secured = security_settings(security, password)?;
                if !secured.is_empty() {
                    settings.insert("802-11-wireless-security", secured);
                }
                let body = (settings, path(&device)?, path("/")?);
                call_joining::<_, (OwnedObjectPath, OwnedObjectPath)>(
                    &conn,
                    "AddAndActivateConnection",
                    &body,
                )
                .await
                .map(drop)
            };
            done.complete(joined.await);
        });
        task
    }

    fn wifi_device(&self) -> Option<String> {
        self.0.wifi_device.lock().ok().and_then(|d| d.clone())
    }
}

/// The settings of a connection, by section and name, as NetworkManager gives and takes them.
type Settings = HashMap<String, HashMap<String, OwnedValue>>;

fn owned(v: Value<'_>) -> Result<OwnedValue, ServiceError> {
    OwnedValue::try_from(v).map_err(|e| ServiceError::Failed(e.to_string()))
}

async fn read_settings(conn: &zbus::Connection, saved: &str) -> Result<Settings, ServiceError> {
    dbus::call(conn, NM, saved, CONNECTION, "GetSettings", &()).await
}

/// Writes all the settings of a connection, kept where they are now: on disk or in memory.
/// Secrets left out stay as they were.
async fn write_settings(
    conn: &zbus::Connection,
    saved: &str,
    settings: Settings,
) -> Result<(), ServiceError> {
    let args: HashMap<&str, Value> = HashMap::new();
    let body = (settings, 0u32, args);
    dbus::call::<_, HashMap<String, OwnedValue>>(conn, NM, saved, CONNECTION, "Update2", &body)
        .await
        .map(drop)
}

/// What a Wi-Fi connection's `802-11-wireless-security` section says.
fn security_of(settings: &Settings) -> Security {
    let Some(section) = settings.get("802-11-wireless-security") else {
        return Security::Open;
    };
    match string(section, "key-mgmt").as_str() {
        "none" => Security::Wep,
        "wpa-psk" => Security::Wpa,
        "sae" => Security::Wpa3,
        "wpa-eap" | "wpa-eap-suite-b-192" | "ieee8021x" => Security::Enterprise,
        _ => Security::Open,
    }
}

/// The `802-11-wireless-security` section that joins with `password`.
fn security_settings(
    security: Security,
    password: Option<String>,
) -> Result<HashMap<&'static str, Value<'static>>, NetError> {
    Ok(match (security, password) {
        (Security::Open, _) => HashMap::new(),
        (Security::Enterprise, _) => {
            return Err(NetError::Service(ServiceError::Unsupported));
        }
        (_, None) => return Err(NetError::NeedsSecret),
        (Security::Wep, Some(key)) => HashMap::from([
            ("key-mgmt", Value::from("none")),
            ("wep-key0", Value::from(key)),
            ("wep-key-type", Value::from(1u32)),
        ]),
        (Security::Wpa3, Some(psk)) => {
            HashMap::from([("key-mgmt", Value::from("sae")), ("psk", Value::from(psk))])
        }
        (Security::Wpa, Some(psk)) => HashMap::from([
            ("key-mgmt", Value::from("wpa-psk")),
            ("psk", Value::from(psk)),
        ]),
    })
}

/// `address-data`: each address with its prefix.
fn address_data(section: &Props, name: &str) -> Vec<String> {
    get::<Vec<HashMap<String, OwnedValue>>>(section, name)
        .unwrap_or_default()
        .iter()
        .filter_map(|a| {
            let address: String = get(a, "address")?;
            let prefix: u32 = get(a, "prefix").unwrap_or(0);
            Some(format!("{address}/{prefix}"))
        })
        .collect()
}

/// The DNS servers of a connection's `ipv4` or `ipv6` section: `dns-data` where
/// NetworkManager has it (1.48 on), else `dns` in its binary form.
fn dns_of(section: &Props, v6: bool) -> Vec<String> {
    if let Some(data) = get::<Vec<String>>(section, "dns-data") {
        return data;
    }
    if v6 {
        get::<Vec<Vec<u8>>>(section, "dns")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|b| <[u8; 16]>::try_from(b).ok())
            .map(|b| Ipv6Addr::from(b).to_string())
            .collect()
    } else {
        get::<Vec<u32>>(section, "dns")
            .unwrap_or_default()
            .into_iter()
            .map(|n| Ipv4Addr::from(n.to_ne_bytes()).to_string())
            .collect()
    }
}

fn ip_settings(settings: &Settings, section: &str, v6: bool) -> IpSettings {
    let Some(s) = settings.get(section) else {
        return IpSettings::default();
    };
    IpSettings {
        method: IpMethod::of(&string(s, "method")),
        addresses: address_data(s, "address-data"),
        gateway: string(s, "gateway"),
        dns: dns_of(s, v6),
    }
}

fn common_settings(settings: &Settings) -> ConnectionSettings {
    let connection = settings.get("connection");
    ConnectionSettings {
        name: connection.map(|c| string(c, "id")).unwrap_or_default(),
        autoconnect: connection
            .and_then(|c| get(c, "autoconnect"))
            .unwrap_or(true),
        security: security_of(settings),
        key: None,
        ipv4: ip_settings(settings, "ipv4", false),
        ipv6: ip_settings(settings, "ipv6", true),
    }
}

fn bad(what: &str, text: &str) -> ServiceError {
    ServiceError::Failed(format!("“{text}” is not {what}"))
}

/// An address of the version asked for.
fn ip_address(text: &str, v6: bool) -> Result<IpAddr, ServiceError> {
    let what = if v6 {
        "an IPv6 address"
    } else {
        "an IPv4 address"
    };
    match text.trim().parse::<IpAddr>() {
        Ok(a) if a.is_ipv6() == v6 => Ok(a),
        _ => Err(bad(what, text)),
    }
}

/// Writes `ip` into a connection's `ipv4` or `ipv6` section. The binary `addresses` and `dns`
/// go: NetworkManager reads `address-data`, and `dns` is written again from `ip.dns`.
fn apply_ip(section: &mut Props, ip: &IpSettings, v6: bool) -> Result<(), ServiceError> {
    let had_dns_data = section.contains_key("dns-data");
    for key in ["addresses", "address-data", "gateway", "dns", "dns-data"] {
        section.remove(key);
    }
    section.insert("method".into(), owned(Value::from(ip.method.name()))?);
    if ip.method == IpMethod::Manual {
        let mut data: Vec<HashMap<String, Value>> = Vec::new();
        for text in ip.addresses.iter().filter(|a| !a.trim().is_empty()) {
            let (address, prefix) = text.trim().split_once('/').unwrap_or((text.trim(), ""));
            let address = ip_address(address, v6)?;
            let longest = if v6 { 128 } else { 32 };
            let prefix: u32 = match prefix.trim() {
                "" if v6 => 64,
                "" => 24,
                p => p
                    .parse()
                    .ok()
                    .filter(|p| *p <= longest)
                    .ok_or_else(|| bad("a prefix", p))?,
            };
            data.push(HashMap::from([
                ("address".to_owned(), Value::from(address.to_string())),
                ("prefix".to_owned(), Value::from(prefix)),
            ]));
        }
        if data.is_empty() {
            return Err(ServiceError::Failed(
                "a manual configuration needs an address".into(),
            ));
        }
        section.insert("address-data".into(), owned(Value::from(data))?);
        if !ip.gateway.trim().is_empty() {
            let gateway = ip_address(&ip.gateway, v6)?;
            section.insert("gateway".into(), owned(Value::from(gateway.to_string()))?);
        }
    }
    if ip.method == IpMethod::Disabled {
        return Ok(());
    }
    let mut dns = Vec::new();
    for text in ip.dns.iter().filter(|d| !d.trim().is_empty()) {
        dns.push(ip_address(text, v6)?);
    }
    if dns.is_empty() {
        return Ok(());
    }
    let binary = if v6 {
        let bytes: Vec<Vec<u8>> = dns
            .iter()
            .filter_map(|a| match a {
                IpAddr::V6(a) => Some(a.octets().to_vec()),
                IpAddr::V4(_) => None,
            })
            .collect();
        owned(Value::from(bytes))?
    } else {
        let numbers: Vec<u32> = dns
            .iter()
            .filter_map(|a| match a {
                IpAddr::V4(a) => Some(u32::from_ne_bytes(a.octets())),
                IpAddr::V6(_) => None,
            })
            .collect();
        owned(Value::from(numbers))?
    };
    section.insert("dns".into(), binary);
    if had_dns_data {
        let texts: Vec<String> = dns.iter().map(IpAddr::to_string).collect();
        section.insert("dns-data".into(), owned(Value::from(texts))?);
    }
    Ok(())
}

fn apply_settings(
    settings: &mut Settings,
    changed: &ConnectionSettings,
) -> Result<(), ServiceError> {
    let connection = settings.entry("connection".into()).or_default();
    if !changed.name.trim().is_empty() {
        connection.insert("id".into(), owned(Value::from(changed.name.trim()))?);
    }
    connection.insert(
        "autoconnect".into(),
        owned(Value::from(changed.autoconnect))?,
    );
    // Only for the sections the connection has: a VPN without `ipv6` keeps none.
    if let Some(section) = settings.get_mut("ipv4") {
        apply_ip(section, &changed.ipv4, false)?;
    }
    if let Some(section) = settings.get_mut("ipv6") {
        apply_ip(section, &changed.ipv6, true)?;
    }
    let key = changed.key.as_deref().filter(|k| !k.is_empty());
    if let (Some(key), Some(section)) = (key, settings.get_mut("802-11-wireless-security")) {
        // Kept by NetworkManager rather than asked of an agent each time.
        let (name, flags) = if string(section, "key-mgmt") == "none" {
            ("wep-key0", "wep-key-flags")
        } else {
            ("psk", "psk-flags")
        };
        section.insert(name.into(), owned(Value::from(key))?);
        section.insert(flags.into(), owned(Value::from(0u32))?);
    }
    Ok(())
}

/// Joins `ap` on `device`, with a saved connection or the password.
async fn join(
    device: Option<String>,
    saved: Option<String>,
    ap: AccessPoint,
    password: Option<String>,
) -> Result<(), NetError> {
    let device = device.ok_or(ServiceError::Unavailable)?;
    let conn = dbus::system().await?;
    if let Some(saved) = saved {
        let body = (path(&saved)?, path(&device)?, path(&ap.path)?);
        return call_joining::<_, OwnedObjectPath>(&conn, "ActivateConnection", &body)
            .await
            .map(drop);
    }
    let security = security_settings(ap.security, password)?;
    let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
    if !security.is_empty() {
        settings.insert("802-11-wireless-security", security);
    }
    let body = (settings, path(&device)?, path(&ap.path)?);
    call_joining::<_, (OwnedObjectPath, OwnedObjectPath)>(&conn, "AddAndActivateConnection", &body)
        .await
        .map(drop)
}

fn path(s: &str) -> Result<ObjectPath<'_>, ServiceError> {
    ObjectPath::try_from(s).map_err(|e| ServiceError::Failed(e.to_string()))
}

/// Calls a method of NetworkManager that joins a network, telling a missing password from the
/// rest of what can go wrong.
async fn call_joining<B, R>(conn: &zbus::Connection, method: &str, body: &B) -> Result<R, NetError>
where
    B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = match conn
        .call_method(Some(NM), ROOT, Some(NM), method, body)
        .await
    {
        Ok(reply) => reply,
        Err(zbus::Error::MethodError(name, ..)) if name.as_str().contains("NoSecrets") => {
            return Err(NetError::NeedsSecret);
        }
        Err(e) => return Err(dbus::failed(e).into()),
    };
    reply
        .body()
        .deserialize()
        .map_err(|e| NetError::Service(dbus::failed(e)))
}

/// What NetworkManager has told so far.
#[derive(Default)]
struct Model {
    root: Props,
    /// Devices by path, with the properties of each of their interfaces.
    devices: BTreeMap<String, Props>,
    access_points: BTreeMap<String, Props>,
    /// The connections in use, by path.
    active: BTreeMap<String, Props>,
    /// The IPv4 and IPv6 configurations of the devices, by path.
    ip: BTreeMap<String, Props>,
    /// The saved connections, by path.
    saved: BTreeMap<String, Saved>,
}

/// What a saved connection's settings say, as far as the list of them shows.
#[derive(Clone, Default)]
struct Saved {
    name: String,
    uuid: String,
    type_name: String,
    ssid: Option<String>,
    autoconnect: bool,
    interface: String,
    last_used: u64,
}

impl Saved {
    fn of(settings: &Settings) -> Self {
        let empty = Props::new();
        let c = settings.get("connection").unwrap_or(&empty);
        Saved {
            name: string(c, "id"),
            uuid: string(c, "uuid"),
            type_name: string(c, "type"),
            ssid: settings
                .get("802-11-wireless")
                .and_then(|w| get::<Vec<u8>>(w, "ssid"))
                .map(|s| String::from_utf8_lossy(&s).into_owned()),
            autoconnect: get(c, "autoconnect").unwrap_or(true),
            interface: string(c, "interface-name"),
            last_used: get(c, "timestamp").unwrap_or(0),
        }
    }
}

/// The path a property holds, unless it is `/`, which is none.
fn object(p: &Props, name: &str) -> Option<String> {
    get::<OwnedObjectPath>(p, name)
        .map(|p| p.to_string())
        .filter(|p| p != "/")
}

/// What an `IP4Config` or `IP6Config` says.
fn ip_config(p: Option<&Props>, v6: bool) -> IpConfig {
    let Some(p) = p else {
        return IpConfig::default();
    };
    let dns = if v6 {
        get::<Vec<Vec<u8>>>(p, "Nameservers")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|b| <[u8; 16]>::try_from(b).ok())
            .map(|b| Ipv6Addr::from(b).to_string())
            .collect()
    } else {
        get::<Vec<HashMap<String, OwnedValue>>>(p, "NameserverData")
            .unwrap_or_default()
            .iter()
            .filter_map(|d| get::<String>(d, "address"))
            .collect()
    };
    IpConfig {
        addresses: address_data(p, "AddressData"),
        gateway: string(p, "Gateway"),
        dns,
    }
}

fn paths(p: &Props, name: &str) -> Vec<String> {
    get::<Vec<OwnedObjectPath>>(p, name)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect()
}

fn device_type(p: &Props) -> u32 {
    get(p, "DeviceType").unwrap_or(0)
}

impl Model {
    fn wifi_devices(&self) -> impl Iterator<Item = (&String, &Props)> {
        self.devices.iter().filter(|(_, p)| device_type(p) == WIFI)
    }

    fn access_point(&self, path: &str, active: &[String]) -> Option<AccessPoint> {
        let p = self.access_points.get(path)?;
        let ssid =
            String::from_utf8_lossy(&get::<Vec<u8>>(p, "Ssid").unwrap_or_default()).into_owned();
        if ssid.is_empty() {
            return None;
        }
        Some(AccessPoint {
            known: self
                .saved
                .values()
                .any(|s| s.ssid.as_deref() == Some(ssid.as_str())),
            active: active.iter().any(|a| a == path),
            ssid,
            strength: get(p, "Strength").unwrap_or(0),
            frequency: get(p, "Frequency").unwrap_or(0),
            security: Security::of(p),
            path: path.to_owned(),
        })
    }

    fn publish(&self) {
        let Some(s) = SHARED.get() else {
            return;
        };
        put(&s.available, !self.root.is_empty());
        put(
            &s.connectivity,
            Connectivity::of(get(&self.root, "Connectivity").unwrap_or(0)),
        );
        put(
            &s.wifi_enabled,
            get(&self.root, "WirelessEnabled").unwrap_or(false),
        );
        let active: Vec<String> = self
            .wifi_devices()
            .filter_map(|(_, p)| get::<OwnedObjectPath>(p, "ActiveAccessPoint"))
            .map(|p| p.to_string())
            .filter(|p| p != "/")
            .collect();
        put(
            &s.wifi,
            active.first().and_then(|a| self.access_point(a, &active)),
        );
        // Each SSID once: the strongest of its access points, active if any of them is.
        let mut by_ssid: BTreeMap<String, AccessPoint> = BTreeMap::new();
        for (_, p) in self.wifi_devices() {
            for path in paths(p, "AccessPoints") {
                let Some(ap) = self.access_point(&path, &active) else {
                    continue;
                };
                match by_ssid.get_mut(&ap.ssid) {
                    Some(kept) => {
                        let active = kept.active || ap.active;
                        if ap.strength > kept.strength {
                            *kept = ap;
                        }
                        kept.active = active;
                    }
                    None => {
                        by_ssid.insert(ap.ssid.clone(), ap);
                    }
                }
            }
        }
        let mut list: Vec<AccessPoint> = by_ssid.into_values().collect();
        list.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.ssid.cmp(&b.ssid)));
        put(&s.access_points, list);
        put(
            &s.wired,
            self.devices
                .values()
                .find(|p| device_type(p) == ETHERNET)
                .map(|p| Wired {
                    interface: string(p, "Interface"),
                    connected: get::<u32>(p, "State") == Some(ACTIVATED),
                    speed: get(p, "Speed").unwrap_or(0),
                }),
        );
        if let Ok(mut d) = s.wifi_device.lock() {
            *d = self.wifi_devices().next().map(|(path, _)| path.clone());
        }
        if let Ok(mut saved) = s.saved.lock() {
            *saved = self
                .saved
                .iter()
                .filter_map(|(path, c)| Some((c.ssid.clone()?, path.clone())))
                .collect();
        }
        put(&s.devices, self.network_devices());
        put(&s.connections, self.connections());
    }

    fn network_devices(&self) -> Vec<NetworkDevice> {
        let mut list: Vec<NetworkDevice> = self
            .devices
            .iter()
            .map(|(path, p)| {
                let kind = DeviceKind::of(device_type(p));
                let speed = match kind {
                    DeviceKind::Wifi => get::<u32>(p, "Bitrate").unwrap_or(0) / 1000,
                    _ => get(p, "Speed").unwrap_or(0),
                };
                let ip = |name: &str| object(p, name).and_then(|c| self.ip.get(&c));
                NetworkDevice {
                    interface: string(p, "Interface"),
                    kind,
                    state: DeviceState::of(get(p, "State").unwrap_or(0)),
                    hardware_address: string(p, "HwAddress"),
                    speed,
                    autoconnect: get(p, "Autoconnect").unwrap_or(false),
                    connection: object(p, "ActiveConnection")
                        .and_then(|a| self.active.get(&a))
                        .map(|a| string(a, "Id")),
                    ipv4: ip_config(ip("Ip4Config"), false),
                    ipv6: ip_config(ip("Ip6Config"), true),
                    path: path.clone(),
                }
            })
            .collect();
        let order = |k: DeviceKind| match k {
            DeviceKind::Ethernet => 0,
            DeviceKind::Wifi => 1,
            DeviceKind::Other => 2,
        };
        list.sort_by(|a, b| {
            order(a.kind)
                .cmp(&order(b.kind))
                .then_with(|| a.interface.cmp(&b.interface))
        });
        list
    }

    fn connections(&self) -> Vec<SavedConnection> {
        let mut list: Vec<SavedConnection> = self
            .saved
            .iter()
            .map(|(path, c)| {
                let active = self
                    .active
                    .iter()
                    .find(|(_, a)| object(a, "Connection").as_deref() == Some(path.as_str()));
                SavedConnection {
                    name: c.name.clone(),
                    uuid: c.uuid.clone(),
                    kind: ConnectionKind::of(&c.type_name),
                    type_name: c.type_name.clone(),
                    ssid: c.ssid.clone(),
                    autoconnect: c.autoconnect,
                    state: active.map_or(ActivationState::Inactive, |(_, a)| {
                        ActivationState::of(get(a, "State").unwrap_or(0))
                    }),
                    interface: c.interface.clone(),
                    last_used: c.last_used,
                    path: path.clone(),
                    active: active.map(|(a, _)| a.clone()),
                }
            })
            .collect();
        list.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.uuid.cmp(&b.uuid))
        });
        list
    }

    /// Reads the devices and access points it doesn't have yet, and forgets those that went.
    async fn sync(&mut self, conn: &zbus::Connection) {
        let devices = paths(&self.root, "Devices");
        self.devices.retain(|path, _| devices.contains(path));
        for path in devices {
            if self.devices.contains_key(&path) {
                continue;
            }
            let Some(mut p) = dbus::get_all(conn, NM, &path, DEVICE).await else {
                continue;
            };
            let more = match device_type(&p) {
                WIFI => Some(WIRELESS),
                ETHERNET => Some(WIRED),
                _ => None,
            };
            if let Some(interface) = more
                && let Some(more) = dbus::get_all(conn, NM, &path, interface).await
            {
                p.extend(more);
            }
            self.devices.insert(path, p);
        }
        let access_points: Vec<String> = self
            .wifi_devices()
            .flat_map(|(_, p)| paths(p, "AccessPoints"))
            .collect();
        self.access_points
            .retain(|path, _| access_points.contains(path));
        for path in access_points {
            if self.access_points.contains_key(&path) {
                continue;
            }
            if let Some(p) = dbus::get_all(conn, NM, &path, ACCESS_POINT).await {
                self.access_points.insert(path, p);
            }
        }
        let active = paths(&self.root, "ActiveConnections");
        self.active.retain(|path, _| active.contains(path));
        for path in active {
            if self.active.contains_key(&path) {
                continue;
            }
            if let Some(p) = dbus::get_all(conn, NM, &path, ACTIVE).await {
                self.active.insert(path, p);
            }
        }
        let configs: Vec<(String, &str)> = self
            .devices
            .values()
            .flat_map(|p| {
                [
                    object(p, "Ip4Config").map(|c| (c, IP4)),
                    object(p, "Ip6Config").map(|c| (c, IP6)),
                ]
            })
            .flatten()
            .collect();
        self.ip
            .retain(|path, _| configs.iter().any(|(c, _)| c == path));
        for (path, interface) in configs {
            if self.ip.contains_key(&path) {
                continue;
            }
            if let Some(p) = dbus::get_all(conn, NM, &path, interface).await {
                self.ip.insert(path, p);
            }
        }
    }

    async fn read_saved(&mut self, conn: &zbus::Connection, path: &str) {
        match read_settings(conn, path).await {
            Ok(settings) => {
                self.saved.insert(path.to_owned(), Saved::of(&settings));
            }
            Err(_) => {
                self.saved.remove(path);
            }
        }
    }
}

async fn follow() {
    let Ok(conn) = dbus::system().await else {
        return;
    };
    dbus::while_owned(&conn, NM, || serve(&conn), || Model::default().publish()).await;
}

async fn serve(conn: &zbus::Connection) {
    // Signals first, so that nothing is missed between reading and following.
    let Some(mut signals) = dbus::signals(conn, ROOT, None).await else {
        return;
    };
    let mut model = Model {
        root: dbus::get_all(conn, NM, ROOT, NM).await.unwrap_or_default(),
        ..Default::default()
    };
    let saved: Vec<OwnedObjectPath> =
        dbus::call(conn, NM, SETTINGS, SETTINGS_IFACE, "ListConnections", &())
            .await
            .unwrap_or_default();
    for path in saved {
        model.read_saved(conn, &path).await;
    }
    model.sync(conn).await;
    model.publish();
    while let Some(Ok(msg)) = signals.next().await {
        let Some((path, interface, member)) = dbus::signal_of(&msg) else {
            continue;
        };
        if let Some((_, changed, _)) = dbus::properties_changed(&msg) {
            let props = if path == ROOT {
                &mut model.root
            } else if let Some(p) = model.devices.get_mut(&path) {
                p
            } else if let Some(p) = model.access_points.get_mut(&path) {
                p
            } else if let Some(p) = model.active.get_mut(&path) {
                p
            } else if let Some(p) = model.ip.get_mut(&path) {
                p
            } else {
                continue;
            };
            props.extend(changed);
        } else if interface == SETTINGS_IFACE
            && (member == "NewConnection" || member == "ConnectionRemoved")
        {
            let Ok(saved) = msg.body().deserialize::<OwnedObjectPath>() else {
                continue;
            };
            if member == "ConnectionRemoved" {
                model.saved.remove(saved.as_str());
            } else {
                model.read_saved(conn, &saved).await;
            }
        } else if interface == CONNECTION && member == "Updated" {
            model.read_saved(conn, &path).await;
        } else if !(interface == NM && (member == "DeviceAdded" || member == "DeviceRemoved")
            || interface == WIRELESS
                && (member == "AccessPointAdded" || member == "AccessPointRemoved"))
        {
            continue;
        }
        // The lists in the properties say what is there; the signals only say when to look.
        if let Ok(added) = msg.body().deserialize::<OwnedObjectPath>() {
            if member == "AccessPointAdded"
                && let Some(p) = model.devices.get_mut(&path)
            {
                let mut list: Vec<OwnedObjectPath> = get(p, "AccessPoints").unwrap_or_default();
                if !list.contains(&added) {
                    list.push(added);
                    if let Ok(v) = OwnedValue::try_from(Value::from(list)) {
                        p.insert("AccessPoints".into(), v);
                    }
                }
            } else if member == "DeviceAdded" {
                let mut list: Vec<OwnedObjectPath> =
                    get(&model.root, "Devices").unwrap_or_default();
                if !list.contains(&added) {
                    list.push(added);
                    if let Ok(v) = OwnedValue::try_from(Value::from(list)) {
                        model.root.insert("Devices".into(), v);
                    }
                }
            }
        }
        model.sync(conn).await;
        model.publish();
    }
}
