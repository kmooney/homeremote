//! Wake-on-LAN.
//!
//! The Samsung drops its network stack in standby, so there is no socket left
//! to ask it to turn on — the only way back in is a magic packet at layer 2.
//! This is why discovery records the TV's MAC while it is awake.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::Result;
use crate::discovery::{MacAddr, local_broadcast_addrs};

/// Conventional WoL ports. Neither is listened to by anything; the packet is
/// consumed by the NIC, so send to both and let the hardware sort it out.
const WOL_PORTS: [u16; 2] = [9, 7];

/// A magic packet: six `0xFF` bytes then the target MAC sixteen times.
pub fn magic_packet(mac: MacAddr) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    let octets = mac.octets();
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&octets);
    }
    packet
}

/// Broadcast a magic packet on every local network.
///
/// Fire-and-forget by nature: nothing acknowledges a WoL packet, so a
/// successful return means "sent", not "the TV is on". Callers should poll for
/// reachability afterwards.
pub async fn wake(mac: MacAddr) -> Result<()> {
    let packet = magic_packet(mac);
    let socket = broadcast_socket()?;

    let mut sent = 0usize;
    for addr in local_broadcast_addrs() {
        for port in WOL_PORTS {
            let target = SocketAddr::V4(SocketAddrV4::new(addr, port));
            match socket.send_to(&packet, target).await {
                Ok(_) => sent += 1,
                Err(e) => tracing::debug!(%target, error = %e, "WoL send failed"),
            }
        }
    }

    if sent == 0 {
        return Err(crate::Error::Protocol(
            "could not send a Wake-on-LAN packet on any interface".into(),
        ));
    }
    tracing::debug!(%mac, sent, "sent Wake-on-LAN packets");
    Ok(())
}

fn broadcast_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    UdpSocket::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_has_the_right_shape() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let packet = magic_packet(mac);

        assert_eq!(&packet[..6], &[0xFF; 6], "must open with six 0xFF bytes");
        // Sixteen repetitions of the MAC follow.
        for i in 0..16 {
            let start = 6 + i * 6;
            assert_eq!(
                &packet[start..start + 6],
                &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
                "repetition {i} is wrong"
            );
        }
    }
}
