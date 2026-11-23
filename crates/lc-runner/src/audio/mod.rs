//! # Audio: the cues' sound, on this Runner's sound card
//!
//! ```text
//!   show document ──▶ [ control thread ] ──plan, snapshots──▶ [ audio callback ] ──▶ device
//!                        files, clock, transports               mixer → resampler
//! ```
//!
//! * The **control thread** (here) does everything that may block or
//!   allocate: it lists the devices, opens the one the show gives this
//!   Runner, turns the show into a [`Plan`] (which cue channel goes to which
//!   device channel, at what gain, from which memory-mapped file), fetches
//!   the sidecar files through the asset cache, keeps the pages about to be
//!   played in memory, and every few milliseconds publishes a [`Snapshot`]
//!   of the clock and the transports. It also writes the health report.
//! * The **callback** (`mixer.rs`, run by the driver's thread) only
//!   computes: no lock, no allocation, no file access. It takes a new plan
//!   from a channel and the newest snapshot from an `ArcSwap`, and hands old
//!   plans back to be freed here.
//!
//! Nothing in the sound depends on when the callback happens to run: each
//! sample is a function of the cluster time at which the device will play
//! it, exactly as each video frame is a function of the cluster time of the
//! refresh that shows it. Every Runner that plays a bus plays the same
//! sample at the same instant, within the clock sync's accuracy.

pub mod device;
pub mod mixer;
pub mod pcm;
pub mod resample;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use lc_proto::audio::{db_to_gain, AudioDeviceRef, AudioHealth, RunnerAudio, SAMPLE_RATE};
use lc_proto::show::{Asset, AssetKind, CueSource, ShowDocument};
use lc_proto::NANOS_PER_SEC;
use lc_time::ClusterClock;

use crate::assets::AssetCache;
use crate::playhead::{Playhead, ShowDoc};
use mixer::{ClockMap, Engine, Plan, Route, Snapshot, Stats, Voice, VoiceState};
use pcm::PcmFile;

/// How often the control thread runs.
const TICK: Duration = Duration::from_millis(5);
/// How often the health report is refreshed.
const HEALTH_EVERY: Duration = Duration::from_millis(500);
/// How often the pages about to be played are touched.
const PREFETCH_EVERY: Duration = Duration::from_millis(100);
/// How much of a voice is kept in memory ahead of where it plays.
const PREFETCH_AHEAD_S: f64 = 3.0;
/// Wait before opening a device again after it failed.
const RETRY: Duration = Duration::from_secs(3);
/// Snapshots and plans are freed here, not in the callback: kept this long
/// after being replaced, then dropped once nobody else holds them.
const KEEP_RETIRED: Duration = Duration::from_millis(250);

/// Start the audio side of a Runner called `runner`, which follows the
/// cluster's show through `playhead` and fetches media into `assets`.
pub fn start(playhead: Arc<Playhead>, runner: String, assets: Arc<AssetCache>) {
    let spawned = std::thread::Builder::new()
        .name("lc-audio".into())
        .spawn(move || Control::new(playhead, runner, assets).run());
    if let Err(e) = spawned {
        tracing::error!("audio: cannot start: {e}");
    }
}

/// What the control thread tells the callback.
enum Command {
    Plan(Arc<Plan>, Vec<VoiceState>),
    Delay(i64),
}

/// What the callback hands back to be freed.
type Trash = (Arc<Plan>, Vec<VoiceState>);

/// The callback's state.
struct Feed {
    engine: Engine,
    clock: Arc<ClusterClock>,
    snapshot: Arc<ArcSwapOption<Snapshot>>,
    commands: Receiver<Command>,
    trash: SyncSender<Trash>,
}

impl device::Fill for Feed {
    fn fill(&mut self, out: &mut [f32], playback_ns: i64) {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Plan(plan, states)) => {
                    let old = self.engine.mixer.replace(plan, states);
                    if let Err(TrySendError::Full(old) | TrySendError::Disconnected(old)) =
                        self.trash.try_send(old)
                    {
                        // Never free memory here: leaked instead (cannot
                        // happen in practice — the bin holds 16 plans).
                        std::mem::forget(old);
                    }
                }
                Ok(Command::Delay(ns)) => self.engine.delay_ns = ns,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let frames = (out.len() / self.engine.channels()) as u32;
        self.engine
            .stats
            .buffer_frames
            .store(frames, Ordering::Relaxed);
        Stats::set(
            &self.engine.stats.latency_ms,
            (playback_ns - self.clock.local_now_ns()) as f64 / 1e6,
        );
        self.engine.stats.callbacks.fetch_add(1, Ordering::Relaxed);
        let snap = self.snapshot.load();
        self.engine.render(out, playback_ns, snap.as_deref());
    }

    fn local_now_ns(&self) -> i64 {
        self.clock.local_now_ns()
    }
}

/// An open output and the channels to its callback.
struct Running {
    output: device::Output,
    device: AudioDeviceRef,
    stats: Arc<Stats>,
    commands: SyncSender<Command>,
    trash: Receiver<Trash>,
    snapshot: Arc<ArcSwapOption<Snapshot>>,
    delay_ns: i64,
}

struct Control {
    playhead: Arc<Playhead>,
    runner: String,
    assets: Arc<AssetCache>,
    /// Rescan requests acted on.
    rescans: u64,
    /// The show and asset-cache generation the plan was built from.
    show: Option<Arc<ShowDoc>>,
    assets_generation: u64,
    /// This Runner's audio settings in that show.
    setup: Option<RunnerAudio>,
    doc: Option<ShowDocument>,
    running: Option<Running>,
    /// Why the device is not open, and when to try again.
    failure: Option<(String, Instant)>,
    files: HashMap<PathBuf, Arc<PcmFile>>,
    plan: Arc<Plan>,
    next_plan_id: u64,
    retired_plans: Vec<(Instant, Arc<Plan>)>,
    retired_snapshots: VecDeque<(Instant, Arc<Snapshot>)>,
    last_health: Instant,
    last_prefetch: Instant,
}

impl Control {
    fn new(playhead: Arc<Playhead>, runner: String, assets: Arc<AssetCache>) -> Control {
        Control {
            playhead,
            runner,
            assets,
            rescans: u64::MAX,
            show: None,
            assets_generation: u64::MAX,
            setup: None,
            doc: None,
            running: None,
            failure: None,
            files: HashMap::new(),
            plan: Arc::new(Plan {
                id: 0,
                voices: Vec::new(),
            }),
            next_plan_id: 1,
            retired_plans: Vec::new(),
            retired_snapshots: VecDeque::new(),
            last_health: Instant::now(),
            last_prefetch: Instant::now(),
        }
    }

    fn run(mut self) {
        loop {
            let started = Instant::now();
            self.tick();
            std::thread::sleep(TICK.saturating_sub(started.elapsed()));
        }
    }

    fn tick(&mut self) {
        // Devices: at start, and when the operator asks (a device was
        // plugged in). Listing can take a while (drivers load); the sound
        // does not wait for it — the callback runs on its own.
        let rescans = self.playhead.audio_rescans();
        if rescans != self.rescans {
            self.rescans = rescans;
            let devices = device::list();
            tracing::info!("audio: {} devices", devices.len());
            self.playhead.update_hardware(|h| h.audio_devices = devices);
        }

        // The show, or a file that has arrived: plan again.
        let show = self.playhead.show();
        let generation = self.assets.generation();
        let show_changed = match (&show, &self.show) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        if show_changed {
            self.show = show.clone();
            self.doc = show.and_then(|s| {
                match serde_json::from_value::<ShowDocument>(s.document.clone()) {
                    Ok(doc) => Some(doc),
                    Err(e) => {
                        tracing::error!("audio: the show cannot be read: {e}");
                        None
                    }
                }
            });
            self.setup = self
                .doc
                .as_ref()
                .and_then(|d| d.audio.for_runner(&self.runner).cloned());
        }

        self.manage_device();

        if show_changed || generation != self.assets_generation {
            self.assets_generation = generation;
            self.replan();
        }

        self.publish();
        self.collect_garbage();

        if self.last_prefetch.elapsed() >= PREFETCH_EVERY {
            self.last_prefetch = Instant::now();
            self.prefetch();
        }
        if self.last_health.elapsed() >= HEALTH_EVERY {
            self.last_health = Instant::now();
            let health = self.health();
            self.playhead.update_hardware(|h| h.audio = health);
        }
    }

    /// Open, keep, or close the output as the show says.
    fn manage_device(&mut self) {
        let wanted = self.setup.as_ref().and_then(|s| s.output.clone());
        // A stream the driver gave up on is closed, and retried.
        if let Some(r) = &self.running {
            if r.output.events.failed.load(Ordering::Relaxed) {
                let why = r
                    .output
                    .events
                    .error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .unwrap_or_else(|| "the device stopped".into());
                tracing::warn!("audio: {}: {why}", r.output.label);
                self.running = None;
                self.failure = Some((why, Instant::now() + RETRY));
            }
        }
        if self.running.as_ref().map(|r| &r.device) != wanted.as_ref() && self.running.is_some() {
            tracing::info!(
                "audio: closing {}",
                self.running.as_ref().map_or("", |r| &r.output.label)
            );
            self.running = None;
            self.failure = None;
        }
        let Some(wanted) = wanted else {
            self.failure = None;
            return;
        };
        if self.running.is_some() {
            return;
        }
        if self
            .failure
            .as_ref()
            .is_some_and(|(_, at)| Instant::now() < *at)
        {
            return;
        }
        match self.open(&wanted) {
            Ok(r) => {
                tracing::info!(
                    "audio: playing on {} ({} channels, {} Hz)",
                    r.output.label,
                    r.output.channels,
                    r.output.sample_rate
                );
                self.running = Some(r);
                self.failure = None;
                self.replan();
            }
            Err(e) => {
                let why = format!("{e:#}");
                if self.failure.as_ref().map(|(w, _)| w) != Some(&why) {
                    tracing::error!("audio: {why}");
                }
                self.failure = Some((why, Instant::now() + RETRY));
            }
        }
    }

    fn open(&mut self, wanted: &AudioDeviceRef) -> anyhow::Result<Running> {
        let (commands, commands_rx) = sync_channel(16);
        let (trash_tx, trash) = sync_channel(16);
        let snapshot = Arc::new(ArcSwapOption::empty());
        let mut stats = None;
        let clock = self.playhead.clock.clone();
        let feed_snapshot = snapshot.clone();
        let output = device::open(wanted, |channels, rate| {
            let s = Arc::new(Stats::new(channels));
            stats = Some(s.clone());
            Feed {
                engine: Engine::new(channels, rate, s),
                clock,
                snapshot: feed_snapshot,
                commands: commands_rx,
                trash: trash_tx,
            }
        })?;
        Ok(Running {
            output,
            device: wanted.clone(),
            stats: stats.expect("set when the device opened"),
            commands,
            trash,
            snapshot,
            delay_ns: 0,
        })
    }

    /// Build the plan for the current show and device, and send it.
    fn replan(&mut self) {
        let (Some(doc), Some(setup), Some(running)) = (&self.doc, &self.setup, &mut self.running)
        else {
            return;
        };
        let id = self.next_plan_id;
        self.next_plan_id += 1;
        let assets = &self.assets;
        let files = &mut self.files;
        let plan = Arc::new(build_plan(
            doc,
            setup,
            running.output.channels,
            id,
            |asset| {
                let sidecar = sidecar_asset(asset)?;
                let path = assets.local_path(&sidecar)?;
                if let Some(f) = files.get(&path) {
                    return Some(f.clone());
                }
                match PcmFile::open(&path) {
                    Ok(f) => {
                        let f = Arc::new(f);
                        files.insert(path, f.clone());
                        Some(f)
                    }
                    Err(e) => {
                        tracing::error!("audio: {e:#}");
                        None
                    }
                }
            },
        ));
        let states = vec![VoiceState::default(); plan.voices.len()];
        let delay_ns = (setup.delay_ms * 1e6).round() as i64;
        if delay_ns != running.delay_ns
            && running.commands.try_send(Command::Delay(delay_ns)).is_ok()
        {
            running.delay_ns = delay_ns;
        }
        if running
            .commands
            .try_send(Command::Plan(plan.clone(), states))
            .is_err()
        {
            tracing::warn!("audio: the output is not taking plans");
            return;
        }
        tracing::info!("audio: {} cues with sound", plan.voices.len());
        let old = std::mem::replace(&mut self.plan, plan);
        self.retired_plans.push((Instant::now(), old));
    }

    /// The clock and transports, for the callback.
    fn publish(&mut self) {
        let Some(running) = &self.running else {
            return;
        };
        let clock = &self.playhead.clock;
        let local0 = clock.local_now_ns();
        let reading = clock.read(local0);
        // The slope over the next second: the estimator's drift.
        let ahead = clock.at_local(local0 + NANOS_PER_SEC);
        let slope = (ahead - reading.cluster_ns) as f64 / NANOS_PER_SEC as f64;
        let (main, tasks, timeline_clock) = self.playhead.timelines();
        // As for the picture: sound only once the clock is measured in the
        // timebase the transports are written in.
        let synced =
            reading.clock_id == timeline_clock && (reading.synced || reading.clock_id.is_none());
        let mut snap = Snapshot {
            clock: ClockMap {
                local0,
                cluster0: reading.cluster_ns,
                slope: if slope.is_finite() && (slope - 1.0).abs() < 0.01 {
                    slope
                } else {
                    1.0
                },
                synced,
            },
            main,
            tasks,
            plan_id: self.plan.id,
            active: Vec::new(),
        };
        let latency = (Stats::get(&running.stats.latency_ms) * 1e6) as i64;
        let content = reading.cluster_ns + latency - running.delay_ns;
        snap.active = self
            .plan
            .voices
            .iter()
            .map(|v| could_sound(v, &snap, content))
            .collect();
        let snap = Arc::new(snap);
        running.snapshot.store(Some(snap.clone()));
        self.retired_snapshots.push_back((Instant::now(), snap));
    }

    /// Free what the callback no longer uses.
    fn collect_garbage(&mut self) {
        if let Some(r) = &self.running {
            while let Ok(old) = r.trash.try_recv() {
                drop(old);
            }
        }
        let now = Instant::now();
        // The newest snapshot is the one in use: keep it.
        while self.retired_snapshots.len() > 1 {
            let (at, s) = &self.retired_snapshots[0];
            if now.duration_since(*at) < KEEP_RETIRED || Arc::strong_count(s) > 1 {
                break;
            }
            self.retired_snapshots.pop_front();
        }
        self.retired_plans
            .retain(|(at, p)| now.duration_since(*at) < KEEP_RETIRED || Arc::strong_count(p) > 1);
        // Files no plan uses any more are unmapped.
        let plan = &self.plan;
        let retired = &self.retired_plans;
        self.files.retain(|_, f| {
            plan.voices.iter().any(|v| Arc::ptr_eq(&v.file, f))
                || retired
                    .iter()
                    .any(|(_, p)| p.voices.iter().any(|v| Arc::ptr_eq(&v.file, f)))
        });
    }

    /// Touch the pages of the voices that may be heard soon, so the
    /// callback never waits for the disk.
    fn prefetch(&self) {
        let (Some(running), Some(snap)) = (&self.running, self.retired_snapshots.back()) else {
            return;
        };
        let snap = &snap.1;
        if snap.plan_id != self.plan.id {
            return;
        }
        let rate = SAMPLE_RATE as f64;
        let latency = (Stats::get(&running.stats.latency_ms) * 1e6) as i64;
        let content = snap.clock.cluster0 + latency - running.delay_ns;
        let ahead = (PREFETCH_AHEAD_S * rate) as u64;
        for (v, active) in self.plan.voices.iter().zip(&snap.active) {
            if !active {
                continue;
            }
            let file_frame = |media: i64| (media - v.offset_samples).max(0) as u64;
            // Its start, for when it goes on air (or loops back) …
            v.file
                .prefetch(file_frame(v.in_samples), (rate as u64).min(ahead));
            // … and where it is now.
            if let Some(show) = snap
                .timeline(v.timeline.as_deref())
                .map(|tl| tl.state_at(content))
                .filter(|s| s.active)
                .map(|s| s.show_time_at(content))
            {
                if (v.start_ns..v.end_ns).contains(&show) && !v.free_run {
                    let elapsed = ((show - v.start_ns) as f64 / NANOS_PER_SEC as f64 * rate) as i64;
                    let media = match v.loop_samples {
                        Some(len) if len > v.in_samples => {
                            v.in_samples + elapsed.rem_euclid(len - v.in_samples)
                        }
                        _ => v.in_samples + elapsed,
                    };
                    v.file.prefetch(file_frame(media - rate as i64 / 10), ahead);
                }
            }
        }
    }

    fn health(&self) -> Option<AudioHealth> {
        let setup = self.setup.as_ref()?;
        let Some(r) = &self.running else {
            let device = setup.output.as_ref()?;
            return Some(AudioHealth {
                state: "error".into(),
                error: Some(
                    self.failure
                        .as_ref()
                        .map_or_else(|| "opening the device".into(), |(w, _)| w.clone()),
                ),
                device: format!("{}: {}", device.host, device.name),
                ..Default::default()
            });
        };
        let s = &r.stats;
        Some(AudioHealth {
            state: "running".into(),
            error: None,
            device: r.output.label.clone(),
            channels: r.output.channels as u32,
            sample_rate: r.output.sample_rate,
            buffer_frames: s.buffer_frames.load(Ordering::Relaxed),
            latency_ms: Stats::get(&s.latency_ms),
            drift_ppm: Stats::get(&s.drift_ppm),
            sync_error_us: Stats::get(&s.sync_error_us),
            underruns: r.output.events.xruns.load(Ordering::Relaxed),
            realigns: s.realigns.load(Ordering::Relaxed),
            voices: s.voices.load(Ordering::Relaxed),
            peaks: s.take_peaks(),
        })
    }
}

/// The asset-cache entry of `asset`'s sound file, if it has one.
fn sidecar_asset(asset: &Asset) -> Option<Asset> {
    let audio = asset.audio.as_ref()?;
    if audio.url.is_empty() {
        return None;
    }
    let name = audio
        .path
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .map_or_else(|| format!("{}.wav", asset.name), str::to_string);
    Some(Asset {
        id: format!("{}#audio", asset.id),
        name,
        kind: AssetKind::Audio,
        url: audio.url.clone(),
        size: audio.size,
        path: audio.path.clone(),
        ..Default::default()
    })
}

/// Could voice `v` be heard around content time `t` (cluster ns)? Looks a
/// little back and a couple of seconds ahead, on the transport in force and
/// on the newest one (a GO already sent, due in 100 ms).
fn could_sound(v: &Voice, snap: &Snapshot, t: i64) -> bool {
    let Some(tl) = snap.timeline(v.timeline.as_deref()) else {
        return false;
    };
    let near = |state: lc_proto::TransportState, at: i64| {
        state.active && {
            let show = state.show_time_at(at);
            show >= v.start_ns - NANOS_PER_SEC && show < v.end_ns
        }
    };
    let s = NANOS_PER_SEC;
    [-s / 2, 0, s / 4, s / 2, s, 2 * s]
        .iter()
        .any(|dt| near(tl.state_at(t + dt), t + dt))
        || {
            let latest = tl.latest();
            let at = latest.anchor_cluster_ns.max(t);
            near(latest, at) || near(latest, at + s)
        }
}

/// Seconds → sample frames at the system rate.
fn samples(s: f64) -> i64 {
    (s * SAMPLE_RATE as f64).round() as i64
}

/// Which cue plays what, on this Runner's `channels` device channels.
/// `file` gives the sound file of an asset (`None`: none, or not here yet).
pub fn build_plan(
    doc: &ShowDocument,
    setup: &RunnerAudio,
    channels: usize,
    id: u64,
    mut file: impl FnMut(&Asset) -> Option<Arc<PcmFile>>,
) -> Plan {
    let output_gain = db_to_gain(setup.gain_db);
    let main = (None, &doc.tracks);
    let tasks = doc.tasks.iter().map(|t| (Some(t.id.clone()), &t.tracks));
    let mut voices = Vec::new();
    for (timeline, tracks) in std::iter::once(main).chain(tasks) {
        for (ti, track) in tracks.iter().enumerate() {
            for (ci, cue) in track.cues.iter().enumerate() {
                let CueSource::Asset { asset } = &cue.source else {
                    continue;
                };
                if cue.audio.mute {
                    continue;
                }
                let Some(asset) = doc.asset(asset) else {
                    continue;
                };
                let Some(audio) = &asset.audio else {
                    continue;
                };
                let Some(f) = file(asset) else {
                    continue;
                };
                let gain = db_to_gain(cue.audio.gain_db) * output_gain;
                let mut routes = Vec::new();
                for src in 0..f.channels() as usize {
                    let Some(bus) = cue.audio.bus_of_channel(src) else {
                        continue;
                    };
                    for dst in 0..channels.min(setup.output_channels.len()) {
                        if setup.bus_of_channel(dst) == Some(bus) {
                            routes.push(Route {
                                src: src as u16,
                                dst: dst as u16,
                                gain,
                            });
                        }
                    }
                }
                if routes.is_empty() || gain == 0.0 {
                    continue;
                }
                let offset = samples(audio.offset_s);
                let file_end = offset + f.frames() as i64;
                let looping = cue.looping || track.free_run;
                let end_s = cue.start_s + cue.duration_s;
                voices.push(Voice {
                    key: if cue.id.is_empty() {
                        format!("{}t{ti}c{ci}", timeline.as_deref().unwrap_or(""))
                    } else {
                        cue.id.clone()
                    },
                    timeline: timeline.clone(),
                    start_s: cue.start_s,
                    end_s,
                    fade_in_s: cue.fade_in_s,
                    fade_out_s: cue.fade_out_s,
                    start_ns: (cue.start_s * NANOS_PER_SEC as f64).round() as i64,
                    end_ns: (end_s * NANOS_PER_SEC as f64).round() as i64,
                    in_samples: samples(cue.in_s.max(0.0)),
                    // A video's sound loops with its picture: at the video's
                    // length, not the (slightly different) audio's.
                    loop_samples: looping.then(|| {
                        if asset.kind == AssetKind::Video && asset.duration_s > 0.0 {
                            samples(asset.duration_s)
                        } else {
                            file_end
                        }
                    }),
                    end_samples: file_end,
                    offset_samples: offset,
                    free_run: cue.free_run || track.free_run,
                    file: f,
                    routes,
                });
            }
        }
    }
    Plan { id, voices }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(name: &str, channels: u16, frames: usize) -> Arc<PcmFile> {
        let path =
            std::env::temp_dir().join(format!("lcplay-plan-{name}-{}.wav", std::process::id()));
        pcm::write_wav(
            &path,
            channels,
            SAMPLE_RATE,
            &vec![0.25; frames * channels as usize],
        )
        .unwrap();
        let f = Arc::new(PcmFile::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        f
    }

    fn doc() -> ShowDocument {
        serde_json::from_value(serde_json::json!({
            "audio": {
                "buses": [ { "name": "L" }, { "name": "R" }, { "name": "Sub" } ],
                "runners": [ { "runner": "a", "output": { "host": "ALSA", "id": "x", "name": "x" },
                               "output_channels": [0, 1, null, 0], "gain_db": -6.0 } ]
            },
            "assets": [
                { "id": "v", "name": "v.mov", "kind": "video", "url": "http://x/v", "size": 1, "duration_s": 10,
                  "audio": { "url": "http://x/v.wav", "size": 1, "path": "Audio/v.mov.wav", "channels": 2, "frames": 480000, "offset_s": 0.01 } },
                { "id": "silent", "name": "s.mov", "kind": "video", "url": "http://x/s", "size": 1 }
            ],
            "tracks": [ { "cues": [
                { "id": "c1", "source": { "type": "asset", "asset": "v" }, "start_s": 2, "duration_s": 5, "in_s": 1, "loop": true, "w": 1, "h": 1 },
                { "id": "c2", "source": { "type": "asset", "asset": "v" }, "start_s": 0, "duration_s": 5, "w": 1, "h": 1,
                  "audio": { "channel_buses": [null, 2] } },
                { "id": "muted", "source": { "type": "asset", "asset": "v" }, "duration_s": 5, "w": 1, "h": 1, "audio": { "mute": true } },
                { "id": "c3", "source": { "type": "asset", "asset": "silent" }, "duration_s": 5, "w": 1, "h": 1 }
            ] } ]
        }))
        .unwrap()
    }

    #[test]
    fn cue_channels_reach_the_device_channels_of_their_buses() {
        let doc = doc();
        let setup = doc.audio.for_runner("a").unwrap();
        let f = wav("routes", 2, 1000);
        let plan = build_plan(&doc, setup, 4, 7, |_| Some(f.clone()));
        assert_eq!(plan.id, 7);
        // c2 feeds only bus 2 (Sub), which no channel of this Runner plays;
        // the muted cue and the silent file give nothing.
        let keys: Vec<&str> = plan.voices.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["c1"]);
        let v = &plan.voices[0];
        // Channel 0 (L) → device 0 and 3; channel 1 (R) → device 1.
        let mut routes: Vec<(u16, u16)> = v.routes.iter().map(|r| (r.src, r.dst)).collect();
        routes.sort();
        assert_eq!(routes, [(0, 0), (0, 3), (1, 1)]);
        assert!((v.routes[0].gain - db_to_gain(-6.0)).abs() < 1e-6);
        assert_eq!(v.in_samples, 48_000);
        assert_eq!(v.offset_samples, 480);
        // Loops with the picture.
        assert_eq!(v.loop_samples, Some(480_000));
        assert_eq!(v.end_samples, 480 + 1000);
        // A device with fewer channels than mapped: the rest is dropped.
        let narrow = build_plan(&doc, setup, 1, 8, |_| Some(f.clone()));
        assert_eq!(narrow.voices[0].routes.len(), 1);
    }

    #[test]
    fn the_sidecar_is_cached_under_its_own_identity() {
        let doc = doc();
        let s = sidecar_asset(doc.asset("v").unwrap()).unwrap();
        assert_eq!(s.id, "v#audio");
        assert_eq!(s.name, "v.mov.wav");
        assert_eq!(s.path, "Audio/v.mov.wav");
        assert!(sidecar_asset(doc.asset("silent").unwrap()).is_none());
    }
}
