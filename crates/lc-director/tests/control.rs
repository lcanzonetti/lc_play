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
        conn.send(&ClientMessage::Command {
            command,
            timeline: None,
        })
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
        timeline: None,
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
            tag: None,
        },
    ];
    for command in bad {
        conn.send(&ClientMessage::Command {
            command: command.clone(),
            timeline: None,
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
            tag: None,
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
            tag: None,
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

/// Load a show given as JSON into a fresh Director.
async fn director_with_show(document: serde_json::Value) -> Director {
    let director = start().await;
    director
        .core()
        .command(Command::LoadShow {
            document,
            tag: None,
        })
        .unwrap();
    director
}

#[tokio::test]
async fn auxiliary_timelines_have_their_own_transport() {
    let director = director_with_show(serde_json::json!({
        "tasks": [ { "id": "intro", "name": "Intro" } ]
    }))
    .await;
    let core = director.core();

    // A task that was never started is not even listed: it is off stage.
    assert!(core.status().tasks.is_empty());

    core.command_on(Command::Play, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let status = core.status();
    let intro = status.tasks["intro"];
    assert!(intro.active && intro.playing);
    assert!(intro.show_time_at(status.cluster_ns) > 0);
    assert!(!status.transport.playing, "the main timeline did not move");
    assert_eq!(status.show_ns, 0);

    // Stopping a task takes it off stage and rewinds it…
    core.command_on(Command::Stop, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = core.status();
    assert!(!status.tasks["intro"].active);
    assert_eq!(status.tasks["intro"].show_time_at(status.cluster_ns), 0);

    // Positioning a stopped task brings it back on stage, paused there.
    core.command_on(
        Command::Seek {
            show_ns: 2 * NANOS_PER_SEC,
        },
        Some("intro"),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = core.status();
    let intro = status.tasks["intro"];
    assert!(intro.active && !intro.playing, "on stage, paused");
    assert_eq!(intro.show_time_at(status.cluster_ns), 2 * NANOS_PER_SEC);
    core.command_on(Command::Stop, Some("intro")).unwrap();

    // Pausing a stopped task cues it up: on stage, paused at the start.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!core.status().tasks["intro"].active);
    core.command_on(Command::Pause, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = core.status();
    let intro = status.tasks["intro"];
    assert!(intro.active && !intro.playing, "on stage, paused");
    assert_eq!(intro.show_time_at(status.cluster_ns), 0);
    core.command_on(Command::Stop, Some("intro")).unwrap();

    // A Pause right behind a Stop (inside its lead time, while the task is
    // still on stage) only pauses: the task still goes off stage, it does
    // not come back cued up at 0.
    tokio::time::sleep(Duration::from_millis(250)).await;
    core.command_on(Command::Play, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(core.status().tasks["intro"].playing);
    core.command_on(Command::Stop, Some("intro")).unwrap();
    core.command_on(Command::Pause, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let intro = core.status().tasks["intro"];
    assert!(!intro.active && !intro.playing, "off stage: {intro:?}");

    // …whereas stopping the main timeline only rewinds it.
    core.command(Command::Seek {
        show_ns: 5 * NANOS_PER_SEC,
    })
    .unwrap();
    core.command(Command::Stop).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = core.status();
    assert!(status.transport.active && !status.transport.playing);
    assert_eq!(status.show_ns, 0);

    assert!(core.command_on(Command::Play, Some("nope")).is_err());
    // "main" is the main timeline by name.
    core.command_on(Command::Play, Some("main")).unwrap();
    assert!(core.transport().playing);
    director.shutdown().await;
}

#[tokio::test]
async fn a_pause_right_behind_a_play_leaves_the_task_on_stage_paused_at_the_start() {
    let director = director_with_show(serde_json::json!({
        "tasks": [ { "id": "intro", "name": "Intro" } ]
    }))
    .await;
    let core = director.core();

    // The task is off stage now; a Play is pending (inside its lead time)
    // when the operator pauses. The Pause is not turned into a hold of an
    // on-stage task, nor does it cancel the Play's coming on stage: it lands
    // on the pending Play and pauses it where it starts.
    core.command_on(Command::Play, Some("intro")).unwrap();
    core.command_on(Command::Pause, Some("intro")).unwrap();
    assert!(
        !core.status().tasks["intro"].active,
        "both still pending: off stage"
    );
    tokio::time::sleep(Duration::from_millis(250)).await; // past the lead time
    let status = core.status();
    let intro = status.tasks["intro"];
    assert!(
        intro.active && !intro.playing,
        "on stage, paused: {intro:?}"
    );
    // At the start, give or take the moment between the two commands.
    let at = intro.show_time_at(status.cluster_ns);
    assert!(
        (0..50_000_000).contains(&at),
        "paused at the start, not at {at} ns"
    );
    // And it stays there.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let status = core.status();
    assert_eq!(status.tasks["intro"].show_time_at(status.cluster_ns), at);
    director.shutdown().await;
}

#[tokio::test]
async fn a_pause_cue_stops_exactly_on_the_cue() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "hold", "at_s": 0.5, "action": "pause" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let status = core.status();
    assert!(!status.transport.playing, "paused by the cue");
    assert_eq!(
        status.show_ns,
        NANOS_PER_SEC / 2,
        "on the cue to the nanosecond, not wherever the scheduler happened to look"
    );

    // Playing on from a pause cue must not trip over the same cue again.
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = core.status();
    assert!(status.transport.playing);
    assert!(status.show_ns > NANOS_PER_SEC / 2);
    director.shutdown().await;
}

#[tokio::test]
async fn a_pause_cue_leaves_a_stopped_task_off_stage() {
    // Unlike the operator's pause, a programmed one does not bring a stopped
    // task on stage (as a programmed jump does not).
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "hold", "at_s": 0.3, "action": "pause", "target": "sting" } ],
        "tasks": [ { "id": "sting" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let status = core.status();
    let sting = status.tasks["sting"];
    assert!(sting.epoch > 0, "the cue fired: {sting:?}");
    assert!(
        !sting.active && !sting.playing,
        "still off stage: {sting:?}"
    );
    director.shutdown().await;
}

#[tokio::test]
async fn a_jump_cue_brings_a_stopped_task_on_stage_paused_there() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "cue", "at_s": 0.3, "action": "jump", "to_s": 7.0, "target": "sting" } ],
        "tasks": [ { "id": "sting" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let status = core.status();
    let sting = status.tasks["sting"];
    assert!(
        sting.active && !sting.playing,
        "on stage, paused: {sting:?}"
    );
    assert_eq!(sting.show_time_at(status.cluster_ns), 7 * NANOS_PER_SEC);
    // The operator's Pause now holds it there: it is not stopped.
    core.command_on(Command::Pause, Some("sting")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = core.status();
    let sting = status.tasks["sting"];
    assert!(sting.active && !sting.playing);
    assert_eq!(sting.show_time_at(status.cluster_ns), 7 * NANOS_PER_SEC);
    director.shutdown().await;
}

#[tokio::test]
async fn cues_can_start_a_task_and_jump() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [
            { "id": "go",   "at_s": 0.3, "action": "play", "target": "sting" },
            { "id": "back", "at_s": 0.6, "action": "jump", "to_s": 0.1 }
        ],
        "tasks": [ { "id": "sting" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();

    tokio::time::sleep(Duration::from_millis(550)).await; // main is at ~0.45 s
    let status = core.status();
    assert!(
        status
            .tasks
            .get("sting")
            .is_some_and(|t| t.active && t.playing),
        "the cue at 0.3 s started the task: {:?}",
        status.tasks
    );

    tokio::time::sleep(Duration::from_millis(450)).await; // ~0.9 s after play
    let status = core.status();
    assert!(!status.transport.playing, "a jump pauses");
    assert_eq!(status.show_ns, 100_000_000, "jumped back to 0.1 s at 0.6 s");
    director.shutdown().await;
}

#[tokio::test]
async fn a_runner_action_reaches_the_runner_it_names() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    let runner = |name: &str| Hello {
        role: Role::Runner,
        name: name.into(),
        runner: Some(RunnerHello::default()),
        ..hello(PROTOCOL_VERSION)
    };
    let (mut left, _) = Connection::connect(&url, || runner("left")).await.unwrap();
    let left_id = director.core().status().nodes[0].node_id.clone();

    let reload = |runner: &str| Command::RunnerAction {
        runner: runner.into(),
        action: lc_proto::RunnerAction::ReloadOutputs,
    };
    assert!(director.core().command(reload("nobody")).is_err());
    director.core().command(reload("left")).unwrap();
    loop {
        match left.recv_timeout(Duration::from_secs(3)).await.unwrap() {
            ServerMessage::RunnerAction { node_id, action } => {
                assert_eq!(node_id, left_id);
                assert_eq!(action, lc_proto::RunnerAction::ReloadOutputs);
                break;
            }
            _ => continue,
        }
    }
    director.shutdown().await;
}

#[tokio::test]
async fn a_command_just_before_a_cue_does_not_cancel_it() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "hold", "at_s": 1.0, "action": "pause" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    // 0.8 s in: the pause is already scheduled (300 ms look-ahead). An
    // unrelated command now replaces the transport state it was derived from.
    tokio::time::sleep(Duration::from_millis(900)).await;
    core.command(Command::SetRate {
        rate_permille: 1000,
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;

    let status = core.status();
    assert!(!status.transport.playing, "the cue still paused the show");
    assert_eq!(status.show_ns, NANOS_PER_SEC);
    director.shutdown().await;
}

#[tokio::test]
async fn a_jump_cue_pauses_where_it_lands_and_fires_again_on_the_next_pass() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "back", "at_s": 0.5, "action": "jump", "to_s": 0.2 } ]
    }))
    .await;
    let core = director.core();
    for _ in 0..3 {
        core.command(Command::Play).unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let status = core.status();
        assert!(!status.transport.playing, "the jump paused the show");
        assert_eq!(status.show_ns, 200_000_000, "on the frame it jumped to");
    }
    director.shutdown().await;
}

#[tokio::test]
async fn a_jump_cue_onto_itself_lets_playback_go_on() {
    // A jump to its own position pauses there; Play then plays on past it
    // instead of firing it again at once.
    let director = director_with_show(serde_json::json!({
        "control_cues": [ { "id": "hold", "at_s": 0.3, "action": "jump", "to_s": 0.3 } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = core.status();
    assert!(!status.transport.playing);
    assert_eq!(status.show_ns, 300_000_000);
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let status = core.status();
    assert!(status.transport.playing, "played on");
    assert!(status.show_ns > 300_000_000);
    director.shutdown().await;
}

#[tokio::test]
async fn a_cue_at_the_start_position_fires_unless_it_would_trap_playback() {
    let director = director_with_show(serde_json::json!({
        "control_cues": [
            { "id": "with-show", "at_s": 0, "action": "play", "target": "bed" },
            { "id": "hold",      "at_s": 0, "action": "pause" }
        ],
        "tasks": [ { "id": "bed" } ]
    }))
    .await;
    let core = director.core();
    core.command(Command::Play).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = core.status();
    assert!(
        status
            .tasks
            .get("bed")
            .is_some_and(|t| t.active && t.playing),
        "a play cue at 0 starts its task with the show"
    );
    assert!(
        status.transport.playing && status.show_ns > 0,
        "a pause cue at the start position does not stop playback from starting"
    );
    director.shutdown().await;
}

#[tokio::test]
async fn a_task_that_leaves_and_returns_keeps_rising_epochs() {
    let with_task = serde_json::json!({ "tasks": [ { "id": "intro" } ] });
    let director = director_with_show(with_task.clone()).await;
    let core = director.core();
    for command in [Command::Play, Command::Pause, Command::Play] {
        core.command_on(command, Some("intro")).unwrap();
    }
    let before = core.status().tasks["intro"].epoch;

    // The task disappears from the show (deleted)…
    core.command(Command::LoadShow {
        document: serde_json::json!({}),
        tag: None,
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(core.command_on(Command::Play, Some("intro")).is_err());
    let stopped = core.status().tasks["intro"];
    assert!(!stopped.active, "a removed task is taken off stage");
    assert!(stopped.epoch > before);

    // …and comes back (undo). Anything that still holds its old transport —
    // a Runner, a Producer — only accepts states with a higher epoch.
    core.command(Command::LoadShow {
        document: with_task,
        tag: None,
    })
    .unwrap();
    core.command_on(Command::Play, Some("intro")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await; // past the lead time
    let resumed = core.status().tasks["intro"];
    assert!(resumed.playing && resumed.epoch > stopped.epoch);
    director.shutdown().await;
}

/// A Runner's health report with a measured refresh.
fn health_with_refresh(vblank_ns: i64, exact: bool) -> lc_proto::Health {
    lc_proto::Health {
        cluster_ns: vblank_ns,
        show_ns: 0,
        frame: 0,
        transport_epoch: 0,
        show_revision: None,
        sync: Default::default(),
        fps: None,
        monitors: Vec::new(),
        gpu: None,
        outputs: 1,
        machine: None,
        standby: false,
        output_health: Vec::new(),
        downloads: Vec::new(),
        audio_devices: Vec::new(),
        audio: None,
        vblank: Some(lc_proto::VblankReport {
            vblank_ns,
            period_ps: 16_683_333_333,
            exact,
        }),
    }
}

/// The next refresh grid sent to `conn`.
async fn next_grid(conn: &mut Connection) -> lc_proto::VblankGrid {
    loop {
        if let ServerMessage::Grid { grid } =
            conn.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            return grid;
        }
    }
}

#[tokio::test]
async fn the_refresh_grid_comes_from_the_best_measured_runner() {
    let director = start().await;
    let url = lc_net::control_url(director.control_addr());
    let runner = |id: &str| Hello {
        role: Role::Runner,
        node_id: NodeId(id.into()),
        name: id.into(),
        runner: Some(RunnerHello::default()),
        ..hello(PROTOCOL_VERSION)
    };
    let (mut a, _) = Connection::connect(&url, || runner("a")).await.unwrap();
    let (mut b, _) = Connection::connect(&url, || runner("b")).await.unwrap();

    // "a" sorts first, but only "b" reads its refreshes from the driver:
    // the grid is built from "b".
    let base = 1_000 * NANOS_PER_SEC;
    b.send(&ClientMessage::Health(health_with_refresh(base, true)))
        .await
        .unwrap();
    let grid = next_grid(&mut a).await;
    assert_eq!(grid.origin_ns, base);
    assert_eq!(grid.generation, 1);

    // A report from a Runner that is not the reference changes nothing, nor
    // does the reference staying on its grid.
    a.send(&ClientMessage::Health(health_with_refresh(
        base + 5_000_000,
        false,
    )))
    .await
    .unwrap();
    b.send(&ClientMessage::Health(health_with_refresh(
        grid.instant(600) + 50_000,
        true,
    )))
    .await
    .unwrap();
    // A Runner connecting now is given the grid straight away.
    let (mut c, _) = Connection::connect(&url, || runner("c")).await.unwrap();
    assert_eq!(next_grid(&mut c).await, grid);
    let status = director.core().status();
    assert_eq!(status.grid, Some(grid));
    assert_eq!(status.grid_source, Some(NodeId("b".into())));
    director.shutdown().await;
}

/// The next show sent to `conn`.
async fn next_show(conn: &mut Connection) -> serde_json::Value {
    loop {
        if let ServerMessage::Show { document, .. } =
            conn.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            return document;
        }
    }
}

#[tokio::test]
async fn two_producers_editing_at_once_both_keep_their_changes() {
    let director = director_with_show(serde_json::json!({
        "tracks": [ { "id": "t1", "cues": [ { "id": "c1", "x": 0 } ] } ],
        "stage": { "width": 1920, "height": 1080,
                   "displays": [ { "id": "d1", "canvas_rect": { "x": 0, "y": 0, "w": 1920, "h": 1080 } } ] }
    }))
    .await;
    let url = lc_net::control_url(director.control_addr());
    let producer = |id: &str| Hello {
        role: Role::Producer,
        node_id: NodeId(id.into()),
        name: id.into(),
        ..hello(PROTOCOL_VERSION)
    };
    let (mut alice, _) = Connection::connect(&url, || producer("alice"))
        .await
        .unwrap();
    let (mut bob, _) = Connection::connect(&url, || producer("bob")).await.unwrap();
    let runner = Hello {
        role: Role::Runner,
        runner: Some(RunnerHello::default()),
        ..hello(PROTOCOL_VERSION)
    };
    let (mut runner, _) = Connection::connect(&url, || runner.clone()).await.unwrap();

    // Both edit, written against the same show, sent back to back.
    let edit = |ops: serde_json::Value, by: &str| ClientMessage::Command {
        command: Command::EditShow {
            ops: serde_json::from_value(ops).unwrap(),
            tag: Some(serde_json::json!({ "by": by, "n": 1 })),
        },
        timeline: None,
    };
    alice
        .send(&edit(
            serde_json::json!([{ "op": "set",
                "path": ["stage", "displays", { "id": "d1" }, "warp_corners"],
                "value": [[0.0, 0.0], [1.0, 0.0], [0.9, 1.0], [0.1, 1.0]] }]),
            "alice",
        ))
        .await
        .unwrap();
    bob.send(&edit(
        serde_json::json!([{ "op": "set",
            "path": ["tracks", { "id": "t1" }, "cues", { "id": "c1" }, "x"], "value": 500 }]),
        "bob",
    ))
    .await
    .unwrap();

    // Whatever the order, the shows the Runner gets end with one that holds
    // both (the first it gets may be the one sent when it connected).
    let mut last = next_show(&mut runner).await;
    for _ in 0..3 {
        if last["tracks"][0]["cues"][0]["x"] == 500
            && last["stage"]["displays"][0]["warp_corners"].is_array()
        {
            break;
        }
        last = next_show(&mut runner).await;
    }
    assert_eq!(last["tracks"][0]["cues"][0]["x"], 500);
    assert_eq!(last["stage"]["displays"][0]["warp_corners"][2][0], 0.9);
    // The show records, per editor, the last edit applied.
    assert_eq!(last["_applied"]["alice"][0], 1);
    assert_eq!(last["_applied"]["bob"][0], 1);

    // An edit sent again (after a reconnect) is not applied a second time:
    // here it would undo Bob's newer change.
    bob.send(&edit(
        serde_json::json!([{ "op": "set",
            "path": ["tracks", { "id": "t1" }, "cues", { "id": "c1" }, "x"], "value": 700 }]),
        "bob2",
    ))
    .await
    .unwrap();
    alice
        .send(&ClientMessage::Command {
            command: Command::EditShow {
                ops: serde_json::from_value(serde_json::json!([{ "op": "set",
                    "path": ["tracks", { "id": "t1" }, "cues", { "id": "c1" }, "x"], "value": 1 }]))
                .unwrap(),
                tag: Some(serde_json::json!({ "by": "alice", "n": 1 })),
            },
            timeline: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        director.core().show().unwrap().document["tracks"][0]["cues"][0]["x"],
        700,
        "Alice's re-sent edit 1 was ignored"
    );

    // Presence goes to the other Producers, not to Runners; leaving says so.
    alice
        .send(&ClientMessage::Presence(lc_proto::Presence {
            name: "Alice".into(),
            color: "#e44".into(),
            selection: Some(serde_json::json!({ "type": "display", "id": "d1" })),
            dragging: false,
        }))
        .await
        .unwrap();
    loop {
        if let ServerMessage::Presence { node_id, presence } =
            bob.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            assert_eq!(node_id, NodeId("alice".into()));
            assert_eq!(presence.unwrap().name, "Alice");
            break;
        }
    }
    // A box selection of many cues is relayed whole (it is well over the
    // old 512-byte limit) ...
    let many: Vec<String> = (0..200).map(|i| format!("cue{i:05}")).collect();
    let presence = |ids: &[String]| {
        ClientMessage::Presence(lc_proto::Presence {
            name: "Alice".into(),
            color: "#e44".into(),
            selection: Some(serde_json::json!({ "type": "cue", "id": ids[0], "ids": ids })),
            dragging: false,
        })
    };
    tokio::time::sleep(Duration::from_millis(250)).await; // presence is rate-limited
    alice.send(&presence(&many)).await.unwrap();
    loop {
        if let ServerMessage::Presence { presence, .. } =
            bob.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            let sel = presence.unwrap().selection.unwrap();
            assert_eq!(sel["ids"].as_array().unwrap().len(), 200);
            break;
        }
    }
    // ... one past the limit is not.
    let too_many: Vec<String> = (0..400).map(|i| format!("cue{i:05}")).collect();
    assert!(
        serde_json::json!({ "type": "cue", "id": too_many[0], "ids": too_many })
            .to_string()
            .len()
            > lc_director::MAX_PRESENCE_SELECTION_BYTES
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    alice.send(&presence(&too_many)).await.unwrap();
    while let Ok(msg) = bob.recv_timeout(Duration::from_millis(400)).await {
        assert!(
            !matches!(msg, ServerMessage::Presence { .. }),
            "an oversized selection was relayed"
        );
    }
    drop(alice);
    loop {
        if let ServerMessage::Presence { node_id, presence } =
            bob.recv_timeout(Duration::from_secs(3)).await.unwrap()
        {
            assert_eq!(node_id, NodeId("alice".into()));
            assert!(presence.is_none(), "Alice has left");
            break;
        }
    }
    // The Runner never saw a presence message.
    while let Ok(msg) = runner.recv_timeout(Duration::from_millis(200)).await {
        assert!(!matches!(msg, ServerMessage::Presence { .. }));
    }
    director.shutdown().await;
}

#[tokio::test]
async fn an_edit_without_a_show_or_that_breaks_it_is_refused() {
    let director = start().await;
    let edit = |ops: serde_json::Value| Command::EditShow {
        ops: serde_json::from_value(ops).unwrap(),
        tag: None,
    };
    let set_name = serde_json::json!([{ "op": "set", "path": ["name"], "value": "x" }]);
    assert!(
        director.core().command(edit(set_name)).is_err(),
        "no show yet"
    );
    director
        .core()
        .command(Command::LoadShow {
            document: serde_json::json!({ "tracks": [ { "cues": [ { "x": 1 } ] } ] }),
            tag: None,
        })
        .unwrap();
    // Ids were assigned on load, so the cue can be addressed.
    let doc = director.core().show().unwrap().document;
    let (track, cue) = (
        doc["tracks"][0]["id"].as_str().unwrap().to_string(),
        doc["tracks"][0]["cues"][0]["id"]
            .as_str()
            .unwrap()
            .to_string(),
    );
    // A negative duration is not a valid show: the edit is refused whole.
    let bad = serde_json::json!([{ "op": "set",
        "path": ["tracks", { "id": track }, "cues", { "id": cue }, "duration_s"], "value": -5 }]);
    let err = director.core().command(edit(bad)).unwrap_err();
    assert!(err.starts_with("edit_show"), "{err}");
    assert_eq!(director.core().show().unwrap().document, doc, "unchanged");

    // An edit that would give two cues one id is refused whole.
    let dup = serde_json::json!([{ "op": "insert", "path": ["tracks"],
        "value": { "id": "t9", "cues": [ { "id": cue } ] } }]);
    let err = director.core().command(edit(dup)).unwrap_err();
    assert!(err.contains("same id"), "{err}");

    // An older show with a free-running track is converted on load.
    director
        .core()
        .command(Command::LoadShow {
            document: serde_json::json!({ "tracks": [ { "id": "t", "free_run": true,
                "cues": [ { "id": "c" } ] } ] }),
            tag: None,
        })
        .unwrap();
    let doc = director.core().show().unwrap().document;
    assert_eq!(doc["tracks"][0]["free_run"], false);
    assert_eq!(doc["tracks"][0]["cues"][0]["free_run"], true);
    assert_eq!(doc["tracks"][0]["cues"][0]["loop"], true);
    director.shutdown().await;
}
