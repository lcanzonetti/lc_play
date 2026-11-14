//! The control channel as an outside client sees it: connect over a real
//! WebSocket, send commands, read the cluster status.

use std::net::SocketAddr;
use std::time::Duration;

use lc_director::{Director, DirectorConfig};
use lc_net::{Connection, NetError};
use lc_proto::{
    ClientMessage, Command, Hello, NodeId, Role, RunnerHello, ServerMessage, TransportState,
    NANOS_PER_SEC, PROTOCOL_VERSION,
};

async fn start() -> Director {
    Director::start(DirectorConfig {
        control_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        sync_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        discovery: None,
        ..Default::default()
    })
    .await
    .unwrap()
}

fn hello(protocol: u32) -> Hello {
    Hello {
        protocol,
        node_id: NodeId::random(),
        role: Role::Controller,
        name: "test".into(),
        runner: None,
    }
}

/// The first cluster status built at or after cluster time `not_before`.
/// (The Director also pushes a status every second; an older one may already
/// be in flight when we ask.)
async fn next_status(conn: &mut Connection, not_before: i64) -> lc_proto::ClusterStatus {
    loop {
        match conn.recv_timeout(Duration::from_secs(3)).await.unwrap() {
            ServerMessage::Cluster(s) if s.cluster_ns >= not_before => return s,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn controller_drives_the_transport() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    let (mut conn, welcome) = Connection::connect(&url, || hello(PROTOCOL_VERSION))
        .await
        .unwrap();
    assert_eq!(welcome.clock_id, director.core().clock.clock_id());
    assert!(!welcome.transport.playing);

    for command in [
        Command::Seek {
            show_ns: 10 * NANOS_PER_SEC,
        },
        Command::Play,
    ] {
        conn.send(&ClientMessage::Command { command })
            .await
            .unwrap();
    }
    // Both changes are broadcast back to us as transport states…
    let mut epochs = Vec::new();
    while epochs.len() < 2 {
        if let ServerMessage::Transport { state, .. } =
            conn.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            epochs.push(state.epoch);
        }
    }
    assert_eq!(epochs, [1, 2]);

    // …and once the lead time has passed, the show is running from 10 s.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asked_at = director.core().clock.now_ns();
    conn.send(&ClientMessage::Command {
        command: Command::Status,
    })
    .await
    .unwrap();
    let status = next_status(&mut conn, asked_at).await;
    assert!(status.transport.playing);
    assert!(status.show_ns > 10 * NANOS_PER_SEC && status.show_ns < 11 * NANOS_PER_SEC);
    assert_eq!(status.nodes.len(), 1, "we are the only node");

    conn.close().await;
    director.shutdown().await;
}

#[tokio::test]
async fn invalid_commands_are_refused_without_dropping_the_link() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    let (mut conn, _) = Connection::connect(&url, || hello(PROTOCOL_VERSION))
        .await
        .unwrap();

    let bad = [
        Command::Seek { show_ns: -1 },
        Command::SetRate {
            rate_permille: 1_000_000,
        },
        // |i32::MIN| does not fit in an i32; the limit check must not wrap.
        Command::SetRate {
            rate_permille: i32::MIN,
        },
        Command::LoadShow {
            document: serde_json::json!("not an object"),
        },
    ];
    for command in bad {
        conn.send(&ClientMessage::Command {
            command: command.clone(),
        })
        .await
        .unwrap();
        loop {
            match conn.recv_timeout(Duration::from_secs(3)).await.unwrap() {
                ServerMessage::Error { .. } => break,
                ServerMessage::Cluster(_) => continue, // periodic status
                other => panic!("{command:?} was not refused: {other:?}"),
            }
        }
    }
    assert_eq!(director.core().transport().epoch, 0, "nothing changed");

    // The link still works.
    conn.send(&ClientMessage::Ping).await.unwrap();
    loop {
        match conn.recv_timeout(Duration::from_secs(3)).await.unwrap() {
            ServerMessage::Pong => break,
            _ => continue,
        }
    }
    director.shutdown().await;
}

#[tokio::test]
async fn mismatched_protocol_version_is_refused() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    match Connection::connect(&url, || hello(PROTOCOL_VERSION + 1)).await {
        Err(NetError::Refused(message)) => {
            assert!(message.contains("protocol version"), "{message}")
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("connection was accepted"),
    }
    director.shutdown().await;
}

#[tokio::test]
async fn state_survives_a_restart_but_comes_back_paused() {
    let dir = std::env::temp_dir().join(format!("lcplay-test-persist-{}", NodeId::random()));
    let cfg = || DirectorConfig {
        control_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        sync_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        discovery: None,
        state_dir: Some(dir.clone()),
        ..Default::default()
    };

    let first = Director::start(cfg()).await.unwrap();
    let id = first.core().id.clone();
    first
        .core()
        .command(Command::LoadShow {
            document: serde_json::json!({"name": "demo"}),
        })
        .unwrap();
    first
        .core()
        .command(Command::Seek {
            show_ns: 42 * NANOS_PER_SEC,
        })
        .unwrap();
    first.core().command(Command::Play).unwrap();
    first.shutdown().await;

    // No Runner kept the timebase alive, so the only safe reading of the
    // saved state is: same show, same position, not playing.
    let second = Director::start(cfg()).await.unwrap();
    let status = second.core().status();
    assert_eq!(second.core().id, id);
    assert_eq!(status.show_revision, Some(1));
    assert!(!status.transport.playing);
    assert_eq!(status.show_ns, 42 * NANOS_PER_SEC);
    assert_eq!(status.transport.epoch, 2, "epoch continues, never restarts");

    second.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Runner that kept playing while the Director was away, as it introduces
/// itself: following `clock_id`, executing `transport`.
fn holdover_runner(name: &str, clock_id: u64, epoch: u64) -> Hello {
    Hello {
        role: Role::Runner,
        name: name.into(),
        runner: Some(RunnerHello {
            clock_id: Some(clock_id),
            cluster_now_ns: Some(500 * NANOS_PER_SEC),
            transport: Some(TransportState {
                epoch,
                playing: true,
                anchor_cluster_ns: 100 * NANOS_PER_SEC,
                ..TransportState::STOPPED
            }),
            ..Default::default()
        }),
        ..hello(PROTOCOL_VERSION)
    }
}

/// Wait for a transport message with exactly this epoch.
async fn expect_transport(conn: &mut Connection, epoch: u64) -> TransportState {
    loop {
        match conn.recv_timeout(Duration::from_secs(3)).await.unwrap() {
            ServerMessage::Transport { state, .. } if state.epoch == epoch => return state,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn a_runner_holding_a_newer_transport_brings_the_cluster_up_to_date() {
    // Runner A missed the last change (it is on epoch 4), Runner B has it
    // (epoch 5). A restarted Director hears A first.
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());

    let (mut a, welcome_a) = Connection::connect(&url, || holdover_runner("a", 77, 4))
        .await
        .unwrap();
    assert_eq!(welcome_a.clock_id, 77, "adopted A's timebase");
    assert_eq!(welcome_a.transport.epoch, 4);

    let (_b, welcome_b) = Connection::connect(&url, || holdover_runner("b", 77, 5))
        .await
        .unwrap();
    // The Director must not hand B an older state (B would ignore it and
    // the two Runners would stay apart) — it takes B's…
    assert_eq!(welcome_b.transport.epoch, 5);
    // …and brings A up to date.
    assert_eq!(expect_transport(&mut a, 5).await.epoch, 5);

    // The next operator command outranks both.
    director.core().command(Command::Pause).unwrap();
    assert!(!expect_transport(&mut a, 6).await.playing);
    director.shutdown().await;
}

#[tokio::test]
async fn a_start_that_no_runner_reaches_does_not_spoil_adoption() {
    let dir = std::env::temp_dir().join(format!("lcplay-test-unanchored-{}", NodeId::random()));
    let cfg = || DirectorConfig {
        control_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        sync_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        discovery: None,
        state_dir: Some(dir.clone()),
        ..Default::default()
    };

    // First life: a show is playing.
    let first = Director::start(cfg()).await.unwrap();
    let clock_id = first.core().clock.clock_id();
    first.core().command(Command::Play).unwrap();
    let playing = first.core().transport();
    first.shutdown().await;

    // Second life: started and stopped without anyone connecting (wrong
    // flags, network not up yet, a supervisor restart).
    Director::start(cfg()).await.unwrap().shutdown().await;

    // Third life: the Runner that kept the show going finally reaches it.
    let third = Director::start(cfg()).await.unwrap();
    let url = lc_net::control_url(third.control_addr());
    let (_runner, welcome) = Connection::connect(&url, || {
        holdover_runner("survivor", clock_id, playing.epoch)
    })
    .await
    .unwrap();
    assert_eq!(welcome.clock_id, clock_id, "still adopts its old timebase");
    assert!(welcome.transport.playing, "and the show keeps playing");
    assert_eq!(welcome.transport, playing, "on the transport it saved");

    third.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shows_are_matched_by_id_not_by_revision() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    let document = serde_json::json!({ "name": "new show" });
    director
        .core()
        .command(Command::LoadShow {
            document: document.clone(),
        })
        .unwrap();
    let current_id = director.core().show().unwrap().id;

    let runner_with = |show_id: Option<String>| Hello {
        role: Role::Runner,
        runner: Some(RunnerHello {
            // Same revision number as the Director's show in both cases.
            show_revision: Some(1),
            show_id,
            ..Default::default()
        }),
        ..hello(PROTOCOL_VERSION)
    };

    // A Runner caching "revision 1" of some *other* show must get the show.
    let (mut stale, _) = Connection::connect(&url, || runner_with(Some("old-show".into())))
        .await
        .unwrap();
    match stale.recv_timeout(Duration::from_secs(3)).await.unwrap() {
        ServerMessage::Show {
            id, document: d, ..
        } => {
            assert_eq!((id, d), (current_id.clone(), document));
        }
        other => panic!("expected the show, got {other:?}"),
    }

    // A Runner that has exactly this show is not sent it again.
    let (mut current, _) = Connection::connect(&url, || runner_with(Some(current_id)))
        .await
        .unwrap();
    current.send(&ClientMessage::Ping).await.unwrap();
    assert_eq!(
        current.recv_timeout(Duration::from_secs(3)).await.unwrap(),
        ServerMessage::Pong,
        "nothing but the pong arrives"
    );
    director.shutdown().await;
}
