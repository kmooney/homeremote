//! Samsung Smart TV (Tizen) support.
//!
//! Two facts drive the design. The TV exposes an unauthenticated info endpoint
//! on port 8001 that hands over its MAC address, and it powers down its network
//! stack in standby — so we capture that MAC while the set is awake and use
//! Wake-on-LAN to bring it back.

use std::net::IpAddr;
use std::time::Duration;

use serde::Deserialize;

use crate::Result;
use crate::device::DeviceId;
use crate::discovery::{Discovered, MacAddr, Vendor, ssdp, upnp};

/// Plain-HTTP info endpoint. Also the legacy websocket port.
pub const INFO_PORT: u16 = 8001;
/// TLS websocket port, the one that supports token auth.
pub const CONTROL_PORT: u16 = 8002;

/// Samsung's remote-control SSDP service.
pub const SSDP_TARGET: &str = "urn:samsung.com:device:RemoteControlReceiver:1";

/// What `GET http://<tv>:8001/api/v2/` returns, reduced to what we use.
#[derive(Debug, Clone, Deserialize)]
pub struct TvInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub device: TvDeviceInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TvDeviceInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "modelName", default)]
    pub model_name: Option<String>,
    #[serde(rename = "wifiMac", default)]
    pub wifi_mac: Option<String>,
    /// `"on"` / `"standby"`. Absent on some firmware.
    #[serde(rename = "PowerState", default)]
    pub power_state: Option<String>,
    /// `"true"` when the TV wants the token handshake on port 8002.
    #[serde(rename = "TokenAuthSupport", default)]
    pub token_auth_support: Option<String>,
}

impl TvInfo {
    /// Friendly name, preferring the one the user actually set on the TV.
    pub fn display_name(&self) -> String {
        self.device
            .name
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| "Samsung TV".to_string())
    }

    pub fn mac(&self) -> Option<MacAddr> {
        self.device.wifi_mac.as_deref()?.parse().ok()
    }

    pub fn is_on(&self) -> Option<bool> {
        let state = self.device.power_state.as_deref()?;
        Some(state.eq_ignore_ascii_case("on"))
    }

    pub fn token_auth(&self) -> bool {
        // Absent means older firmware, which pairs without a token.
        self.device
            .token_auth_support
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// Stable identity. The MAC is the best option: it outlives DHCP leases and
    /// is the same value we later need for Wake-on-LAN.
    pub fn stable_id(&self) -> Option<String> {
        if let Some(mac) = self.mac() {
            return Some(mac.to_string());
        }
        self.id.clone()
    }
}

/// Ask a candidate address whether it is a Tizen TV.
pub async fn fetch_info(client: &reqwest::Client, addr: IpAddr) -> Result<TvInfo> {
    let url = format!("http://{addr}:{INFO_PORT}/api/v2/");
    let response = client.get(&url).send().await?.error_for_status()?;
    let body = response.text().await?;
    parse_info(&body)
}

fn parse_info(body: &str) -> Result<TvInfo> {
    serde_json::from_str(body)
        .map_err(|e| crate::Error::Protocol(format!("unexpected TV info payload: {e}")))
}

/// Find Samsung TVs on the network.
///
/// Identity comes from the UPnP description, not from the Tizen info endpoint,
/// because the TV keeps serving UPnP in standby long after it has closed its
/// control ports. Keying on the endpoint that only answers when the set is
/// awake would give the same TV two different identities depending on when it
/// was scanned.
pub async fn discover(timeout: Duration) -> Result<Vec<Discovered>> {
    let responses = ssdp::search(SSDP_TARGET, timeout).await?;
    let client = crate::http_client();

    let mut seen = std::collections::HashSet::new();
    let mut devices = Vec::new();

    for response in responses {
        let addr = response.addr;
        if !seen.insert(addr) {
            continue;
        }

        // The description confirms it is a Samsung and supplies the identity;
        // the info endpoint enriches it when the TV happens to be awake.
        let (description, info) = tokio::join!(
            upnp::describe(&client, addr, response.location(), &[]),
            fetch_info(&client, addr),
        );
        let Ok(description) = description else {
            tracing::debug!(%addr, "SSDP hit published no UPnP description");
            continue;
        };
        if !description.made_by("samsung") {
            continue;
        }
        let Some(uuid) = description.uuid() else {
            tracing::warn!(%addr, "TV published no UDN; cannot track it across reboots");
            continue;
        };

        let info = info.ok();
        if info.is_none() {
            tracing::debug!(
                %addr,
                "TV is reachable but its control API is closed (usually standby)"
            );
        }

        devices.push(Discovered {
            id: DeviceId::new(format!("samsung:{uuid}")),
            vendor: Vendor::Samsung,
            name: info
                .as_ref()
                .map(TvInfo::display_name)
                .or_else(|| description.friendly_name.clone())
                .unwrap_or_else(|| "Samsung TV".to_string()),
            model: info
                .as_ref()
                .and_then(|i| i.device.model_name.clone())
                .or_else(|| description.model_name.clone()),
            addr,
            port: None,
            // Only the live TV knows which interface is actually connected, and
            // that is the one Wake-on-LAN has to target. In standby this stays
            // empty and is filled in by the next scan with the TV on.
            mac: info.as_ref().and_then(TvInfo::mac),
        });
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "id": "uuid:0e3b1f2a-1111-2222-3333-444455556666",
      "name": "[TV] Living Room",
      "type": "Samsung SmartTV",
      "version": "2.0.25",
      "device": {
        "name": "[TV] Living Room",
        "modelName": "UN55MU8000",
        "wifiMac": "a0:b1:c2:d3:e4:f5",
        "PowerState": "on",
        "TokenAuthSupport": "true",
        "networkType": "wired"
      }
    }"#;

    #[test]
    fn parses_the_info_endpoint() {
        let info = parse_info(SAMPLE).unwrap();
        assert_eq!(info.display_name(), "[TV] Living Room");
        assert_eq!(info.device.model_name.as_deref(), Some("UN55MU8000"));
        assert_eq!(info.mac(), Some("a0:b1:c2:d3:e4:f5".parse().unwrap()));
        assert_eq!(info.is_on(), Some(true));
        assert!(info.token_auth());
    }

    #[test]
    fn identity_prefers_the_mac_over_the_uuid() {
        // The MAC is what Wake-on-LAN needs, so it must be the identity we key on.
        let info = parse_info(SAMPLE).unwrap();
        assert_eq!(info.stable_id().as_deref(), Some("a0:b1:c2:d3:e4:f5"));
    }

    #[test]
    fn tolerates_firmware_that_omits_optional_fields() {
        let sparse = r#"{"device":{"name":"Bedroom TV"}}"#;
        let info = parse_info(sparse).unwrap();
        assert_eq!(info.display_name(), "Bedroom TV");
        assert_eq!(info.mac(), None);
        assert_eq!(info.is_on(), None, "unknown power state must not be guessed");
        assert!(!info.token_auth());
        assert_eq!(info.stable_id(), None);
    }

    #[test]
    fn standby_is_reported_as_off() {
        let sparse = r#"{"device":{"PowerState":"standby"}}"#;
        assert_eq!(parse_info(sparse).unwrap().is_on(), Some(false));
    }

    #[test]
    fn rejects_a_non_tv_response() {
        assert!(parse_info("<html>404</html>").is_err());
        assert!(parse_info(r#"{"something":"else"}"#).is_err());
    }
}
