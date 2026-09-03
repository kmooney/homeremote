//! A minimal SSDP (UPnP discovery) client.
//!
//! Hand-rolled rather than pulled from a crate for one reason: we need to send
//! the `M-SEARCH` from *every* local interface. On a Windows machine with a
//! Hyper-V or VPN adapter, a socket bound to `0.0.0.0` routinely searches the
//! wrong network and finds nothing.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::Result;
use crate::discovery::local_ipv4_interfaces;

const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MULTICAST_PORT: u16 = 1900;

/// Devices are told to spread replies over `MX` seconds to avoid stampeding the
/// searcher. Keep it low; we're searching a home network with a handful of hosts.
const MX_SECONDS: u8 = 2;

/// One reply to an `M-SEARCH`.
#[derive(Debug, Clone)]
pub struct SsdpResponse {
    /// Who answered.
    pub addr: IpAddr,
    /// Header names are lowercased; values are trimmed.
    pub headers: HashMap<String, String>,
}

impl SsdpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// URL of the device description XML.
    pub fn location(&self) -> Option<&str> {
        self.header("location")
    }

    /// Unique Service Name — stable per device, so it deduplicates replies.
    pub fn usn(&self) -> Option<&str> {
        self.header("usn")
    }

    pub fn server(&self) -> Option<&str> {
        self.header("server")
    }

    fn parse(addr: IpAddr, bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.split("\r\n");
        // Status line: "HTTP/1.1 200 OK". Anything else (a NOTIFY, say) isn't ours.
        let status = lines.next()?;
        if !status.to_ascii_uppercase().starts_with("HTTP/") {
            return None;
        }
        let headers = lines
            .filter_map(|line| {
                let (k, v) = line.split_once(':')?;
                Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
            })
            .collect();
        Some(SsdpResponse { addr, headers })
    }
}

/// Broadcast an `M-SEARCH` for `search_target` and collect replies until
/// `timeout` elapses.
///
/// Replies are deduplicated by `USN`, since we send the query several times to
/// survive the packet loss that UDP multicast is prone to on Wi-Fi.
pub async fn search(search_target: &str, timeout: Duration) -> Result<Vec<SsdpResponse>> {
    let mut interfaces = local_ipv4_interfaces();
    if interfaces.is_empty() {
        // No interface list available: fall back to letting the OS choose.
        interfaces.push(Ipv4Addr::UNSPECIFIED);
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();

    for iface in interfaces {
        let socket = match bind_sender(iface) {
            Ok(s) => s,
            Err(e) => {
                // A down or unusable adapter shouldn't sink the whole search.
                tracing::debug!(%iface, error = %e, "skipping interface for SSDP");
                continue;
            }
        };
        let request = msearch_request(search_target);
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            probe(socket, request, timeout, tx).await
        }));
    }
    drop(tx);

    if tasks.is_empty() {
        return Err(crate::Error::Protocol(
            "no usable network interface for SSDP discovery".into(),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    let mut responses = Vec::new();
    while let Some(resp) = rx.recv().await {
        // Fall back to the source address when a device omits USN.
        let key = resp
            .usn()
            .map(str::to_string)
            .unwrap_or_else(|| resp.addr.to_string());
        if seen.insert(key) {
            responses.push(resp);
        }
    }
    let mut any_sent = false;
    for task in tasks {
        any_sent |= task.await.unwrap_or(false);
    }
    if !any_sent {
        // Distinct from "nothing answered": the query never left the machine,
        // usually a firewall or an interface with no multicast route.
        return Err(crate::Error::Protocol(
            "could not send SSDP discovery on any interface".into(),
        ));
    }
    Ok(responses)
}

fn msearch_request(search_target: &str) -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {MULTICAST_ADDR}:{MULTICAST_PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX_SECONDS}\r\n\
         ST: {search_target}\r\n\
         USER-AGENT: homeremote/{}\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    )
    .into_bytes()
}

/// Send the query a few times on one interface and forward every reply.
/// Returns whether at least one query made it onto the wire.
async fn probe(
    socket: UdpSocket,
    request: Vec<u8>,
    timeout: Duration,
    tx: mpsc::UnboundedSender<SsdpResponse>,
) -> bool {
    let mut sent_ok = false;
    let target = SocketAddr::V4(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
    let deadline = tokio::time::Instant::now() + timeout;

    // Three attempts, spaced out: multicast over Wi-Fi drops packets freely.
    let mut sends = 0;
    let mut next_send = tokio::time::Instant::now();
    let mut buf = vec![0u8; 4096];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return sent_ok;
        }
        if sends < 3 && now >= next_send {
            match socket.send_to(&request, target).await {
                Ok(_) => sent_ok = true,
                Err(e) => {
                    // A send failure here means this interface can't reach the
                    // multicast group at all; further attempts won't help.
                    tracing::debug!(error = %e, "SSDP send failed");
                    return sent_ok;
                }
            }
            sends += 1;
            next_send = now + Duration::from_millis(700);
        }

        let wake = if sends < 3 { next_send.min(deadline) } else { deadline };
        match tokio::time::timeout_at(wake, socket.recv_from(&mut buf)).await {
            // Timed out waiting: loop round to re-send or finish.
            Err(_) => continue,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "SSDP receive failed");
                return sent_ok;
            }
            Ok(Ok((len, from))) => {
                if let Some(resp) = SsdpResponse::parse(from.ip(), &buf[..len])
                    && tx.send(resp).is_err()
                {
                    return sent_ok; // collector went away
                }
            }
        }
    }
}

/// A UDP socket that sends multicast out of one specific interface.
///
/// The socket binds to `0.0.0.0` and selects the interface with
/// `IP_MULTICAST_IF` rather than binding to the interface address directly.
/// Binding to a specific unicast address makes macOS reject multicast sends
/// outright with `EHOSTUNREACH` — it consults the unicast route table, which
/// has no entry for 239.0.0.0/8.
fn bind_sender(iface: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    // Home networks are flat, but a couple of hops of headroom costs nothing.
    socket.set_multicast_ttl_v4(4)?;
    if !iface.is_unspecified() {
        socket.set_multicast_if_v4(&iface)?;
    }
    UdpSocket::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const SAMPLE: &[u8] = b"HTTP/1.1 200 OK\r\n\
        CACHE-CONTROL: max-age=1800\r\n\
        LOCATION: http://192.168.1.50:60006/upnp/desc/aios_device/aios_device.xml\r\n\
        ST: urn:schemas-denon-com:device:ACT-Denon:1\r\n\
        USN: uuid:5f9ec1b3-ed59-79ee-4530-0005cdf1a2b3::urn:schemas-denon-com:device:ACT-Denon:1\r\n\
        SERVER: KnOS/3.2 UPnP/1.0 Denon-Heos/524810b\r\n\r\n";

    fn addr() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))
    }

    #[test]
    fn parses_a_denon_reply() {
        let resp = SsdpResponse::parse(addr(), SAMPLE).expect("should parse");
        assert_eq!(
            resp.location(),
            Some("http://192.168.1.50:60006/upnp/desc/aios_device/aios_device.xml")
        );
        assert!(resp.usn().unwrap().starts_with("uuid:5f9ec1b3"));
        assert!(resp.server().unwrap().contains("Denon-Heos"));
    }

    #[test]
    fn header_lookup_is_case_insensitive_on_the_wire() {
        let raw = b"HTTP/1.1 200 OK\r\nLoCaTiOn: http://x/\r\n\r\n";
        let resp = SsdpResponse::parse(addr(), raw).unwrap();
        assert_eq!(resp.location(), Some("http://x/"));
    }

    #[test]
    fn rejects_non_http_datagrams() {
        // A NOTIFY advertisement is not a reply to our search.
        let raw = b"NOTIFY * HTTP/1.1\r\nNT: upnp:rootdevice\r\n\r\n";
        assert!(SsdpResponse::parse(addr(), raw).is_none());
        assert!(SsdpResponse::parse(addr(), b"\xff\xfe garbage").is_none());
    }

    #[test]
    fn request_carries_the_search_target() {
        let req = String::from_utf8(msearch_request("urn:test:1")).unwrap();
        assert!(req.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(req.contains("ST: urn:test:1\r\n"));
        assert!(req.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }
}
