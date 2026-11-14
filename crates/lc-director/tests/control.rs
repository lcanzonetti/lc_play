//! The control channel as an outside client sees it: connect over a real
//! WebSocket, send commands, read the cluster status.

use std::net::SocketAddr;
use std::time::Duration;

use lc_director::{Director, DirectorConfig};
use lc_net::{Connection, NetError};
use lc_proto::{
    ClientMessage, Command, Hello, NodeId, Role, ServerMessage, NANOS_PER_SEC, PROTOCOL_VERSION,
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
