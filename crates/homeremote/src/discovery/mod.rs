//! Finding the devices, so nobody has to type an IP address.
//!
//! Three protocols, because the three devices don't agree on one: SSDP for the
//! Denon and the Samsung, mDNS for the Apple TV. Everything here is
//! best-effort — a device that's powered off simply doesn't answer.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::device::{DeviceId, DeviceKind};

pub mod mdns;
pub mod ssdp;
pub mod upnp;
pub mod wol;

/// Which backend should drive this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Vendor {
    Denon,
    Samsung,
    AppleTv,
}

impl Vendor {
    pub fn kind(self) -> DeviceKind {
        match self {
            Vendor::Denon => DeviceKind::Receiver,
            Vendor::Samsung => DeviceKind::Television,
            Vendor::AppleTv => DeviceKind::StreamingBox,
        }
    }
}

/// A device we found on the network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovered {
    pub id: DeviceId,
    pub vendor: Vendor,
    pub name: String,
    pub model: Option<String>,
    pub addr: IpAddr,
    /// Control port, when the device advertises one rather than using a fixed
    /// port. The Apple TV picks a fresh Companion port on each boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Needed to wake the Samsung from standby, where it stops answering IP.
    pub mac: Option<MacAddr>,
}

impl Discovered {
    pub fn kind(&self) -> DeviceKind {
        self.vendor.kind()
    }
}

/// A six-byte hardware address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub fn octets(&self) -> [u8; 6] {
        self.0
    }
}

impl std::str::FromStr for MacAddr {
    type Err = String;

    /// Accepts `aa:bb:cc:dd:ee:ff`, `AA-BB-CC-DD-EE-FF`, or bare hex.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() != 12 {
            return Err(format!("not a MAC address: {s:?}"));
        }
        let mut out = [0u8; 6];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("bad MAC {s:?}: {e}"))?;
        }
        Ok(MacAddr(out))
    }
}

impl std::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl TryFrom<String> for MacAddr {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<MacAddr> for String {
    fn from(m: MacAddr) -> String {
        m.to_string()
    }
}

/// How long to listen for replies. Devices answer within `MX`, but a busy TV
/// can be slow, so we wait longer than the spec strictly requires.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);

/// Non-loopback IPv4 addresses of this machine.
///
/// Windows boxes routinely carry virtual adapters (Hyper-V, WSL, VPNs) that win
/// the default route, so multicast goes out every interface rather than
/// whichever one the OS would have picked.
pub fn local_ipv4_interfaces() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut addrs: Vec<Ipv4Addr> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4.ip),
            if_addrs::IfAddr::V6(_) => None,
        })
        .filter(|ip| !ip.is_unspecified() && !ip.is_link_local())
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

/// IPv4 broadcast addresses for each local network, for Wake-on-LAN.
pub fn local_broadcast_addrs() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return vec![Ipv4Addr::BROADCAST];
    };
    let mut addrs: Vec<Ipv4Addr> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4.broadcast.unwrap_or_else(|| {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                Ipv4Addr::from(ip | !mask)
            })),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect();
    addrs.push(Ipv4Addr::BROADCAST);
    addrs.sort();
    addrs.dedup();
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parses_common_spellings() {
        let expected = MacAddr([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        for s in ["aa:bb:cc:dd:ee:ff", "AA-BB-CC-DD-EE-FF", "aabbccddeeff"] {
            assert_eq!(s.parse::<MacAddr>().unwrap(), expected, "failed on {s}");
        }
    }

    #[test]
    fn mac_round_trips_through_display() {
        let mac: MacAddr = "01:23:45:67:89:AB".parse().unwrap();
        assert_eq!(mac.to_string(), "01:23:45:67:89:ab");
        assert_eq!(mac.to_string().parse::<MacAddr>().unwrap(), mac);
    }

    #[test]
    fn mac_rejects_wrong_length() {
        assert!("aa:bb:cc".parse::<MacAddr>().is_err());
        assert!("aa:bb:cc:dd:ee:ff:00".parse::<MacAddr>().is_err());
    }
}
