//! Discovery beacon: the small JSON datagram every node multicasts once a
//! second so that the others can find it without any configuration.

use serde::{Deserialize, Serialize};

use crate::{ClockId, NodeId, Role};

/// Multicast group for discovery. `239.255.0.0/16` is the IPv4
/// *organization-local* administratively scoped range (RFC 2365): routers do
/// not forward it beyond the site, which is what a show network wants.
/// The last two octets spell "LC" (76, 67).
pub const DISCOVERY_GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 76, 67);
/// UDP port for discovery beacons.
pub const DISCOVERY_PORT: u16 = 7400;

/// Fixed first field of every beacon, so stray datagrams on the group/port
/// are rejected cheaply.
pub const BEACON_MAGIC: &str = "lcplay";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    /// Always [`BEACON_MAGIC`].
    pub magic: String,
    pub protocol: u32,
    pub node_id: NodeId,
    pub role: Role,
    /// Human-readable name shown in the Producer ("Runner stage-left").
    pub name: String,
    /// TCP port of this node's control endpoint, if it serves one. The IP
    /// address is *not* in the beacon: receivers use the datagram's source
    /// address, which is correct even on multi-homed machines.
    #[serde(default)]
    pub control_port: Option<u16>,
    /// UDP port of the clock-sync responder (Directors only).
    #[serde(default)]
    pub sync_port: Option<u16>,
    /// Timebase this node serves (Director) or follows (Runner).
    #[serde(default)]
    pub clock_id: Option<ClockId>,
}

impl Beacon {
    pub fn new(node_id: NodeId, role: Role, name: impl Into<String>) -> Self {
        Beacon {
            magic: BEACON_MAGIC.to_string(),
            protocol: crate::PROTOCOL_VERSION,
            node_id,
            role,
            name: name.into(),
            control_port: None,
            sync_port: None,
            clock_id: None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // Serialising a plain struct of strings/ints cannot fail.
        serde_json::to_vec(self).expect("beacon serialises")
    }

    /// Parse a datagram. `None` for anything that is not a beacon of *our*
    /// protocol version — foreign traffic is expected on a shared network and
    /// is not an error.
    pub fn decode(bytes: &[u8]) -> Option<Beacon> {
        let b: Beacon = serde_json::from_slice(bytes).ok()?;
        (b.magic == BEACON_MAGIC && b.protocol == crate::PROTOCOL_VERSION).then_some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut b = Beacon::new(NodeId::random(), Role::Director, "main");
        b.control_port = Some(7401);
        b.clock_id = Some(42);
        assert_eq!(Beacon::decode(&b.encode()), Some(b));
    }

    #[test]
    fn rejects_foreign_and_mismatched_datagrams() {
        assert_eq!(Beacon::decode(b"hello"), None);
        let mut b = Beacon::new(NodeId::random(), Role::Runner, "r");
        b.protocol += 1;
        assert_eq!(Beacon::decode(&b.encode()), None);
        b.protocol -= 1;
        b.magic = "other".into();
        assert_eq!(Beacon::decode(&b.encode()), None);
    }
}
