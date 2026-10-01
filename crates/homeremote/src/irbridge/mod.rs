//! The Wi-Fi to IR bridge: an M5Stack ATOM that blasts IR at the Samsung.
//!
//! The UN55J6203 has no IP control (see [`crate::samsung`]), so the bridge
//! stands in front of it. Its firmware lives in `firmware/ir-bridge`; it
//! advertises itself over mDNS and takes plain HTTP requests.
//!
//! IR is one-way. The bridge knows a code went out, never whether the TV acted
//! on it — so this device reports itself reachable and leaves the TV's power
//! state unknown. Claiming the TV is on after sending "on" would let an
//! activity skip the step the next time, after someone turned it off by hand.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::device::{
    Capabilities, Command, Device, DeviceEvent, DeviceId, DeviceKind, DeviceState, PowerState,
};
use crate::discovery::mdns;
use crate::discovery::{Discovered, MacAddr, Vendor};
use crate::{Error, Result};

/// What the firmware advertises. Changing it orphans every flashed bridge.
pub const MDNS_SERVICE: &str = "_homeremote-ir._tcp.local.";

/// The firmware's HTTP port, used when mDNS has not told us.
pub const DEFAULT_PORT: u16 = 80;

/// How often to check the bridge is still on the network, so the remote shows
/// it offline before someone presses a button and wonders why the TV ignored it.
const HEALTH_INTERVAL: Duration = Duration::from_secs(20);

/// How long to browse mDNS when the bridge has stopped answering at the
/// address we had. Short, because an activity step is waiting on it.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

const EVENT_QUEUE: usize = 16;

/// Find IR bridges on the network.
pub async fn discover(timeout: Duration) -> Result<Vec<Discovered>> {
    let mut devices: Vec<Discovered> = mdns::browse(MDNS_SERVICE, timeout)
        .await?
        .iter()
        .filter_map(from_service)
        .collect();
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(devices)
}

/// Turn one mDNS record into a device, keyed on the MAC the firmware puts in
/// its TXT record — the only identity that survives a new DHCP lease.
fn from_service(service: &mdns::MdnsService) -> Option<Discovered> {
    let Some(mac) = service.txt("id").and_then(|id| id.parse::<MacAddr>().ok()) else {
        tracing::warn!(name = %service.instance, "IR bridge advertised no MAC; skipping");
        return None;
    };
    let addr = service.ipv4()?;
    Some(Discovered {
        id: DeviceId::new(format!("irbridge:{mac}")),
        vendor: Vendor::IrBridge,
        name: "Wi-Fi IR Bridge".to_string(),
        model: service.txt("model").map(str::to_string),
        addr,
        port: Some(service.port),
        mac: Some(mac),
    })
}

/// Look up where one bridge is now, after it has moved address.
async fn resolve(id: &DeviceId) -> Option<(IpAddr, u16)> {
    match discover(RESOLVE_TIMEOUT).await {
        Ok(found) => found
            .into_iter()
            .find(|d| &d.id == id)
            .map(|d| (d.addr, d.port.unwrap_or(DEFAULT_PORT))),
        Err(e) => {
            tracing::debug!(error = %e, "could not browse for the IR bridge");
            None
        }
    }
}

/// The firmware path that carries a command.
fn path_for(command: &Command) -> Result<String> {
    match command {
        Command::Power(PowerState::On) => Ok("/power/on".into()),
        Command::Power(PowerState::Off) => Ok("/power/off".into()),
        // The firmware only sends discrete codes. A toggle sent to a TV that is
        // already on turns it off, and IR cannot tell us which happened.
        Command::Power(PowerState::Toggle) => Err(Error::Unsupported(
            "power toggle over IR — the TV's state is unknown, so use on or off",
        )),
        // Any Samsung code, so new ones can be tried without reflashing.
        Command::Raw(code) => {
            let code = code.trim();
            let digits = code
                .strip_prefix("0x")
                .or_else(|| code.strip_prefix("0X"))
                .unwrap_or(code);
            if digits.is_empty()
                || digits.len() > 8
                || !digits.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(Error::Protocol(format!(
                    "{code:?} is not a Samsung IR code; expected hex like 0xE0E040BF"
                )));
            }
            Ok(format!("/samsung?code=0x{digits}"))
        }
        other => Err(Error::Unsupported(other.describe())),
    }
}

struct Inner {
    id: DeviceId,
    name: String,
    endpoint: Mutex<(IpAddr, u16)>,
    client: reqwest::Client,
    state: Mutex<DeviceState>,
    events: broadcast::Sender<DeviceEvent>,
}

impl Inner {
    fn url(&self, path: &str) -> String {
        let (addr, port) = *self.endpoint.lock().unwrap();
        format!("http://{addr}:{port}{path}")
    }

    fn set_reachable(&self, reachable: bool) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = state.reachable != reachable;
            state.reachable = reachable;
            changed
        };
        if changed {
            let _ = self.events.send(DeviceEvent::Reachability(reachable));
        }
    }

    async fn post(&self, path: &str) -> std::result::Result<(), reqwest::Error> {
        self.client
            .post(self.url(path))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn ping(&self) -> bool {
        match self.client.get(self.url("/")).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// Re-find the bridge over mDNS. Returns whether it moved.
    async fn relocate(&self) -> bool {
        let Some(found) = resolve(&self.id).await else {
            return false;
        };
        let mut endpoint = self.endpoint.lock().unwrap();
        if *endpoint == found {
            return false;
        }
        tracing::info!(device = %self.id, from = %endpoint.0, to = %found.0, "IR bridge moved");
        *endpoint = found;
        true
    }

    /// Send a request, re-resolving once if the bridge is not where it was.
    ///
    /// Only a failure to *reach* the bridge is retried. An error the bridge
    /// itself returned means it heard us, and a second attempt would just
    /// repeat it.
    async fn send(&self, path: &str) -> Result<()> {
        let error = match self.post(path).await {
            Ok(()) => {
                self.set_reachable(true);
                return Ok(());
            }
            Err(e) => e,
        };
        if error.status().is_some() {
            self.set_reachable(true);
            return Err(Error::Http(error));
        }
        if self.relocate().await {
            let retried = self.post(path).await;
            self.set_reachable(retried.is_ok());
            return retried.map_err(Error::Http);
        }
        self.set_reachable(false);
        Err(Error::Unreachable(self.endpoint.lock().unwrap().0))
    }
}

pub struct IrBridge {
    inner: Arc<Inner>,
}

impl IrBridge {
    /// Start tracking a bridge. Returns immediately; reachability arrives with
    /// the first health check.
    pub fn connect(found: &Discovered) -> Self {
        let (events, _) = broadcast::channel(EVENT_QUEUE);
        let inner = Arc::new(Inner {
            id: found.id.clone(),
            name: found.name.clone(),
            endpoint: Mutex::new((found.addr, found.port.unwrap_or(DEFAULT_PORT))),
            client: crate::http_client(),
            state: Mutex::new(DeviceState::default()),
            events,
        });
        tokio::spawn(watch_health(Arc::downgrade(&inner)));
        Self { inner }
    }
}

/// Poll the bridge until the device is dropped.
async fn watch_health(inner: Weak<Inner>) {
    loop {
        let Some(bridge) = inner.upgrade() else { return };
        let mut up = bridge.ping().await;
        if !up && bridge.relocate().await {
            up = bridge.ping().await;
        }
        bridge.set_reachable(up);
        drop(bridge);
        tokio::time::sleep(HEALTH_INTERVAL).await;
    }
}

#[async_trait::async_trait]
impl Device for IrBridge {
    fn id(&self) -> &DeviceId {
        &self.inner.id
    }

    fn kind(&self) -> DeviceKind {
        DeviceKind::IrBridge
    }

    fn name(&self) -> &str {
        &self.inner.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            power: true,
            ..Capabilities::default()
        }
    }

    async fn send(&self, command: Command) -> Result<()> {
        let path = path_for(&command)?;
        self.inner.send(&path).await
    }

    async fn state(&self) -> DeviceState {
        self.inner.state.lock().unwrap().clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.inner.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The record the firmware advertises, as mdns-sd hands it over.
    fn record(txt: &[(&str, &str)]) -> mdns::MdnsService {
        mdns::MdnsService {
            fullname: format!("homeremote-ir-12bf08.{MDNS_SERVICE}"),
            instance: "homeremote-ir-12bf08".into(),
            hostname: "homeremote-ir-12bf08.local.".into(),
            port: 80,
            addresses: vec!["192.168.4.60".parse().unwrap()],
            txt: txt
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn identity_is_the_mac_from_the_txt_record() {
        let found = from_service(&record(&[("id", "8c:94:df:12:bf:08"), ("model", "atom")]))
            .unwrap();
        assert_eq!(found.id.as_str(), "irbridge:8c:94:df:12:bf:08");
        assert_eq!(found.vendor, Vendor::IrBridge);
        assert_eq!(found.port, Some(80));
        assert_eq!(found.model.as_deref(), Some("atom"));
    }

    #[test]
    fn a_record_without_a_mac_is_not_a_bridge() {
        // Without it there is no identity that survives a new lease.
        assert!(from_service(&record(&[("model", "atom")])).is_none());
    }

    #[test]
    fn power_maps_to_the_discrete_endpoints() {
        assert_eq!(path_for(&Command::Power(PowerState::On)).unwrap(), "/power/on");
        assert_eq!(path_for(&Command::Power(PowerState::Off)).unwrap(), "/power/off");
    }

    #[test]
    fn power_toggle_is_refused() {
        // A toggle to a TV that is already on turns it off.
        assert!(matches!(
            path_for(&Command::Power(PowerState::Toggle)),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn raw_takes_a_samsung_code_in_either_spelling() {
        assert_eq!(
            path_for(&Command::Raw("0xE0E040BF".into())).unwrap(),
            "/samsung?code=0xE0E040BF"
        );
        assert_eq!(
            path_for(&Command::Raw("E0E040BF".into())).unwrap(),
            "/samsung?code=0xE0E040BF"
        );
    }

    #[test]
    fn raw_rejects_anything_that_is_not_a_32_bit_code() {
        for bad in ["", "0x", "power", "0xE0E040BF00", "12 34"] {
            assert!(path_for(&Command::Raw(bad.into())).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn other_commands_are_unsupported() {
        assert!(matches!(
            path_for(&Command::Input("HDMI1".into())),
            Err(Error::Unsupported(_))
        ));
    }
}
