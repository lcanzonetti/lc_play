//! The UDP side of clock sync: the Director's responder and the Runner's
//! polling client.
//!
//! Sync has its own UDP socket and never shares the control connection: a
//! TCP stream delays everything behind one lost segment, and that delay would
//! be read as clock offset.
//!
//! Timestamps are taken in user space, right after `recv` and right before
//! `send`. Scheduler latency (tens of microseconds, occasionally more) thus
//! ends up in the measured delay — which is exactly what the estimator's
//! minimum-delay filter is there to reject. Kernel or hardware timestamping
//! (`SO_TIMESTAMPING`, PTP) would tighten this further and can replace this
//! module without touching the estimator.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::time::{sleep, timeout_at, Instant};

use crate::clock::{ClusterClock, MasterClock};
use crate::wire::{Kind, SyncPacket, PACKET_LEN};

/// Poll period while acquiring lock (fills the estimator window quickly).
const FAST_POLL: Duration = Duration::from_millis(125);
/// Poll period once locked. Oscillator drift changes over minutes; 2 Hz is
/// plenty and costs 88 bytes per second per Runner.
const STEADY_POLL: Duration = Duration::from_millis(500);
/// Samples after which the client drops from `FAST_POLL` to `STEADY_POLL`.
const FAST_POLL_SAMPLES: u32 = 32;
/// A response slower than this is useless as a measurement anyway.
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Answer sync requests forever. Run as a task on the Director.
pub async fn run_sync_server(socket: UdpSocket, clock: Arc<MasterClock>) {
    let mut buf = [0u8; 128];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                // E.g. an ICMP "port unreachable" surfaced by the OS for an
                // earlier reply. Not fatal; just don't spin on it.
                tracing::debug!("sync server recv: {e}");
                sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        // t2 first: everything between the packet's arrival and this line is
        // measurement error.
        let t2 = clock.now_ns();
        let Some(req) = SyncPacket::decode(&buf[..len]) else {
            continue;
        };
        if req.kind != Kind::Request {
            continue;
        }
        let mut resp = SyncPacket {
            kind: Kind::Response,
            seq: req.seq,
            clock_id: clock.clock_id(),
            t1: req.t1,
            t2,
            t3: 0,
        };
        // t3 last, immediately before the send.
        resp.t3 = clock.now_ns();
        if let Err(e) = socket.send_to(&resp.encode(), from).await {
            tracing::debug!("sync server send to {from}: {e}");
        }
    }
}

/// Poll the Director named by `target` and feed `clock`. Run as a task on a
/// Runner.
///
/// `target` is a watch channel so the agent can point the client at a
/// (different) Director, or at nothing, at any time. The task ends when the
/// sender is dropped. While the Director is unreachable the client simply
/// keeps asking; the clock runs in holdover meanwhile and re-converges as
/// soon as answers return.
pub async fn run_sync_client(
    clock: Arc<ClusterClock>,
    mut target: watch::Receiver<Option<SocketAddr>>,
) {
    loop {
        let Some(addr) = *target.borrow_and_update() else {
            if target.changed().await.is_err() {
                return;
            }
            continue;
        };
        let bind = if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = match UdpSocket::bind(bind).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("sync client: cannot bind UDP socket: {e}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        tracing::debug!("sync client polling {addr}");

        let mut seq: u32 = 0;
        let mut buf = [0u8; 128];
        'poll: loop {
            seq = seq.wrapping_add(1);
            let t1 = clock.local_now_ns();
            if socket
                .send_to(&SyncPacket::request(seq, t1).encode(), addr)
                .await
                .is_ok()
            {
                let deadline = Instant::now() + RESPONSE_TIMEOUT;
                // Read until *our* response arrives; late answers to earlier
                // requests are discarded (their t4 would be meaningless).
                while let Ok(Ok((len, from))) =
                    timeout_at(deadline, socket.recv_from(&mut buf)).await
                {
                    let t4 = clock.local_now_ns();
                    // Matched by content (seq + our own t1, a nanosecond
                    // timestamp nobody else knows), not by source address: a
                    // multi-homed Director may answer from another of its
                    // addresses, and dropping those would leave the Runner
                    // unsynced with no visible cause.
                    let _ = from;
                    if len != PACKET_LEN {
                        continue;
                    }
                    match SyncPacket::decode(&buf[..len]) {
                        Some(p) if p.kind == Kind::Response && p.seq == seq && p.t1 == t1 => {
                            clock.ingest(p.clock_id, t1, p.t2, p.t3, t4);
                            break;
                        }
                        _ => continue,
                    }
                }
            }

            let period = if clock.quality().samples < FAST_POLL_SAMPLES {
                FAST_POLL
            } else {
                STEADY_POLL
            };
            tokio::select! {
                _ = sleep(period) => {}
                changed = target.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break 'poll; // re-read the target (and re-bind for its address family)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{MonotonicClock, SkewedClock};

    /// Real sockets on loopback: a Runner whose clock is 1 h off and 200 ppm
    /// fast must converge onto the Director's clock.
    #[tokio::test]
    async fn client_locks_to_server_over_loopback() {
        let base = Arc::new(MonotonicClock::new());
        let master = Arc::new(MasterClock::new(base.clone(), 0xC10C));
        let server_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let server = tokio::spawn(run_sync_server(server_sock, master.clone()));

        let runner_local = Arc::new(SkewedClock::new(base, 3_600_000_000_000, 200.0));
        let clock = Arc::new(ClusterClock::new(runner_local));
        clock.follow(0xC10C);
        let (tx, rx) = watch::channel(Some(server_addr));
        let client = tokio::spawn(run_sync_client(clock.clone(), rx));

        // Fast polling gives ~8 samples a second.
        let deadline = Instant::now() + Duration::from_secs(10);
        while clock.quality().samples < 16 {
            assert!(
                Instant::now() < deadline,
                "no sync samples: {:?}",
                clock.quality()
            );
            sleep(Duration::from_millis(50)).await;
        }
        let err = (clock.now_ns() - master.now_ns()).abs();
        assert!(
            err < 2_000_000,
            "clock error {err} ns, {:?}",
            clock.quality()
        );

        drop(tx);
        client.await.unwrap(); // ends when the target sender is dropped
        server.abort();
    }
}
