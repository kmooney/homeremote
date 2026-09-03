//! Denon AVR support (tested against an AVR-S720W).
//!
//! Control lives on a raw TCP socket on port 23. Note the constraint that
//! shapes everything downstream: **the receiver accepts exactly one connection
//! on that port**. A second client is not refused, it is silently ignored.

pub mod connection;
pub mod protocol;

pub use connection::DenonReceiver;

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use crate::Result;
use crate::device::DeviceId;
use crate::discovery::mdns::{self, AIRPLAY_SERVICE};
use crate::discovery::{Discovered, MacAddr, Vendor, ssdp, upnp};

/// The receiver's control port.
pub const CONTROL_PORT: u16 = 23;

/// Fallback control path for when something else holds the single telnet
/// socket: `http://<avr>/goform/formiPhoneAppDirect.xml?<COMMAND>`.
///
/// Port 80, verified against an AVR-S720W — port 8080 serves the UPnP
/// description but answers 404 here. Fire-and-forget: it carries commands but
/// gives no state back, so the event stream is lost while it is in use.
pub const HTTP_CONTROL_PORT: u16 = 80;
pub const HTTP_CONTROL_PATH: &str = "/goform/formiPhoneAppDirect.xml";

/// Denon's own SSDP search target. Newer HEOS firmware answers this; the
/// AVR-S720W does not, which is why it is only one of several sources.
pub const SSDP_TARGET: &str = "urn:schemas-denon-com:device:ACT-Denon:1";

/// The generic UPnP search every device answers.
///
/// The AVR-S720W ignores the Denon-specific target above and only replies to
/// this one, so the net has to be cast wide and narrowed by the manufacturer
/// field in each candidate's description.
const SSDP_ROOT_TARGET: &str = "upnp:rootdevice";

/// D&M Holdings OUI, shared by Denon and Marantz. Their AirPlay `deviceid` is
/// the same value as the receiver's serial number, so mDNS gives us a second
/// discovery path that needs no SSDP at all.
const DENON_OUI: [u8; 3] = [0x00, 0x05, 0xCD];

/// Description XML paths, newest layout first.
const DESCRIPTION_PATHS: [(u16, &str); 2] = [
    (60006, "/upnp/desc/aios_device/aios_device.xml"),
    (8080, "/description.xml"),
];

/// Find Denon receivers on the network.
///
/// Candidates come from three sources — the Denon SSDP target, the generic UPnP
/// one, and AirPlay over mDNS — because no single one finds every model. Each
/// candidate is then confirmed by fetching its description and checking the
/// manufacturer, so casting a wide net costs accuracy nothing.
pub async fn discover(timeout: Duration) -> Result<Vec<Discovered>> {
    let (denon_hits, root_hits, airplay) = tokio::join!(
        ssdp::search(SSDP_TARGET, timeout),
        ssdp::search(SSDP_ROOT_TARGET, timeout),
        mdns::browse(AIRPLAY_SERVICE, timeout),
    );

    // Address -> the description URL it advertised, if any.
    let mut candidates: HashMap<IpAddr, Option<String>> = HashMap::new();
    for hits in [denon_hits, root_hits] {
        for response in hits.unwrap_or_default() {
            candidates
                .entry(response.addr)
                .or_insert_with(|| response.location().map(str::to_string));
        }
    }
    for service in airplay.unwrap_or_default() {
        // Only D&M hardware; every other AirPlay speaker on the network is noise.
        let is_dm = service
            .txt("deviceid")
            .and_then(|id| id.parse::<MacAddr>().ok())
            .is_some_and(|mac| mac.octets()[..3] == DENON_OUI);
        if let (true, Some(addr)) = (is_dm, service.ipv4()) {
            candidates.entry(addr).or_insert(None);
        }
    }

    let client = crate::http_client();
    let probes = candidates
        .into_iter()
        .map(|(addr, location)| {
            let client = &client;
            async move {
                let paths = &DESCRIPTION_PATHS[..];
                let urls = upnp::candidate_urls(addr, location.as_deref(), paths);
                (addr, upnp::fetch(client, &urls).await)
            }
        });

    let mut devices = Vec::new();
    for (addr, described) in futures_util::future::join_all(probes).await {
        // The manufacturer check is what makes the wide net safe: a generic
        // rootdevice search also turns up the router and the TV.
        let Some(desc) = described else { continue };
        if !(desc.made_by("denon") || desc.made_by("marantz") || desc.made_by("d&m")) {
            continue;
        }
        // The serial is the receiver's only identity that survives a DHCP
        // lease change.
        let Some(serial) = desc.serial_number.clone() else {
            tracing::warn!(%addr, "Denon published no serial number; skipping");
            continue;
        };
        let model = desc.model_name.clone().unwrap_or_else(|| "Denon AVR".into());
        devices.push(Discovered {
            id: DeviceId::new(format!("denon:{serial}")),
            vendor: Vendor::Denon,
            name: desc.friendly_name.clone().unwrap_or_else(|| model.clone()),
            model: Some(model),
            addr,
            // Denon sets its serial to its MAC, so this is free.
            mac: serial.parse().ok(),
        });
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::upnp;

    /// Captured verbatim from the AVR-S720W at `:8080/description.xml`.
    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>Denon AVR-S720W</friendlyName>
    <manufacturer>Denon</manufacturer>
    <modelName>*AVR-S720W</modelName>
    <serialNumber>0005CD91E7A1</serialNumber>
  </device>
</root>"#;

    #[test]
    fn the_serial_doubles_as_the_mac() {
        // Which is what lets the SSDP and mDNS paths agree on one identity.
        let desc = upnp::parse(DESCRIPTION).unwrap();
        let mac: MacAddr = desc.serial_number.unwrap().parse().unwrap();
        assert_eq!(mac.octets()[..3], DENON_OUI);
        assert_eq!(mac.to_string(), "00:05:cd:91:e7:a1");
    }

    #[test]
    fn the_receiver_is_recognised_as_denon() {
        assert!(upnp::parse(DESCRIPTION).unwrap().made_by("denon"));
    }

    #[test]
    fn description_paths_cover_both_firmware_layouts() {
        // Newer HEOS firmware serves the aios path; the S720W serves the old one.
        let ports: Vec<u16> = DESCRIPTION_PATHS.iter().map(|(p, _)| *p).collect();
        assert!(ports.contains(&60006) && ports.contains(&8080));
    }
}
