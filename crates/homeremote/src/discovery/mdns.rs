//! mDNS / DNS-SD browsing, used to find the Apple TV.
//!
//! Apple advertises the Companion endpoint as `_companion-link._tcp`. The TXT
//! record carries the device identifier we pair against, which is what makes
//! the Apple TV addressable by identity rather than by IP.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::Result;

/// The Companion protocol service, which is what we actually control.
pub const COMPANION_SERVICE: &str = "_companion-link._tcp.local.";
/// AirPlay, browsed alongside it because it carries a friendlier device name.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

/// One resolved mDNS service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsService {
    /// e.g. `Living Room._companion-link._tcp.local.`
    pub fullname: String,
    /// The instance name with the service suffix stripped: `Living Room`.
    pub instance: String,
    pub hostname: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    pub txt: HashMap<String, String>,
}

impl MdnsService {
    pub fn txt(&self, key: &str) -> Option<&str> {
        self.txt.get(key).map(String::as_str)
    }

    /// First IPv4 address, preferred over IPv6 for these devices.
    pub fn ipv4(&self) -> Option<IpAddr> {
        self.addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| self.addresses.first())
            .copied()
    }
}

/// Strip the trailing service type from a DNS-SD fullname.
///
/// mDNS escapes dots and spaces inside the instance label, so unescape as we go
/// — an Apple TV named "Kev's TV" arrives as `Kev\032s\032TV`.
fn instance_name(fullname: &str, service: &str) -> String {
    let raw = fullname.strip_suffix(service).unwrap_or(fullname);
    let raw = raw.strip_suffix('.').unwrap_or(raw);
    unescape(raw)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // `\DDD` is a decimal byte escape; `\X` is a literal X.
        let digits: String = chars.clone().take(3).collect();
        if digits.len() == 3
            && digits.chars().all(|d| d.is_ascii_digit())
            && let Ok(byte) = digits.parse::<u8>()
        {
            out.push(byte as char);
            for _ in 0..3 {
                chars.next();
            }
            continue;
        }
        if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}

/// Browse one service type and return everything that resolves within `timeout`.
///
/// Always waits the full timeout rather than stopping at the first hit: a
/// household can have more than one Apple TV, and we want the whole list.
pub async fn browse(service: &str, timeout: Duration) -> Result<Vec<MdnsService>> {
    let daemon = ServiceDaemon::new()
        .map_err(|e| crate::Error::Protocol(format!("could not start mDNS daemon: {e}")))?;
    let receiver = daemon
        .browse(service)
        .map_err(|e| crate::Error::Protocol(format!("could not browse {service}: {e}")))?;

    let mut found: HashMap<String, MdnsService> = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        match tokio::time::timeout_at(deadline, receiver.recv_async()).await {
            Err(_) => break,           // deadline reached: report what we have
            Ok(Err(_)) => break,       // daemon shut down
            Ok(Ok(event)) => {
                if let ServiceEvent::ServiceResolved(info) = event {
                    if !info.is_valid() {
                        continue;
                    }
                    let txt = info
                        .txt_properties
                        .iter()
                        .map(|p| (p.key().to_ascii_lowercase(), p.val_str().to_string()))
                        .collect();
                    let addresses = info
                        .get_addresses()
                        .iter()
                        .map(|scoped| scoped.to_ip_addr())
                        .collect();
                    let fullname = info.get_fullname().to_string();
                    let entry = MdnsService {
                        instance: instance_name(&fullname, service),
                        hostname: info.get_hostname().to_string(),
                        port: info.get_port(),
                        addresses,
                        txt,
                        fullname: fullname.clone(),
                    };
                    found.insert(fullname, entry);
                }
            }
        }
    }

    // Best-effort: the daemon is dropped either way.
    let _ = daemon.shutdown();

    let mut services: Vec<MdnsService> = found.into_values().collect();
    services.sort_by(|a, b| a.fullname.cmp(&b.fullname));
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_service_suffix() {
        assert_eq!(
            instance_name("Living Room._companion-link._tcp.local.", COMPANION_SERVICE),
            "Living Room"
        );
    }

    #[test]
    fn unescapes_mdns_byte_escapes() {
        // A device named "Kev's TV" is advertised with the space escaped.
        assert_eq!(
            instance_name("Kev\\039s\\032TV._companion-link._tcp.local.", COMPANION_SERVICE),
            "Kev's TV"
        );
    }

    #[test]
    fn unescapes_literal_backslash_escapes() {
        assert_eq!(unescape("a\\.b"), "a.b");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn leaves_unrecognised_names_alone() {
        assert_eq!(instance_name("weird", COMPANION_SERVICE), "weird");
    }

    #[test]
    fn prefers_ipv4() {
        let svc = MdnsService {
            fullname: "x".into(),
            instance: "x".into(),
            hostname: "x.local.".into(),
            port: 49152,
            addresses: vec![
                "fe80::1".parse().unwrap(),
                "192.168.1.42".parse().unwrap(),
            ],
            txt: HashMap::new(),
        };
        assert_eq!(svc.ipv4(), Some("192.168.1.42".parse().unwrap()));
    }
}
