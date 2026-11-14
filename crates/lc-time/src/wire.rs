//! The clock-sync datagram.
//!
//! One fixed 44-byte layout serves as both request and response (big-endian):
//!
//! ```text
//!  0  magic      "LCTS"
//!  4  version    u8   (= 1)
//!  5  kind       u8   (0 = request, 1 = response)
//!  6  reserved   u16
//!  8  seq        u32  echoed by the server; matches responses to requests
//! 12  clock_id   u64  timebase of t2/t3 (0 in requests)
//! 20  t1         i64  client clock at request send        (client-local ns)
//! 28  t2         i64  server clock at request receive     (cluster ns)
//! 36  t3         i64  server clock at response send       (cluster ns)
//! ```
//!
//! The client records `t4` (its clock at response receive) itself. Binary and
//! fixed-size on purpose: encoding must take the same, negligible time on
//! every exchange, because any time spent between taking a timestamp and the
//! packet leaving is measurement error.

use lc_proto::ClockId;

pub const MAGIC: [u8; 4] = *b"LCTS";
pub const VERSION: u8 = 1;
pub const PACKET_LEN: usize = 44;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPacket {
    pub kind: Kind,
    pub seq: u32,
    pub clock_id: ClockId,
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
}

impl SyncPacket {
    pub fn request(seq: u32, t1: i64) -> Self {
        SyncPacket {
            kind: Kind::Request,
            seq,
            clock_id: 0,
            t1,
            t2: 0,
            t3: 0,
        }
    }

    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0..4].copy_from_slice(&MAGIC);
        b[4] = VERSION;
        b[5] = match self.kind {
            Kind::Request => 0,
            Kind::Response => 1,
        };
        b[8..12].copy_from_slice(&self.seq.to_be_bytes());
        b[12..20].copy_from_slice(&self.clock_id.to_be_bytes());
        b[20..28].copy_from_slice(&self.t1.to_be_bytes());
        b[28..36].copy_from_slice(&self.t2.to_be_bytes());
        b[36..44].copy_from_slice(&self.t3.to_be_bytes());
        b
    }

    /// `None` for anything that is not a well-formed packet of our version.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != PACKET_LEN || b[0..4] != MAGIC || b[4] != VERSION {
            return None;
        }
        let kind = match b[5] {
            0 => Kind::Request,
            1 => Kind::Response,
            _ => return None,
        };
        let i64_at = |i: usize| i64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        Some(SyncPacket {
            kind,
            seq: u32::from_be_bytes(b[8..12].try_into().unwrap()),
            clock_id: u64::from_be_bytes(b[12..20].try_into().unwrap()),
            t1: i64_at(20),
            t2: i64_at(28),
            t3: i64_at(36),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let p = SyncPacket {
            kind: Kind::Response,
            seq: 0xDEAD_BEEF,
            clock_id: u64::MAX - 5,
            t1: -17,
            t2: i64::MAX,
            t3: 123_456_789_012,
        };
        assert_eq!(SyncPacket::decode(&p.encode()), Some(p));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(SyncPacket::decode(b"nope"), None);
        let mut b = SyncPacket::request(1, 2).encode();
        b[4] = 9; // wrong version
        assert_eq!(SyncPacket::decode(&b), None);
        let mut b = SyncPacket::request(1, 2).encode();
        b[5] = 7; // unknown kind
        assert_eq!(SyncPacket::decode(&b), None);
        assert_eq!(SyncPacket::decode(&[0u8; PACKET_LEN + 1]), None);
    }
}
