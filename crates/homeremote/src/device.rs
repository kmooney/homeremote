//! The vocabulary every backend speaks.
//!
//! Deliberately a superset rather than a lowest common denominator: a device
//! that cannot honour a command returns [`Error::Unsupported`] instead of the
//! trait being narrowed to what all three share.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::Result;

/// Stable identity for a device, independent of its current IP.
///
/// Derived from something the device carries with it: the Denon's serial, the
/// TV's MAC, the Apple TV's mDNS identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    /// The Denon AVR: owns volume, owns input switching.
    Receiver,
    /// The Samsung set: owns display power and its own input.
    Television,
    /// The Apple TV: owns navigation and transport.
    StreamingBox,
    /// The ATOM that turns requests into IR for a device with no IP control.
    IrBridge,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub power: bool,
    pub volume: bool,
    pub input: bool,
    pub nav: bool,
    pub transport: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerState {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeCmd {
    /// Absolute level, 0.0–1.0. Backends map this onto their own scale.
    Set(f32),
    Up,
    Down,
    Mute(bool),
    ToggleMute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NavKey {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    Home,
    Menu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKey {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Command {
    Power(PowerState),
    Volume(VolumeCmd),
    /// Backend-specific input name, e.g. the Denon's `MPLAY` or the TV's `HDMI1`.
    Input(String),
    Nav(NavKey),
    Transport(TransportKey),
    /// Replace what the device's focused text field holds.
    Text(String),
    /// Escape hatch: a raw protocol string, passed through untouched.
    Raw(String),
}

impl Command {
    /// Human label for logs and error messages.
    pub fn describe(&self) -> &'static str {
        match self {
            Command::Power(_) => "power",
            Command::Volume(_) => "volume",
            Command::Input(_) => "input",
            Command::Nav(_) => "navigation",
            Command::Transport(_) => "transport",
            Command::Text(_) => "text",
            Command::Raw(_) => "raw",
        }
    }
}

/// Last-known state. Every field is optional because a device may not have told
/// us yet, and guessing is worse than admitting we don't know.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub reachable: bool,
    pub power: Option<bool>,
    /// 0.0–1.0, normalised across backends.
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    pub input: Option<String>,
    /// What the focused text field holds, or `None` when no field is focused
    /// and there is nothing to type into.
    pub keyboard: Option<String>,
}

/// A state change pushed by the device itself.
///
/// The Denon volunteers these on its control socket, which is what keeps the
/// UI's volume slider honest when someone uses the physical remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceEvent {
    Power(bool),
    Volume(f32),
    Muted(bool),
    Input(String),
    /// A text field gained focus (with its text) or lost it.
    Keyboard(Option<String>),
    Reachability(bool),
}

/// A `DeviceEvent` tagged with its source, for the daemon's fan-out bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedEvent {
    pub device: DeviceId,
    pub event: DeviceEvent,
}

/// A selectable source on a device.
///
/// `code` is what goes on the wire; `label` is what the owner has named it on
/// the device itself, which is what should appear in the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOption {
    pub code: String,
    pub label: String,
}

#[async_trait::async_trait]
pub trait Device: Send + Sync {
    fn id(&self) -> &DeviceId;
    fn kind(&self) -> DeviceKind;
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    async fn send(&self, cmd: Command) -> Result<()>;
    async fn state(&self) -> DeviceState;

    /// Selectable inputs, in the order the device lists them.
    ///
    /// Empty until the device has told us, so callers must cope with not
    /// knowing yet rather than treating empty as "has no inputs".
    async fn inputs(&self) -> Vec<InputOption> {
        Vec::new()
    }

    /// Subscribe to state pushed by the device.
    fn subscribe(&self) -> broadcast::Receiver<DeviceEvent>;
}
