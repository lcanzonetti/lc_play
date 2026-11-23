//! The whole audio path on a real sound device: a Director, a Runner agent,
//! its audio thread, the driver's callback.
//!
//! It needs a device: ALSA's `null` device (present wherever ALSA is
//! configured, no card needed) or a name given in `LCPLAY_TEST_AUDIO`
//! (`host:id`, as the Producer's Audio window lists it). Without one it is
//! skipped. Run it by hand: `cargo test -p lc-runner --test audio_device --
//! --ignored --nocapture` (with `LCPLAY_TEST_SOAK_S=120` to also watch it
//! stay locked for two minutes). ALSA's `null` device has no clock of its
//! own (it takes samples as fast as they come), so drift and re-alignment
//! figures only mean something on a real or clocked device (`alsa:pulse`
//! with a PulseAudio null sink, for instance).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use lc_director::{Director, DirectorConfig};
use lc_proto::{Command, FrameRate, NANOS_PER_SEC};
use lc_runner::agent::{Agent, AgentConfig, DirectorAddr};
use lc_runner::assets::AssetCache;
use lc_time::{MonotonicClock, SkewedClock};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a sound device; run by hand"]
async fn a_cue_is_heard_on_the_device_in_step_with_the_cluster() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let wanted = std::env::var("LCPLAY_TEST_AUDIO").unwrap_or_else(|_| "alsa:null".into());
    let Some(device) = lc_runner::audio::device::list()
        .into_iter()
        .find(|d| d.id == wanted)
    else {
        eprintln!("no device '{wanted}': skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("lcplay-audio-e2e-{}", lc_proto::NodeId::random()));
    std::fs::create_dir_all(dir.join("cache/Optimized")).unwrap();

    // A 2 s, 1 kHz stereo sidecar, put in the cache as if downloaded.
    let rate = lc_proto::audio::SAMPLE_RATE as usize;
    let samples: Vec<f32> = (0..2 * rate)
        .flat_map(|i| {
            let s = (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / rate as f32).sin() * 0.5;
            [s, s]
        })
        .collect();
    let wav = dir.join("cache/Optimized/tone.mov.wav");
    lc_runner::audio::pcm::write_wav(&wav, 2, rate as u32, &samples).unwrap();
    let size = std::fs::metadata(&wav).unwrap().len();

    let base = Arc::new(MonotonicClock::new());
    let director = Director::start(DirectorConfig {
        name: "test-director".into(),
        control_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        sync_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        lead: Duration::from_millis(100),
        frame_rate: FrameRate::FPS_60,
        state_dir: None,
        osc_addr: None,
        discovery: None,
        local_clock: base.clone(),
    })
    .await
    .unwrap();
    // A Runner on "another computer": its clock 3 s off and 40 ppm fast.
    let mut cfg = AgentConfig::new("a", DirectorAddr::Static(director.control_addr()));
    cfg.local_clock = Arc::new(SkewedClock::new(base.clone(), 3 * NANOS_PER_SEC, 40.0));
    let runner = Agent::spawn(cfg);
    lc_runner::audio::start(
        runner.playhead.clone(),
        "a".into(),
        AssetCache::new(dir.join("cache")),
    );

    let document = serde_json::json!({
        "audio": {
            "buses": [ { "name": "L" }, { "name": "R" } ],
            "runners": [ { "runner": "a", "output": { "host": device.host, "id": device.id, "name": device.name },
                           "output_channels": [0, 1] } ]
        },
        "assets": [ { "id": "tone", "name": "tone.mov", "kind": "video", "url": "http://127.0.0.1:1/tone.mov", "size": 1,
                      "path": "Optimized/tone.mov", "duration_s": 2,
                      "audio": { "url": "http://127.0.0.1:1/tone.wav", "size": size, "path": "Optimized/tone.mov.wav",
                                 "channels": 2, "frames": 2 * rate, "offset_s": 0 } } ],
        "tracks": [ { "cues": [ { "id": "c", "source": { "type": "asset", "asset": "tone" },
                                  "start_s": 0, "duration_s": 60, "loop": true, "w": 1, "h": 1 } ] } ]
    });
    director
        .core()
        .command(Command::LoadShow {
            document,
            tag: None,
        })
        .unwrap();
    director.core().command(Command::Play).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut heard = false;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let h = runner.playhead.hardware();
        if let Some(a) = &h.audio {
            println!(
                "{} {} ch {} Hz, {} frames, latency {:.1} ms, drift {:+.1} ppm, error {:+.0} µs, {} voices, {} realigns, {} xruns, peaks {:?}",
                a.state, a.channels, a.sample_rate, a.buffer_frames, a.latency_ms, a.drift_ppm,
                a.sync_error_us, a.voices, a.realigns, a.underruns, a.peaks
            );
            assert_ne!(a.state, "error", "{:?}", a.error);
            heard |= a.voices == 1 && a.peaks.first().is_some_and(|&p| p > 0.4);
        }
        if heard || tokio::time::Instant::now() > deadline {
            break;
        }
    }
    assert!(heard, "the cue was not heard on the device");

    // `LCPLAY_TEST_SOAK_S`: keep playing that long, and check the output
    // stays locked (no re-alignment once settled).
    let soak: u64 = std::env::var("LCPLAY_TEST_SOAK_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if soak > 0 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let settled = runner.playhead.hardware().audio.map_or(0, |a| a.realigns);
        for _ in 0..soak / 5 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let a = runner.playhead.hardware().audio.unwrap();
            println!(
                "drift {:+.2} ppm, error {:+.0} µs, latency {:.1} ms, {} realigns, {} xruns",
                a.drift_ppm, a.sync_error_us, a.latency_ms, a.realigns, a.underruns
            );
            assert_eq!(a.realigns, settled, "re-aligned while playing");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
