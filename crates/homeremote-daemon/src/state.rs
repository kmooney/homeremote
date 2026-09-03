//! Shared daemon state: the live devices, the config, and the event bus.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use homeremote::config::{Config, DeviceConfig};
use homeremote::device::{Device, DeviceId, DeviceState, TaggedEvent};
use serde::Serialize;
use tokio::sync::{RwLock, broadcast};

/// Everything a UI client is told about, on one channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerEvent {
    /// A device reported a change.
    Device(TaggedEvent),
    /// An activity started, advanced, finished, or failed.
    Activity(ActivityProgress),
    /// The device list itself changed, e.g. after a re-scan.
    DevicesChanged,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ActivityProgress {
    pub activity: String,
    pub status: ActivityStatus,
    /// 1-based, for "step 2 of 5" in the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityStatus {
    Started,
    /// A step was skipped because the device was already in the wanted state.
    Skipped,
    Running,
    Finished,
    Failed,
}

/// A configured device, and its live connection when it has one.
pub struct ManagedDevice {
    pub config: DeviceConfig,
    /// `None` when the backend is not implemented or could not be opened. The
    /// device still appears in the UI, greyed out with a reason, rather than
    /// vanishing.
    pub device: Option<Arc<dyn Device>>,
    pub unavailable_because: Option<String>,
}

impl ManagedDevice {
    pub async fn snapshot(&self) -> DeviceSnapshot {
        let (state, inputs) = match &self.device {
            Some(device) => (device.state().await, device.inputs().await),
            None => (DeviceState::default(), Vec::new()),
        };
        DeviceSnapshot {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            vendor: self.config.vendor,
            kind: self.config.vendor.kind(),
            addr: self.config.addr.map(|a| a.to_string()),
            capabilities: self.device.as_ref().map(|d| d.capabilities()),
            unavailable_because: self.unavailable_because.clone(),
            inputs,
            state,
        }
    }
}

/// What the API hands to the UI for one device.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSnapshot {
    pub id: DeviceId,
    pub name: String,
    pub vendor: homeremote::discovery::Vendor,
    pub kind: homeremote::device::DeviceKind,
    pub addr: Option<String>,
    pub capabilities: Option<homeremote::device::Capabilities>,
    pub unavailable_because: Option<String>,
    /// Selectable sources, named as the owner has them on the device.
    pub inputs: Vec<homeremote::device::InputOption>,
    pub state: DeviceState,
}

pub struct AppState {
    pub config_path: PathBuf,
    pub config: RwLock<Config>,
    pub devices: RwLock<HashMap<DeviceId, ManagedDevice>>,
    pub events: broadcast::Sender<ServerEvent>,
    /// Which activity is currently running, for the diffing logic and the UI.
    pub current_activity: RwLock<Option<String>>,
    /// A pairing waiting on the PIN from the TV screen.
    ///
    /// Held here because the device ties the PIN to this open connection, and
    /// it times out in well under a minute — which is why pairing belongs in
    /// the UI rather than a slow copy-and-paste.
    pub pending_pairing: tokio::sync::Mutex<Option<PendingPairing>>,
}

pub struct PendingPairing {
    pub device: DeviceId,
    pub pairing: homeremote::appletv::session::PendingPairing,
}

impl AppState {
    pub fn new(config_path: PathBuf, config: Config) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            config_path,
            config: RwLock::new(config),
            devices: RwLock::new(HashMap::new()),
            events,
            current_activity: RwLock::new(None),
            pending_pairing: tokio::sync::Mutex::new(None),
        })
    }

    /// Open every configured device and start forwarding its events.
    ///
    /// A device that cannot be opened is kept and marked, so the UI can explain
    /// why rather than silently omitting it.
    pub async fn open_devices(self: &Arc<Self>) {
        let entries = self.config.read().await.devices.clone();
        let mut devices = self.devices.write().await;

        for entry in entries {
            if devices.contains_key(&entry.id) {
                // Already live: just refresh the address we know for it.
                if let Some(managed) = devices.get_mut(&entry.id) {
                    managed.config = entry;
                }
                continue;
            }
            let managed = match homeremote::open(&entry) {
                Ok(device) => {
                    self.clone().forward_events(entry.id.clone(), device.subscribe());
                    ManagedDevice {
                        config: entry,
                        device: Some(device),
                        unavailable_because: None,
                    }
                }
                Err(e) => {
                    tracing::info!(device = %entry.id, reason = %e, "device not controllable");
                    ManagedDevice {
                        config: entry,
                        device: None,
                        unavailable_because: Some(e.to_string()),
                    }
                }
            };
            devices.insert(managed.config.id.clone(), managed);
        }
    }

    /// Tag one device's events and republish them on the shared bus.
    fn forward_events(
        self: Arc<Self>,
        id: DeviceId,
        mut source: broadcast::Receiver<homeremote::device::DeviceEvent>,
    ) {
        tokio::spawn(async move {
            loop {
                match source.recv().await {
                    Ok(event) => {
                        let _ = self.events.send(ServerEvent::Device(TaggedEvent {
                            device: id.clone(),
                            event,
                        }));
                    }
                    // A burst outran this subscriber; keep going rather than
                    // tearing down the forwarder.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(device = %id, missed = n, "event forwarder fell behind");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Re-open one device after its credentials changed.
    pub async fn reopen(self: &Arc<Self>, id: &DeviceId) {
        self.devices.write().await.remove(id);
        self.open_devices().await;
    }

    pub async fn device(&self, id: &DeviceId) -> Option<Arc<dyn Device>> {
        self.devices.read().await.get(id)?.device.clone()
    }

    pub async fn snapshots(&self) -> Vec<DeviceSnapshot> {
        let devices = self.devices.read().await;
        let mut out = Vec::with_capacity(devices.len());
        for managed in devices.values() {
            out.push(managed.snapshot().await);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn publish(&self, event: ServerEvent) {
        // No subscribers is normal — nobody may have the page open.
        let _ = self.events.send(event);
    }

    /// Persist the current config.
    pub async fn save_config(&self) -> homeremote::Result<()> {
        self.config.read().await.save(&self.config_path)
    }
}
