//! OSC show control: lets a lighting desk, a show controller or any OSC app
//! drive the transport.
//!
//! OSC (Open Sound Control) messages arrive as UDP datagrams and are mapped
//! onto the same [`Command`]s every other control surface uses — this module
//! adds a *way in*, not new behaviour.
//!
//! | Address          | Argument            | Effect |
//! |------------------|---------------------|--------|
//! | `/lcplay/play`   | —                   | play |
//! | `/lcplay/pause`  | —                   | pause |
//! | `/lcplay/stop`   | —                   | pause and return to 0 |
//! | `/lcplay/seek`   | seconds (number)    | jump to that show time |
//! | `/lcplay/rate`   | speed (1.0 = normal)| set playback rate |
//! | `/lcplay/loop`   | seconds, or 0 = off | loop the show at that length |
//!
//! For `play`, `pause` and `stop` a single numeric argument of `0` is
//! ignored: control surfaces send `1` when a button is pressed and `0` when
//! it is released, and a release must not trigger the action a second time.
//!
//! OSC over UDP has no acknowledgements and no authentication. That is
//! normal for show control on a closed network, and unsuitable for anything
//! else.

use std::sync::Arc;

use lc_proto::{Command, NANOS_PER_SEC};
use rosc::{OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;

use crate::Core;

/// Receive and execute OSC messages forever.
pub async fn run(socket: UdpSocket, core: Arc<Core>) {
    // OSC packets are small; 64 KiB is the UDP maximum.
    let mut buf = vec![0u8; 65_536];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("osc recv: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };
        match rosc::decoder::decode_udp(&buf[..len]) {
            Ok((_, packet)) => handle_packet(&core, packet, &from.to_string()),
            Err(e) => tracing::debug!("osc: undecodable packet from {from}: {e}"),
        }
    }
}

fn handle_packet(core: &Core, packet: OscPacket, from: &str) {
    match packet {
        OscPacket::Message(msg) => {
            let addr = msg.addr.clone();
            match commands_for(&msg) {
                Ok(commands) => {
                    for command in commands {
                        tracing::info!("osc {addr} from {from}");
                        if let Err(e) = core.command(command) {
                            tracing::warn!("osc {addr}: {e}");
                        }
                    }
                }
                Err(e) => tracing::warn!("osc {addr} from {from}: {e}"),
            }
        }
        // A bundle is just several messages sent together. (Its time tag —
        // "execute at…" — is ignored: the Director schedules on its own
        // cluster clock.)
        OscPacket::Bundle(bundle) => {
            for inner in bundle.content {
                handle_packet(core, inner, from);
            }
        }
    }
}

/// First argument as a number, whatever numeric type the sender used.
fn number(msg: &OscMessage) -> Option<f64> {
    match msg.args.first()? {
        OscType::Float(v) => Some(*v as f64),
        OscType::Double(v) => Some(*v),
        OscType::Int(v) => Some(*v as f64),
        OscType::Long(v) => Some(*v as f64),
        _ => None,
    }
}

/// Translate one OSC message into zero or more commands.
fn commands_for(msg: &OscMessage) -> Result<Vec<Command>, String> {
    // Button release (see module docs).
    let released = number(msg) == Some(0.0);
    let seconds = |what: &str| {
        number(msg)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| (v * NANOS_PER_SEC as f64).round() as i64)
            .ok_or_else(|| format!("expects {what} as a non-negative number"))
    };
    Ok(match msg.addr.as_str() {
        "/lcplay/play" | "/lcplay/pause" | "/lcplay/stop" if released => vec![],
        "/lcplay/play" => vec![Command::Play],
        "/lcplay/pause" => vec![Command::Pause],
        "/lcplay/stop" => vec![Command::Pause, Command::Seek { show_ns: 0 }],
        "/lcplay/seek" => vec![Command::Seek {
            show_ns: seconds("the show time in seconds")?,
        }],
        "/lcplay/rate" => {
            let rate = number(msg)
                .filter(|v| v.is_finite() && v.abs() <= 16.0)
                .ok_or("expects a speed between -16 and 16")?;
            vec![Command::SetRate {
                rate_permille: (rate * 1000.0).round() as i32,
            }]
        }
        "/lcplay/loop" => {
            let ns = seconds("the loop length in seconds (0 = off)")?;
            vec![Command::SetLoop {
                loop_ns: (ns > 0).then_some(ns),
            }]
        }
        other => return Err(format!("unknown address '{other}'")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.into(),
            args,
        }
    }

    #[test]
    fn transport_addresses_map_to_commands() {
        assert_eq!(
            commands_for(&msg("/lcplay/play", vec![])),
            Ok(vec![Command::Play])
        );
        assert_eq!(
            commands_for(&msg("/lcplay/stop", vec![])),
            Ok(vec![Command::Pause, Command::Seek { show_ns: 0 }])
        );
        assert_eq!(
            commands_for(&msg("/lcplay/seek", vec![OscType::Float(12.5)])),
            Ok(vec![Command::Seek {
                show_ns: 12_500_000_000
            }])
        );
        // Integer arguments work too (many desks only send ints).
        assert_eq!(
            commands_for(&msg("/lcplay/loop", vec![OscType::Int(30)])),
            Ok(vec![Command::SetLoop {
                loop_ns: Some(30 * NANOS_PER_SEC)
            }])
        );
        assert_eq!(
            commands_for(&msg("/lcplay/loop", vec![OscType::Int(0)])),
            Ok(vec![Command::SetLoop { loop_ns: None }])
        );
        assert_eq!(
            commands_for(&msg("/lcplay/rate", vec![OscType::Double(0.5)])),
            Ok(vec![Command::SetRate { rate_permille: 500 }])
        );
    }

    #[test]
    fn button_release_does_not_fire_again() {
        assert_eq!(
            commands_for(&msg("/lcplay/play", vec![OscType::Float(1.0)])),
            Ok(vec![Command::Play])
        );
        assert_eq!(
            commands_for(&msg("/lcplay/play", vec![OscType::Float(0.0)])),
            Ok(vec![])
        );
    }

    #[test]
    fn bad_input_is_refused_with_a_reason() {
        assert!(commands_for(&msg("/lcplay/seek", vec![])).is_err());
        assert!(commands_for(&msg("/lcplay/seek", vec![OscType::Float(-1.0)])).is_err());
        assert!(commands_for(&msg("/lcplay/rate", vec![OscType::Float(f32::NAN)])).is_err());
        assert!(commands_for(&msg("/other", vec![])).is_err());
    }
}
