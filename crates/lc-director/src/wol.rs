//! Wake-on-LAN: switching a computer on over the network.
//!
//! A "magic packet" is six `0xFF` bytes followed by the target's network
//! card address (MAC) sixteen times, sent as a UDP broadcast. A powered-off
//! computer whose network card and BIOS/UEFI have Wake-on-LAN enabled
//! recognises it and powers up.
//!
//! The packet is broadcast to the whole local network (`255.255.255.255`)
//! and to the directed broadcast of the Runner's last known address
//! (assuming a /24 network), on UDP ports 9 and 7, the two in common use.
//! It does not cross routers.

use std::net::{Ipv4Addr, UdpSocket};

/// Parse `aa:bb:cc:dd:ee:ff` (or with `-`).
fn parse_mac(mac: &str) -> Result<[u8; 6], String> {
    let bad = || format!("'{mac}' is not a network card address");
    let parts: Vec<&str> = mac.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(bad());
    }
    let mut out = [0u8; 6];
    for (byte, part) in out.iter_mut().zip(parts) {
        *byte = u8::from_str_radix(part, 16).map_err(|_| bad())?;
    }
    Ok(out)
}

fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

pub fn wake(mac: &str, last_ip: &str) -> Result<(), String> {
    let packet = magic_packet(parse_mac(mac)?);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Ok(ip) = last_ip.parse::<Ipv4Addr>() {
        let [a, b, c, _] = ip.octets();
        targets.push(Ipv4Addr::new(a, b, c, 255));
    }
    let mut sent = false;
    for target in targets {
        for port in [9, 7] {
            sent |= socket.send_to(&packet, (target, port)).is_ok();
        }
    }
    if sent {
        Ok(())
    } else {
        Err("could not send the broadcast (no network?)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac = parse_mac("01:23:45:67:89:AB").unwrap();
        let p = magic_packet(mac);
        assert_eq!(p.len(), 6 + 16 * 6);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &mac);
        assert_eq!(&p[96..102], &mac);
        assert_eq!(parse_mac("01-23-45-67-89-ab").unwrap(), mac);
        assert!(parse_mac("nonsense").is_err());
    }
}
