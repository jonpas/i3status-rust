use std::ffi::OsStr;
use std::fmt;
use std::net::Ipv4Addr;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use chan::Sender;
use uuid::Uuid;

use block::{Block, ConfigBlock};
use blocks::dbus::arg::{Array, Iter, Variant};
use blocks::dbus::{BusType, Connection, ConnectionItem, Message, MessageItem, Path};
use config::Config;
use errors::*;
use input::{I3BarEvent, MouseButton};
use scheduler::Task;
use widget::{I3BarWidget, State};
use widgets::button::ButtonWidget;

enum NetworkState {
    Unknown,
    Asleep,
    Disconnected,
    Disconnecting,
    Connecting,
    ConnectedLocal,
    ConnectedSite,
    ConnectedGlobal,
}

impl From<u32> for NetworkState {
    fn from(id: u32) -> Self {
        match id {
            // https://developer.gnome.org/NetworkManager/stable/nm-dbus-types.html#NMState
            10 => NetworkState::Asleep,
            20 => NetworkState::Disconnected,
            30 => NetworkState::Disconnecting,
            40 => NetworkState::Connecting,
            50 => NetworkState::ConnectedLocal,
            60 => NetworkState::ConnectedSite,
            70 => NetworkState::ConnectedGlobal,
            _ => NetworkState::Unknown,
        }
    }
}

enum ActiveConnectionState {
    Unknown,
    Activating,
    Activated,
    Deactivating,
    Deactivated,
}

impl From<u32> for ActiveConnectionState {
    fn from(id: u32) -> Self {
        match id {
            // https://developer.gnome.org/NetworkManager/stable/nm-dbus-types.html#NMActiveConnectionState
            1 => ActiveConnectionState::Activating,
            2 => ActiveConnectionState::Activated,
            3 => ActiveConnectionState::Deactivating,
            4 => ActiveConnectionState::Deactivated,
            _ => ActiveConnectionState::Unknown,
        }
    }
}

impl ActiveConnectionState {
    fn to_state(&self, good: &State) -> State {
        match self {
            ActiveConnectionState::Activated => good.clone(),
            ActiveConnectionState::Activating => State::Warning,
            ActiveConnectionState::Deactivating => State::Warning,
            ActiveConnectionState::Deactivated => State::Critical,
            ActiveConnectionState::Unknown => State::Critical,
        }
    }
}

#[derive(Debug)]
enum DeviceType {
    Unknown,
    Ethernet,
    Wifi,
    Modem,
    Bridge,
    TUN,
    Wireguard,
}

impl From<u32> for DeviceType {
    fn from(id: u32) -> Self {
        match id {
            // https://developer.gnome.org/NetworkManager/stable/nm-dbus-types.html#NMDeviceType
            1 => DeviceType::Ethernet,
            2 => DeviceType::Wifi,
            8 => DeviceType::Modem,
            13 => DeviceType::Bridge,
            16 => DeviceType::TUN,
            29 => DeviceType::Wireguard,
            _ => DeviceType::Unknown,
        }
    }
}

impl DeviceType {
    fn to_icon_name(&self) -> Option<String> {
        match self {
            DeviceType::Ethernet => Some("net_wired".to_string()),
            DeviceType::Wifi => Some("net_wireless".to_string()),
            DeviceType::Modem => Some("net_modem".to_string()),
            DeviceType::Bridge => Some("net_bridge".to_string()),
            DeviceType::TUN => Some("net_bridge".to_string()),
            DeviceType::Wireguard => Some("net_vpn".to_string()),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Ipv4Address {
    address: Ipv4Addr,
    prefix: u32,
    gateway: Ipv4Addr,
}

trait ByteOrderSwap {
    fn swap(&self) -> Self;
}

impl ByteOrderSwap for u32 {
    fn swap(&self) -> u32 {
        ((self & 0x000000FF) << 24) | ((self & 0x0000FF00) << 8) | ((self & 0x00FF0000) >> 8) | ((self & 0xFF000000) >> 24)
    }
}

impl<'a> From<Array<'a, u32, Iter<'a>>> for Ipv4Address {
    fn from(s: Array<'a, u32, Iter<'a>>) -> Ipv4Address {
        let mut i = s.into_iter();
        Ipv4Address {
            address: Ipv4Addr::from(i.next().unwrap().swap()),
            prefix: i.next().unwrap(),
            gateway: Ipv4Addr::from(i.next().unwrap().swap()),
        }
    }
}

impl fmt::Display for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

struct ConnectionManager {}

impl ConnectionManager {
    pub fn new() -> Self {
        ConnectionManager {}
    }

    fn get(c: &Connection, path: Path, t: &str, property: &str) -> Result<Message> {
        let m = Message::new_method_call("org.freedesktop.NetworkManager", path, "org.freedesktop.DBus.Properties", "Get")
            .block_error("networkmanager", "Failed to create message")?
            .append2(MessageItem::Str(t.to_string()), MessageItem::Str(property.to_string()));

        let r = c.send_with_reply_and_block(m, 1000);

        r.block_error("networkmanager", "Failed to retrieve property")
    }

    fn get_property(c: &Connection, property: &str) -> Result<Message> {
        Self::get(c, "/org/freedesktop/NetworkManager".into(), "org.freedesktop.NetworkManager", property)
    }

    pub fn state(&self, c: &Connection) -> Result<NetworkState> {
        let m = Self::get_property(c, "State").block_error("networkmanager", "Failed to retrieve state")?;

        let state: Variant<u32> = m.get1().block_error("networkmanager", "Failed to read property")?;

        Ok(NetworkState::from(state.0))
    }

    pub fn primary_connection(&self, c: &Connection) -> Result<NmConnection> {
        let m = Self::get_property(c, "PrimaryConnection").block_error("networkmanager", "Failed to retrieve primary connection")?;

        let primary_connection: Variant<Path> = m.get1().block_error("networkmanager", "Failed to read primary connection")?;

        if let Ok(conn) = primary_connection.0.as_cstr().to_str() {
            if conn == "/" {
                return Err(BlockError("networkmanager".to_string(), "No primary connection".to_string()));
            }
        }

        Ok(NmConnection { path: primary_connection.0.clone() })
    }

    pub fn active_connections(&self, c: &Connection) -> Result<Vec<NmConnection>> {
        let m = Self::get_property(c, "ActiveConnections").block_error("networkmanager", "Failed to retrieve active connections")?;

        let active_connections: Variant<Array<Path, Iter>> = m.get1().block_error("networkmanager", "Failed to read active connections")?;

        Ok(active_connections.0.into_iter().map(|x| NmConnection { path: x }).collect())
    }
}

#[derive(Clone)]
struct NmConnection<'a> {
    path: Path<'a>,
}

impl<'a> NmConnection<'a> {
    fn state(&self, c: &Connection) -> Result<ActiveConnectionState> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.Connection.Active", "State").block_error("networkmanager", "Failed to retrieve connection state")?;

        let state: Variant<u32> = m.get1().block_error("networkmanager", "Failed to read connection state")?;
        Ok(ActiveConnectionState::from(state.0))
    }

    fn ip4config(&self, c: &Connection) -> Result<NmIp4Config> {
        let m =
            ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.Connection.Active", "Ip4Config").block_error("networkmanager", "Failed to retrieve connection ip4config")?;

        let ip4config: Variant<Path> = m.get1().block_error("networkmanager", "Failed to read ip4config")?;
        Ok(NmIp4Config { path: ip4config.0 })
    }

    fn devices(&self, c: &Connection) -> Result<Vec<NmDevice>> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.Connection.Active", "Devices").block_error("networkmanager", "Failed to retrieve connection device")?;

        let devices: Variant<Array<Path, Iter>> = m.get1().block_error("networkmanager", "Failed to read devices")?;
        Ok(devices.0.into_iter().map(|x| NmDevice { path: x }).collect())
    }
}

#[derive(Clone)]
struct NmDevice<'a> {
    path: Path<'a>,
}

impl<'a> NmDevice<'a> {
    fn device_type(&self, c: &Connection) -> Result<DeviceType> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.Device", "DeviceType").block_error("networkmanager", "Failed to retrieve device type")?;

        let device_type: Variant<u32> = m.get1().block_error("networkmanager", "Failed to read device type")?;
        Ok(DeviceType::from(device_type.0))
    }

    fn active_access_point(&self, c: &Connection) -> Result<NmAccessPoint> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.Device.Wireless", "ActiveAccessPoint")
            .block_error("networkmanager", "Failed to retrieve device active access point")?;

        let active_ap: Variant<Path> = m.get1().block_error("networkmanager", "Failed to read active access point")?;
        Ok(NmAccessPoint { path: active_ap.0 })
    }
}

#[derive(Clone)]
struct NmAccessPoint<'a> {
    path: Path<'a>,
}

impl<'a> NmAccessPoint<'a> {
    fn ssid(&self, c: &Connection) -> Result<String> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.AccessPoint", "Ssid").block_error("networkmanager", "Failed to retrieve SSID")?;

        let ssid: Variant<Array<u8, Iter>> = m.get1().block_error("networkmanager", "Failed to read ssid")?;
        Ok(std::str::from_utf8(&ssid.0.into_iter().collect::<Vec<u8>>())
            .block_error("networkmanager", "Failed to parse ssid")?
            .to_string())
    }
}

#[derive(Clone)]
struct NmIp4Config<'a> {
    path: Path<'a>,
}

impl<'a> NmIp4Config<'a> {
    fn addresses(&self, c: &Connection) -> Result<Vec<Ipv4Address>> {
        let m = ConnectionManager::get(c, self.path.clone(), "org.freedesktop.NetworkManager.IP4Config", "Addresses").block_error("networkmanager", "Failed to retrieve addresses")?;

        let addresses: Variant<Array<Array<u32, Iter>, Iter>> = m.get1().block_error("networkmanager", "Failed to read addresses")?;
        Ok(addresses.0.into_iter().map(|addr| Ipv4Address::from(addr)).collect())
    }
}

pub struct NetworkManager {
    id: String,
    indicator: ButtonWidget,
    output: Vec<ButtonWidget>,
    dbus_conn: Connection,
    manager: ConnectionManager,
    config: Config,
    on_click: Option<String>,
    primary_only: bool,
    unknown_device_icon: bool,
    ip: bool,
    ssid: bool,
    max_ssid_width: usize,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetworkManagerConfig {
    #[serde(default = "NetworkManagerConfig::default_on_click")]
    pub on_click: Option<String>,

    /// Whether to only show the primary connection, or all active connections.
    #[serde(default = "NetworkManagerConfig::default_primary_only")]
    pub primary_only: bool,

    /// Whether to show an unknown device icon instead of name for unknown devices.
    #[serde(default = "NetworkManagerConfig::default_unknown_device_icon")]
    pub unknown_device_icon: bool,

    /// Whether to show the IP address of active networks.
    #[serde(default = "NetworkManagerConfig::default_ip")]
    pub ip: bool,

    /// Whether to show the SSID of active wireless networks.
    #[serde(default = "NetworkManagerConfig::default_ssid")]
    pub ssid: bool,

    /// Max SSID width, in characters.
    #[serde(default = "NetworkManagerConfig::default_max_ssid_width")]
    pub max_ssid_width: usize,
}

impl NetworkManagerConfig {
    fn default_on_click() -> Option<String> {
        None
    }

    fn default_primary_only() -> bool {
        false
    }

    fn default_unknown_device_icon() -> bool {
        false
    }

    fn default_ip() -> bool {
        true
    }

    fn default_ssid() -> bool {
        true
    }

    fn default_max_ssid_width() -> usize {
        21
    }
}

impl ConfigBlock for NetworkManager {
    type Config = NetworkManagerConfig;

    fn new(block_config: Self::Config, config: Config, send: Sender<Task>) -> Result<Self> {
        let id: String = Uuid::new_v4().simple().to_string();
        let id_copy = id.clone();
        let dbus_conn = Connection::get_private(BusType::System).block_error("networkmanager", "failed to establish D-Bus connection")?;
        let manager = ConnectionManager::new();

        thread::spawn(move || {
            let c = Connection::get_private(BusType::System).unwrap();
            let rule = "type='signal',\
                        path='/org/freedesktop/NetworkManager',\
                        interface='org.freedesktop.NetworkManager',\
                        member='PropertiesChanged'";

            c.add_match(&rule).unwrap();

            loop {
                let timeout = 300_000;

                for event in c.iter(timeout) {
                    match event {
                        ConnectionItem::Nothing => (),
                        _ => send.send(Task {
                            id: id_copy.clone(),
                            update_time: Instant::now(),
                        }),
                    }
                }
            }
        });

        Ok(NetworkManager {
            id: id.clone(),
            config: config.clone(),
            indicator: ButtonWidget::new(config.clone(), &id),
            output: Vec::new(),
            dbus_conn,
            manager,
            on_click: block_config.on_click,
            primary_only: block_config.primary_only,
            unknown_device_icon: block_config.unknown_device_icon,
            ip: block_config.ip,
            ssid: block_config.ssid,
            max_ssid_width: block_config.max_ssid_width,
        })
    }
}

impl Block for NetworkManager {
    fn id(&self) -> &str {
        &self.id
    }

    fn update(&mut self) -> Result<Option<Duration>> {
        let state = self.manager.state(&self.dbus_conn);

        self.indicator.set_state(match state {
            Ok(NetworkState::ConnectedGlobal) => State::Good,
            Ok(NetworkState::ConnectedSite) => State::Info,
            Ok(NetworkState::ConnectedLocal) => State::Idle,
            Ok(NetworkState::Connecting) => State::Warning,
            Ok(NetworkState::Disconnecting) => State::Warning,
            _ => State::Critical,
        });
        self.indicator.set_text(match state {
            Ok(NetworkState::Disconnected) => "×",
            Ok(NetworkState::Asleep) => "×",
            Ok(NetworkState::Unknown) => "E",
            _ => "",
        });

        self.output = match state {
            // It would be a waste of time to bother NetworkManager in any of these states
            Ok(NetworkState::Disconnected) | Ok(NetworkState::Asleep) | Ok(NetworkState::Unknown) => vec![],

            _ => {
                let good_state = match state {
                    Ok(NetworkState::ConnectedGlobal) => State::Good,
                    Ok(NetworkState::ConnectedSite) => State::Info,
                    _ => State::Idle,
                };

                let connections = if self.primary_only {
                    match self.manager.primary_connection(&self.dbus_conn) {
                        Ok(conn) => vec![conn],
                        Err(_) => vec![],
                    }
                } else {
                    // We sort things so that the primary connection comes first
                    let active = self.manager.active_connections(&self.dbus_conn).unwrap_or_else(|_| Vec::new());
                    match self.manager.primary_connection(&self.dbus_conn) {
                        Ok(conn) => vec![conn.clone()].into_iter().chain(active.into_iter().filter(|ref x| x.path != conn.path)).collect(),
                        Err(_) => active,
                    }
                };

                connections
                    .into_iter()
                    .map(|conn| {
                        let mut widget = ButtonWidget::new(self.config.clone(), &self.id);

                        // Set the state for this connection
                        widget.set_state(if let Ok(conn_state) = conn.state(&self.dbus_conn) {
                            conn_state.to_state(&good_state)
                        } else {
                            ActiveConnectionState::Unknown.to_state(&good_state)
                        });

                        // Get all devices for this connection
                        let mut devicevec: Vec<String> = Vec::new();
                        if let Ok(devices) = conn.devices(&self.dbus_conn) {
                            for device in devices {
                                let iconstr = if let Ok(dev_type) = device.device_type(&self.dbus_conn) {
                                    match dev_type.to_icon_name() {
                                        Some(icon_name) => self.config.icons.get(&icon_name).cloned().unwrap_or("".to_string()),
                                        None => {
                                            if self.unknown_device_icon {
                                                self.config.icons.get("unknown").cloned().unwrap_or("".to_string())
                                            } else {
                                                format!("{:?}", dev_type).to_string()
                                            }
                                        }
                                    }
                                } else {
                                    "".to_string()
                                };

                                let mut ssidstr = "".to_string();
                                if self.ssid {
                                    if let Ok(ap) = device.active_access_point(&self.dbus_conn) {
                                        if let Ok(ssid) = ap.ssid(&self.dbus_conn) {
                                            let mut truncated = ssid.to_string();
                                            truncated.truncate(self.max_ssid_width);
                                            ssidstr = truncated + " ";
                                        }
                                    }
                                }

                                devicevec.push(iconstr + &ssidstr);
                            }
                        };

                        // Get all IPs for this connection
                        let ip = if self.ip {
                            let mut ip = "×".to_string();
                            if let Ok(ip4config) = conn.ip4config(&self.dbus_conn) {
                                if let Ok(addresses) = ip4config.addresses(&self.dbus_conn) {
                                    if addresses.len() > 0 {
                                        ip = addresses.into_iter().map(|x| x.to_string()).collect::<Vec<String>>().join(",")
                                    }
                                }
                            }
                            ip
                        } else {
                            "".to_string()
                        };

                        widget.set_text(devicevec.join(" ") + &ip);

                        widget
                    })
                    .collect()
            }
        };

        Ok(None)
    }

    fn view(&self) -> Vec<&I3BarWidget> {
        if self.output.len() == 0 {
            vec![&self.indicator]
        } else {
            self.output.iter().map(|x| x as &I3BarWidget).collect()
        }
    }

    fn click(&mut self, e: &I3BarEvent) -> Result<()> {
        if let Some(ref name) = e.name {
            if name.as_str() == self.id {
                match e.button {
                    MouseButton::Left => {
                        if let Some(ref cmd) = self.on_click {
                            let command_broken: Vec<&str> = cmd.split_whitespace().collect();
                            let mut itr = command_broken.iter();
                            let mut _cmd = Command::new(OsStr::new(&itr.next().unwrap())).args(itr).spawn();
                        }
                    }
                    _ => (),
                }
            }
        }

        Ok(())
    }
}
