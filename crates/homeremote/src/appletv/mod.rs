//! Apple TV support over the Companion protocol.
//!
//! Discovery works today; pairing and control are the last phase of the
//! project. The Companion protocol is undocumented and reverse-engineered —
//! [pyatv](https://github.com/postlund/pyatv) is the reference — which is why
//! it sits behind its own feature flag and is sequenced after the receiver and
//! the TV.

pub mod crypto;
pub mod frame;
pub mod link;
pub mod session;

mod device;
pub use device::AppleTv;
pub mod pairing;
pub mod opack;
pub mod tlv8;

use std::time::Duration;

/// Fallback Companion port, used when mDNS has not told us the real one. The
/// Apple TV picks a fresh port on each boot, so this is only a last resort.
pub const DEFAULT_PORT: u16 = 49152;

use crate::Result;
use crate::device::DeviceId;
use crate::discovery::mdns::{self, AIRPLAY_SERVICE, COMPANION_SERVICE, MdnsService};
use crate::discovery::{Discovered, MacAddr, Vendor};

/// Find Apple TVs on the network.
///
/// Browses two services: `_companion-link._tcp` is the endpoint we will
/// eventually control, and `_airplay._tcp` is browsed alongside it purely
/// because it carries the hardware model string, which Companion does not.
pub async fn discover(timeout: Duration) -> Result<Vec<Discovered>> {
    // Concurrently, so browsing two services costs one timeout, not two.
    let (companion, airplay) = tokio::join!(
        mdns::browse(COMPANION_SERVICE, timeout),
        mdns::browse(AIRPLAY_SERVICE, timeout),
    );
    let companion = companion?;
    // A missing AirPlay record only costs us the model name.
    let airplay = airplay.unwrap_or_default();

    let mut devices = Vec::new();
    for service in companion {
        // Macs, iPhones, and HomePods all advertise Companion for Handoff. Only
        // an Apple TV should end up in the remote.
        if !is_apple_tv(&service) {
            tracing::debug!(name = %service.instance, "ignoring non-Apple-TV Companion peer");
            continue;
        }
        let Some(addr) = service.ipv4() else {
            tracing::debug!(name = %service.instance, "Companion service resolved with no address");
            continue;
        };
        // This machine advertises itself; a loopback address is never a device
        // we can control.
        if addr.is_loopback() {
            continue;
        }
        // Match up the AirPlay record by address; the instance names differ.
        let airplay_peer = airplay.iter().find(|a| a.ipv4() == Some(addr));
        let model = service
            .txt("rpmd")
            .or_else(|| airplay_peer.and_then(|a| a.txt("model")))
            .map(str::to_string);

        let Some(identifier) = identifier(&service, airplay_peer) else {
            tracing::warn!(name = %service.instance, "Apple TV advertised no stable identifier");
            continue;
        };

        devices.push(Discovered {
            id: DeviceId::new(format!("appletv:{identifier}")),
            vendor: Vendor::AppleTv,
            name: service.instance.clone(),
            model,
            addr,
            port: Some(service.port),
            mac: airplay_peer
                .and_then(|a| a.txt("deviceid"))
                .and_then(|d| d.parse::<MacAddr>().ok()),
        });
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(devices)
}

/// Look up where one Apple TV is listening now.
///
/// The Companion port changes on every boot, and the address can move with a
/// new DHCP lease, so a long-running connection has to re-ask rather than
/// trust what it was given at startup.
pub async fn resolve(id: &DeviceId, timeout: Duration) -> Option<(std::net::IpAddr, u16)> {
    match discover(timeout).await {
        Ok(found) => found
            .into_iter()
            .find(|d| &d.id == id)
            .and_then(|d| Some((d.addr, d.port?))),
        Err(e) => {
            tracing::debug!(error = %e, "could not browse for the Apple TV");
            None
        }
    }
}

/// Whether a Companion peer is an Apple TV rather than some other Apple device.
///
/// The `rpMd` TXT key carries the hardware model (`AppleTV5,3`). Macs and
/// iPhones advertise Companion for Handoff but omit it entirely; HomePods set it
/// to `AudioAccessory*`. Requiring the `AppleTV` prefix is exact.
fn is_apple_tv(service: &MdnsService) -> bool {
    service
        .txt("rpmd")
        .is_some_and(|model| model.starts_with("AppleTV"))
}

/// The value we key pairing credentials on.
///
/// This has to be genuinely stable, because losing it means losing the pairing
/// and making someone re-enter a PIN on the TV.
///
/// Most of the Companion TXT record is **privacy-rotating**: `rpBA`, `rpAD`,
/// `rpHI`, `rpHN`, and `rpHA` were all observed changing within minutes on the
/// same Apple TV. `rpMRtID` did not, and it matches the AirPlay record's `psi`
/// — so that is the identity, with AirPlay as the fallback.
fn identifier(companion: &MdnsService, airplay: Option<&MdnsService>) -> Option<String> {
    companion
        .txt("rpmrtid")
        .or_else(|| airplay.and_then(|a| a.txt("psi")))
        .or_else(|| airplay.and_then(|a| a.txt("deviceid")))
        .map(str::to_ascii_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn service(name: &str, txt: &[(&str, &str)]) -> MdnsService {
        MdnsService {
            fullname: format!("{name}._companion-link._tcp.local."),
            instance: name.to_string(),
            hostname: format!("{name}.local."),
            port: 49153,
            addresses: vec!["192.168.4.34".parse().unwrap()],
            txt: txt
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// The Apple TV's Companion record, captured live.
    fn apple_tv(rpba: &str) -> MdnsService {
        service(
            "Living Room",
            &[
                ("rpmd", "AppleTV5,3"),
                ("rpba", rpba),
                ("rpmrtid", "582DF7CA-6BF5-4E9D-B72B-78DB78D30A10"),
            ],
        )
    }

    #[test]
    fn recognises_a_real_apple_tv() {
        assert!(is_apple_tv(&apple_tv("F0:42:37:83:0B:65")));
    }

    #[test]
    fn rejects_a_mac_advertising_companion() {
        // A MacBook advertises the same service for Handoff, with no rpMd.
        // Without this filter it lands in the remote as a controllable device.
        let mac = service(
            "Kevin's MacBook Air",
            &[("rpba", "78:EC:2A:62:2A:CE"), ("rpvr", "715.2")],
        );
        assert!(!is_apple_tv(&mac));
    }

    #[test]
    fn rejects_a_homepod() {
        let homepod = service("Kitchen", &[("rpmd", "AudioAccessory5,1")]);
        assert!(!is_apple_tv(&homepod));
    }

    #[test]
    fn identity_survives_privacy_rotation() {
        // These two records are the SAME Apple TV, minutes apart. Every address
        // field rotated; only rpMRtID held. Keying on rpBA would silently
        // discard the pairing every few minutes.
        let before = apple_tv("F0:42:37:83:0B:65");
        let after = apple_tv("08:17:E7:3F:70:66");
        assert_ne!(before.txt("rpba"), after.txt("rpba"), "precondition");
        assert_eq!(identifier(&before, None), identifier(&after, None));
    }

    #[test]
    fn identity_is_the_media_remote_id() {
        assert_eq!(
            identifier(&apple_tv("F0:42:37:83:0B:65"), None).as_deref(),
            Some("582DF7CA-6BF5-4E9D-B72B-78DB78D30A10")
        );
    }

    #[test]
    fn falls_back_to_airplay_when_companion_omits_the_id() {
        let companion = service("Living Room", &[("rpmd", "AppleTV5,3")]);
        let airplay = service(
            "Living Room",
            &[("psi", "582df7ca-6bf5-4e9d-b72b-78db78d30a10")],
        );
        assert_eq!(
            identifier(&companion, Some(&airplay)).as_deref(),
            Some("582DF7CA-6BF5-4E9D-B72B-78DB78D30A10")
        );
    }

    #[test]
    fn no_identifier_when_nothing_stable_is_advertised() {
        // rpBA alone must not qualify: it rotates.
        let rotating_only = service("Living Room", &[("rpmd", "AppleTV5,3"), ("rpba", "AA:BB")]);
        assert_eq!(identifier(&rotating_only, None), None);
    }

    #[test]
    fn ignores_unrelated_txt_keys() {
        let companion = MdnsService {
            txt: HashMap::from([("rpvr".to_string(), "715.2".to_string())]),
            ..service("x", &[])
        };
        assert_eq!(identifier(&companion, None), None);
    }
}
