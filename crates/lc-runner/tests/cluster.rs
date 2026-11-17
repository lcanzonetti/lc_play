//! End-to-end tests of the communication and synchronisation layer: a real
//! Director and real Runner agents, talking over real sockets on loopback.
//!
//! What cannot be real on one machine is "different computers with different
//! clocks". That is simulated where it matters: every Runner (and, in the
//! restart test, the second Director) reads time through a [`SkewedClock`]
//! with its own offset and drift, all derived from one shared base clock.
//! Because the first Director uses the base clock directly, **cluster time is
//! the base clock** — which gives the tests an independent source of truth to
//! measure every node against, even while no Director is running.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lc_director::{Director, DirectorConfig};
use lc_proto::{Command, FrameRate, NANOS_PER_SEC};
use lc_runner::agent::{Agent, AgentConfig, DirectorAddr};
use lc_time::{LocalClock, MonotonicClock, SkewedClock};

/// Clock agreement demanded of a locked Runner. Loopback achieves tens of
/// microseconds; the bound is loose because CI machines stall unpredictably.
/// It is still an eighth of a 60 fps frame.
const SYNC_TOLERANCE_NS: i64 = 2_000_000;
const LEAD: Duration = Duration::from_millis(100);
const FPS: FrameRate = FrameRate::FPS_60;

/// A scratch directory removed when the test ends.
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("lcplay-test-{tag}-{}", lc_proto::NodeId::random()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn path(&self, sub: &str) -> PathBuf {
        self.0.join(sub)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start_director(
    clock: Arc<dyn LocalClock>,
    control_port: u16,
    state_dir: Option<PathBuf>,
) -> Director {
    Director::start(DirectorConfig {
        name: "test-director".into(),
        control_addr: SocketAddr::from(([127, 0, 0, 1], control_port)),
        sync_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        lead: LEAD,
        frame_rate: FPS,
        state_dir,
        osc_addr: None,
        discovery: None, // tests address the Director explicitly
        local_clock: clock,
    })
    .await
    .expect("director starts")
}

/// A Runner on "another machine": its clock is `offset_s` seconds off and
/// `ppm` fast relative to the base clock.
fn spawn_runner(
    name: &str,
    base: &Arc<MonotonicClock>,
    director: SocketAddr,
    offset_s: i64,
    ppm: f64,
    cache_dir: Option<PathBuf>,
) -> Agent {
    let mut cfg = AgentConfig::new(name, DirectorAddr::Static(director));
    cfg.local_clock = Arc::new(SkewedClock::new(
        base.clone(),
        offset_s * NANOS_PER_SEC,
        ppm,
    ));
    cfg.cache_dir = cache_dir;
    Agent::spawn(cfg)
}

/// Poll `condition` until it holds; panic with `what` after `timeout`.
async fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_locked(runners: &[&Agent]) {
    wait_for("all runners locked", Duration::from_secs(15), || {
        runners.iter().all(|r| {
            let q = r.playhead.clock.quality();
            r.playhead.is_connected() && q.locked && q.samples >= 16
        })
    })
    .await;
}

/// Assert a Runner's idea of cluster time against the truth.
fn assert_synced(runner: &Agent, truth: &dyn LocalClock, context: &str) -> i64 {
    // Read the truth on both sides of the Runner so scheduling between the
    // two reads cannot be mistaken for clock error.
    let before = truth.now_ns();
    let runner_now = runner.playhead.clock.now_ns();
    let after = truth.now_ns();
    let error = if runner_now < before {
        runner_now - before
    } else if runner_now > after {
        runner_now - after
    } else {
        0
    };
    assert!(
        error.abs() < SYNC_TOLERANCE_NS,
        "{context}: runner clock is {error} ns from cluster time ({:?})",
        runner.playhead.clock.quality()
    );
    error
}

/// Assert that a Runner presents the frame the transport prescribes for the
/// true cluster time (±1 frame: the truth is read at a slightly different
/// instant than the Runner's own clock).
fn assert_on_frame(runner: &Agent, truth: &dyn LocalClock, context: &str) {
    let before = truth.now_ns();
    let pos = runner.playhead.position();
    let after = truth.now_ns();
    let expect = |cluster_ns| FPS.frame_at(pos.transport.show_time_at(cluster_ns));
    assert!(
        (expect(before) - 1..=expect(after) + 1).contains(&pos.frame),
        "{context}: runner shows frame {}, cluster time prescribes {}..={}",
        pos.frame,
        expect(before),
        expect(after)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runners_with_different_clocks_agree_on_the_frame() {
    let base = Arc::new(MonotonicClock::new());
    let director = start_director(base.clone(), 0, None).await;
    let addr = director.control_addr();

    // An hour ahead and fast; five minutes behind and slow; nearly right.
    let a = spawn_runner("a", &base, addr, 3600, 150.0, None);
    let b = spawn_runner("b", &base, addr, -300, -80.0, None);
    let c = spawn_runner("c", &base, addr, 0, 30.0, None);
    let runners = [&a, &b, &c];
    wait_locked(&runners).await;

    for r in runners {
        let err = assert_synced(r, base.as_ref(), "after lock");
        println!(
            "runner clock error after lock: {err} ns, {:?}",
            r.playhead.clock.quality()
        );
    }

    // Play. The command is anchored one lead time ahead…
    let before = base.now_ns();
    director.core().command(Command::Play).unwrap();
    let play = director.core().transport();
    assert!(play.playing);
    assert!(play.anchor_cluster_ns >= before + LEAD.as_nanos() as i64);

    // …and every Runner must hold the *identical* state before it applies:
    // that is what makes them start on the same frame.
    wait_for("runners received the play state", LEAD / 2, || {
        runners.iter().all(|r| r.playhead.transport() == play)
    })
    .await;
    assert!(
        base.now_ns() < play.anchor_cluster_ns,
        "state arrived before its anchor"
    );
    for r in runners {
        assert!(
            !r.playhead.position().transport.playing,
            "nobody starts early"
        );
    }

    tokio::time::sleep(Duration::from_millis(400)).await;
    for r in runners {
        assert!(r.playhead.position().transport.playing);
        assert_on_frame(r, base.as_ref(), "while playing");
    }

    // Seek while playing, then pause: all Runners freeze on one frame.
    director
        .core()
        .command(Command::Seek {
            show_ns: 90 * NANOS_PER_SEC,
        })
        .unwrap();
    director.core().command(Command::Pause).unwrap();
    tokio::time::sleep(LEAD * 3).await;
    let frames: Vec<i64> = runners
        .iter()
        .map(|r| r.playhead.position().frame)
        .collect();
    assert_eq!(frames[0], FPS.frame_at(90 * NANOS_PER_SEC));
    assert!(
        frames.iter().all(|f| *f == frames[0]),
        "paused on different frames: {frames:?}"
    );

    // The Director sees every Runner, with health.
    wait_for("health from all runners", Duration::from_secs(5), || {
        let status = director.core().status();
        status.nodes.len() == 3 && status.nodes.iter().all(|n| n.health.is_some())
    })
    .await;

    // …and judges each of them to be on the prescribed frame.
    for node in director.core().status().nodes {
        assert_eq!(node.frame_error, Some(0), "{} per the Director", node.name);
    }

    // Losing one Runner must not disturb the rest of the cluster.
    c.shutdown().await;
    wait_for(
        "director notices the lost runner",
        Duration::from_secs(10),
        || director.core().status().nodes.len() == 2,
    )
    .await;
    director.core().command(Command::Play).unwrap();
    tokio::time::sleep(LEAD * 3).await;
    for r in [&a, &b] {
        assert_synced(r, base.as_ref(), "after a peer left");
        assert_on_frame(r, base.as_ref(), "after a peer left");
        assert_eq!(
            r.playhead.clock.steps(),
            0,
            "clock never stepped after initial lock"
        );
    }

    a.shutdown().await;
    b.shutdown().await;
    director.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn playback_survives_director_loss_and_the_director_adopts_on_return() {
    let tmp = TempDir::new("adopt");
    let base = Arc::new(MonotonicClock::new());
    let director = start_director(base.clone(), 0, Some(tmp.path("director"))).await;
    let addr = director.control_addr();
    let clock_id = director.core().clock.clock_id();

    let a = spawn_runner("a", &base, addr, 7200, 120.0, None);
    let b = spawn_runner("b", &base, addr, -60, -90.0, None);
    let runners = [&a, &b];
    wait_locked(&runners).await;
    director.core().command(Command::Play).unwrap();
    let play = director.core().transport();
    // Give the estimators a few seconds of baseline so the drift (which
    // holdover depends on) is measured, not guessed.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // ---- the Director dies --------------------------------------------------
    director.shutdown().await;
    wait_for("runners notice", Duration::from_secs(10), || {
        runners.iter().all(|r| !r.playhead.is_connected())
    })
    .await;

    // The show goes on: still the right frame, measured against cluster time
    // that now only exists as the base clock.
    let frame_before = a.playhead.position().frame;
    tokio::time::sleep(Duration::from_secs(2)).await;
    for r in runners {
        assert!(r.playhead.position().transport.playing);
        assert_synced(r, base.as_ref(), "in holdover");
        assert_on_frame(r, base.as_ref(), "in holdover");
    }
    let advanced = a.playhead.position().frame - frame_before;
    assert!(
        (115..=130).contains(&advanced),
        "2 s of holdover advanced {advanced} frames"
    );

    // ---- a new Director process starts --------------------------------------
    // Its monotonic clock has nothing in common with the old one's.
    let new_clock = Arc::new(SkewedClock::new(base.clone(), 5000 * NANOS_PER_SEC, 40.0));
    let director = start_director(new_clock, addr.port(), Some(tmp.path("director"))).await;
    assert_ne!(
        director.core().clock.clock_id(),
        clock_id,
        "starts with a timebase of its own"
    );

    wait_for("runners reconnect", Duration::from_secs(15), || {
        runners.iter().all(|r| r.playhead.is_connected())
    })
    .await;

    // It took over the running timebase and transport instead of imposing new ones.
    assert_eq!(
        director.core().clock.clock_id(),
        clock_id,
        "adopted the timebase"
    );
    assert_eq!(director.core().transport(), play, "adopted the transport");
    let director_error = director.core().clock.now_ns() - base.now_ns();
    assert!(
        director_error.abs() < SYNC_TOLERANCE_NS,
        "adopted clock is {director_error} ns from the original cluster time"
    );

    // Runners re-lock onto it without a jump.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for r in runners {
        assert_eq!(
            r.playhead.clock.steps(),
            0,
            "no clock step across the Director restart"
        );
        assert_on_frame(r, base.as_ref(), "after adoption");
        assert!(r.playhead.clock.quality().locked);
    }

    // And it is in charge again.
    director.core().command(Command::Pause).unwrap();
    tokio::time::sleep(LEAD * 3).await;
    let paused_at = director.core().status().show_ns;
    for r in runners {
        let pos = r.playhead.position();
        assert!(!pos.transport.playing);
        assert_eq!(pos.show_ns, paused_at, "paused on the same instant");
    }

    a.shutdown().await;
    b.shutdown().await;
    director.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_runner_resumes_from_its_cache_without_a_director() {
    let tmp = TempDir::new("cache");
    let base = Arc::new(MonotonicClock::new());
    let director = start_director(base.clone(), 0, None).await;
    let addr = director.control_addr();

    let runner = spawn_runner("solo", &base, addr, 10, 0.0, Some(tmp.path("runner")));
    let node_id = runner.node_id.clone();
    wait_locked(&[&runner]).await;

    let document =
        serde_json::json!({ "canvas": { "width": 3840, "height": 1080 }, "outputs": [] });
    director
        .core()
        .command(Command::LoadShow {
            document: document.clone(),
            tag: None,
        })
        .unwrap();
    director
        .core()
        .command(Command::Seek {
            show_ns: 30 * NANOS_PER_SEC,
        })
        .unwrap();
    director.core().command(Command::Play).unwrap();
    let play = director.core().transport();
    wait_for(
        "show and transport reach the runner",
        Duration::from_secs(5),
        || runner.playhead.show().is_some() && runner.playhead.transport() == play,
    )
    .await;

    // Power cut: everything goes down.
    runner.shutdown().await;
    director.shutdown().await;

    // The Runner boots alone (the Director address now leads nowhere).
    let runner = spawn_runner("solo", &base, addr, 999, 0.0, Some(tmp.path("runner")));
    assert_eq!(runner.node_id, node_id, "keeps its identity");
    let show = runner.playhead.show().expect("show restored from cache");
    assert_eq!((show.revision, &show.document), (1, &document));

    let first = runner.playhead.position();
    assert!(first.transport.playing, "resumes playing");
    assert!(
        first.show_ns >= 30 * NANOS_PER_SEC,
        "from the last known position"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let later = runner.playhead.position();
    assert!(later.frame > first.frame, "and advances on its own clock");
    assert!(!runner.playhead.is_connected());

    runner.shutdown().await;
}
