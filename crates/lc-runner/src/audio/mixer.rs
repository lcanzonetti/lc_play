//! The sound itself: which sample of which cue is due at which cluster
//! instant, mixed into the device's channels, and resampled onto the
//! device's clock.
//!
//! Everything here runs in the audio callback, so: no locks, no file
//! reads, no allocation (the control thread allocates; see `mod.rs`).
//!
//! ## Two clocks
//!
//! * The **cluster grid**: grid frame *g* is the sound due at cluster time
//!   *g* / 48 kHz ([`grid_ns`]). The [`Mixer`] computes grid frames — each
//!   a pure function of the show, the transports and the cluster time, the
//!   same on every Runner, exactly as a video frame is.
//! * The **device clock**: the card plays its frames at its own rate. For
//!   each callback, the driver says when the first frame will be heard
//!   (`playback` instant, on this computer's clock); the cluster clock says
//!   which cluster instant that is; minus the Runner's delay, that is the
//!   grid position the frame must carry.
//!
//! The [`Engine`] reads the grid at the positions the device's frames fall
//! on (band-limited interpolation, `resample.rs`), and steers its step so
//! that where it is matches where it should be: a PI loop on the
//! difference, which settles on the card's real rate against the cluster's
//! (tens of ppm; up to ±1000 ppm are corrected, which also covers the
//! cluster clock's own slewing, ≤ 500 ppm). Inaudible: 1000 ppm is a
//! sixtieth of a semitone. An error of more than [`REALIGN_S`] — the first
//! callback, a step of the cluster clock, a device that stalled — is not
//! slewed but jumped, with short fades.
//!
//! ## Clicks
//!
//! A cue's sound can jump: a transport jump, a pause, a loop of the
//! timeline, the start of a cue in the middle of its file. Each jump
//! crossfades over [`DECLICK`] frames (5 ms): the old stream carries on for
//! 5 ms fading out while the new one fades in. A cue that starts at its in
//! point (the playhead crossing its start, or a free-running cue going on
//! air) starts at once, so an attack at the start of a file is kept; a
//! file's own loop (`loop` on the cue) is seamless.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use lc_proto::audio::SAMPLE_RATE;
use lc_proto::{TransportState, NANOS_PER_SEC};
use lc_time::TransportTimeline;

use super::pcm::PcmFile;
use super::resample::{Sinc, HALF, TAPS};

/// Crossfade length on a jump, in frames (5 ms).
pub const DECLICK: u32 = 240;
/// Errors larger than this are corrected by a jump, not by resampling.
pub const REALIGN_S: f64 = 0.020;
/// Most the resampler's step may differ from nominal.
const MAX_CORRECTION: f64 = 1e-3;
/// Servo gains, per second and per second²: critically damped, settling in
/// about 30 s (ω = √KI ≈ 0.14 rad/s, ζ = KP / 2ω ≈ 1).
const KP: f64 = 0.3;
const KI: f64 = 0.02;
/// Most error, in seconds, the servo's integral takes in per second (see
/// `Servo::update`).
const INTEGRATE_CAP_S: f64 = 0.000_5;
/// Time constant of the error filter (driver timestamps jitter).
const FILTER_S: f64 = 0.2;
/// Most output frames computed in one go (the callback is split).
pub const CHUNK: usize = 1024;
/// Grid frames the ring holds: one chunk at up to 6× the step, plus the
/// filter's look-ahead.
const RING: usize = 8192;

/// Cluster time of grid frame `g`, in ns (rounded down).
pub fn grid_ns(g: i64) -> i64 {
    // 1e9 / 48000 = 62500 / 3 exactly.
    (g as i128 * 62_500).div_euclid(3) as i64
}

/// The grid position of cluster instant `ns`: whole frames and fraction.
pub fn grid_pos(ns: i64) -> (i64, f64) {
    let scaled = ns as i128 * SAMPLE_RATE as i128;
    let whole = scaled.div_euclid(NANOS_PER_SEC as i128);
    let rem = scaled.rem_euclid(NANOS_PER_SEC as i128);
    (whole as i64, rem as f64 / NANOS_PER_SEC as f64)
}

/// Local time → cluster time, as a straight line valid for a short while.
/// The cluster clock itself takes a lock; the control thread samples it
/// every few milliseconds and hands this over instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockMap {
    pub local0: i64,
    pub cluster0: i64,
    /// Cluster ns per local ns (1 ± the estimated drift).
    pub slope: f64,
    /// False while the timebase is not measured yet: the sound is held
    /// (silent) as the picture is.
    pub synced: bool,
}

impl ClockMap {
    pub fn cluster(&self, local_ns: i64) -> i64 {
        self.cluster0 + ((local_ns - self.local0) as f64 * self.slope).round() as i64
    }
}

/// What the callback needs to know about time, refreshed by the control
/// thread every few milliseconds.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub clock: ClockMap,
    pub main: TransportTimeline,
    /// Auxiliary timelines that have ever been started, by id.
    pub tasks: Vec<(String, TransportTimeline)>,
    /// The plan `active` refers to.
    pub plan_id: u64,
    /// Per voice of that plan: could it be heard soon? Voices that cannot
    /// are skipped (a show may have hundreds of cues with sound).
    pub active: Vec<bool>,
}

impl Snapshot {
    pub fn timeline(&self, id: Option<&str>) -> Option<&TransportTimeline> {
        match id {
            None => Some(&self.main),
            Some(id) => self.tasks.iter().find(|(k, _)| k == id).map(|(_, t)| t),
        }
    }
}

/// One cue channel → one device channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Route {
    pub src: u16,
    pub dst: u16,
    /// Cue gain × the Runner's output gain.
    pub gain: f32,
}

/// A cue with sound that this Runner plays.
pub struct Voice {
    /// Identifies the cue across plans, so a cue keeps playing smoothly
    /// when the show is edited.
    pub key: String,
    /// `None`: the main timeline; else the task's id.
    pub timeline: Option<String>,
    pub start_s: f64,
    pub end_s: f64,
    pub fade_in_s: f64,
    pub fade_out_s: f64,
    pub start_ns: i64,
    pub end_ns: i64,
    /// The in point, in media samples.
    pub in_samples: i64,
    /// Media length to loop at (the video's length for a video), or `None`.
    pub loop_samples: Option<i64>,
    /// Past this media sample, silence.
    pub end_samples: i64,
    /// The media sample the file's first frame is.
    pub offset_samples: i64,
    pub free_run: bool,
    pub file: Arc<PcmFile>,
    pub routes: Vec<Route>,
}

impl Voice {
    /// Gain of the cue's fades at show time `t` (seconds).
    fn fade_at(&self, t: f64) -> f32 {
        let mut k = 1.0f64;
        if self.fade_in_s > 0.0 {
            k *= ((t - self.start_s) / self.fade_in_s).clamp(0.0, 1.0);
        }
        if self.fade_out_s > 0.0 {
            k *= ((self.end_s - t) / self.fade_out_s).clamp(0.0, 1.0);
        }
        k as f32
    }

    /// The media sample after `prev`, following the file's loop.
    fn next(&self, prev: i64) -> i64 {
        match self.loop_samples {
            Some(len) if len > self.in_samples && prev + 1 >= len => self.in_samples,
            _ => prev + 1,
        }
    }

    /// Which media sample is due at cluster time `t`, and the fade gain —
    /// or `None` if the cue is not to be heard then. `onset` is the
    /// free-running cue's on-air instant, kept by the caller.
    fn due(
        &self,
        timeline: Option<&TransportTimeline>,
        t: i64,
        onset: &mut Option<i64>,
    ) -> Option<(i64, f32)> {
        let Some(state) = timeline.map(|tl| tl.state_at(t)) else {
            *onset = None;
            return None;
        };
        if !state.active {
            *onset = None;
            return None;
        }
        let show_ns = state.show_time_at(t);
        if show_ns < self.start_ns || show_ns >= self.end_ns {
            *onset = None;
            return None;
        }
        let elapsed_ns = if self.free_run {
            let on =
                *onset.get_or_insert_with(|| crate::app::on_air_since(&state, self.start_s, t));
            t - on
        } else {
            *onset = None;
            if !normal_speed(&state) {
                return None;
            }
            show_ns - self.start_ns
        };
        let elapsed =
            (elapsed_ns.max(0) as i128 * SAMPLE_RATE as i128 / NANOS_PER_SEC as i128) as i64;
        let media = match self.loop_samples {
            Some(len) if len > self.in_samples => {
                self.in_samples + elapsed.rem_euclid(len - self.in_samples)
            }
            _ => self.in_samples + elapsed,
        };
        if media >= self.end_samples {
            return None;
        }
        Some((media, self.fade_at(show_ns as f64 / NANOS_PER_SEC as f64)))
    }

    /// Add media sample `media` × `gain` into one output frame.
    #[inline]
    fn add(&self, samples: &[f32], channels: usize, media: i64, gain: f32, out: &mut [f32]) {
        let f = media - self.offset_samples;
        if f < 0 || f as u64 >= self.file.frames() || gain == 0.0 {
            return;
        }
        let fch = self.file.channels() as usize;
        let base = f as usize * fch;
        for r in &self.routes {
            if let (Some(s), Some(o)) = (
                samples.get(base + r.src as usize),
                out.get_mut(r.dst as usize),
            ) {
                if (r.dst as usize) < channels {
                    *o += s * r.gain * gain;
                }
            }
        }
    }
}

/// Sound is heard only at normal speed (or from free-running cues).
fn normal_speed(s: &TransportState) -> bool {
    s.playing && s.rate_permille == 1000
}

/// Everything this Runner plays.
pub struct Plan {
    pub id: u64,
    pub voices: Vec<Voice>,
}

/// A voice's playing state, kept from one frame to the next.
#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceState {
    /// The media sample played last, if the cue was heard.
    prev: Option<i64>,
    prev_gain: f32,
    /// A free-running cue's on-air instant.
    onset: Option<i64>,
    /// The old stream fading out after a jump: next sample, frames left,
    /// gain it had.
    tail: Option<(i64, u32, f32)>,
    /// Frames of fade-in left after a jump.
    fade_in: u32,
}

impl VoiceState {
    fn busy(&self) -> bool {
        self.prev.is_some() || self.tail.is_some()
    }
}

/// Computes grid frames.
pub struct Mixer {
    plan: Arc<Plan>,
    states: Vec<VoiceState>,
    /// Device channels the plan sends anything to: `0 .. width`. The rest
    /// stay silent and are not resampled (a 64-channel card playing a
    /// stereo show resamples two).
    width: usize,
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer {
            plan: Arc::new(Plan {
                id: 0,
                voices: Vec::new(),
            }),
            states: Vec::new(),
            width: 0,
        }
    }
}

impl Mixer {
    /// Take a new plan; `states` must have one entry per voice (allocated by
    /// the caller, off the audio thread). A cue in both plans keeps its
    /// state, so editing the show does not interrupt what plays. Returns the
    /// old plan and states, to be dropped off the audio thread.
    pub fn replace(
        &mut self,
        plan: Arc<Plan>,
        mut states: Vec<VoiceState>,
    ) -> (Arc<Plan>, Vec<VoiceState>) {
        for (new, state) in plan.voices.iter().zip(states.iter_mut()) {
            if let Some(i) = self.plan.voices.iter().position(|v| v.key == new.key) {
                *state = self.states[i];
            }
        }
        self.width = plan
            .voices
            .iter()
            .flat_map(|v| &v.routes)
            .map(|r| r.dst as usize + 1)
            .max()
            .unwrap_or(0);
        let old_plan = std::mem::replace(&mut self.plan, plan);
        let old_states = std::mem::replace(&mut self.states, states);
        (old_plan, old_states)
    }

    /// After a jump of the output: every cue fades in afresh.
    pub fn break_continuity(&mut self) {
        for s in &mut self.states {
            *s = VoiceState {
                onset: s.onset,
                ..Default::default()
            };
        }
    }

    /// Device channels in use: `0 .. width`.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Cues being heard.
    pub fn busy(&self) -> u32 {
        self.states.iter().filter(|s| s.busy()).count() as u32
    }

    /// Add grid frames `g0 .. g0 + n` into `out` (`n × channels`,
    /// interleaved, already zeroed).
    pub fn mix(&mut self, snap: &Snapshot, g0: i64, n: usize, channels: usize, out: &mut [f32]) {
        let filtered = snap.plan_id == self.plan.id;
        let plan = &self.plan;
        for (vi, voice) in plan.voices.iter().enumerate() {
            let st = &mut self.states[vi];
            if filtered && !st.busy() && !snap.active.get(vi).copied().unwrap_or(true) {
                st.onset = None;
                continue;
            }
            let timeline = snap.timeline(voice.timeline.as_deref());
            let samples = voice.file.samples();
            for j in 0..n {
                let frame = &mut out[j * channels..(j + 1) * channels];
                let t = grid_ns(g0 + j as i64);
                match voice.due(timeline, t, &mut st.onset) {
                    Some((media, gain)) => {
                        let media = match st.prev {
                            Some(prev) => {
                                let expected = voice.next(prev);
                                if (media - expected).abs() <= 1 && expected < voice.end_samples {
                                    expected
                                } else {
                                    // A jump: fade the old stream out, the
                                    // new one in.
                                    st.tail = Some((prev + 1, DECLICK, st.prev_gain));
                                    st.fade_in = DECLICK;
                                    media
                                }
                            }
                            None => {
                                // Starting: at once from the in point, with
                                // a fade anywhere else.
                                st.fade_in = if (media - voice.in_samples).abs() <= 1 {
                                    0
                                } else {
                                    DECLICK
                                };
                                media
                            }
                        };
                        let ramp = 1.0 - st.fade_in as f32 / DECLICK as f32;
                        st.fade_in = st.fade_in.saturating_sub(1);
                        voice.add(samples, channels, media, gain * ramp, frame);
                        st.prev = Some(media);
                        st.prev_gain = gain;
                    }
                    None => {
                        if let Some(prev) = st.prev.take() {
                            st.tail = Some((prev + 1, DECLICK, st.prev_gain));
                        }
                    }
                }
                if let Some((next, left, gain)) = st.tail {
                    voice.add(
                        samples,
                        channels,
                        next,
                        gain * left as f32 / DECLICK as f32,
                        frame,
                    );
                    st.tail = (left > 1).then_some((next + 1, left - 1, gain));
                }
            }
        }
    }
}

/// Figures the callback publishes for the health report.
#[derive(Default)]
pub struct Stats {
    pub drift_ppm: AtomicU64,
    pub sync_error_us: AtomicU64,
    pub latency_ms: AtomicU64,
    pub realigns: AtomicU64,
    pub late: AtomicU64,
    pub voices: AtomicU32,
    pub callbacks: AtomicU64,
    /// Frames in the last callback.
    pub buffer_frames: AtomicU32,
    /// Peak per channel since the control thread last read it (f32 bits).
    pub peaks: Vec<AtomicU32>,
}

impl Stats {
    pub fn new(channels: usize) -> Stats {
        Stats {
            peaks: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            ..Default::default()
        }
    }

    pub fn get(a: &AtomicU64) -> f64 {
        f64::from_bits(a.load(Ordering::Relaxed))
    }

    pub fn set(a: &AtomicU64, v: f64) {
        a.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Read and reset the peaks.
    pub fn take_peaks(&self) -> Vec<f32> {
        self.peaks
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
            .collect()
    }
}

/// The position loop.
#[derive(Debug, Default, Clone, Copy)]
struct Servo {
    /// Filtered error, seconds (positive: the device plays behind).
    filtered: f64,
    integral: f64,
    primed: bool,
}

impl Servo {
    fn update(&mut self, error_s: f64, dt: f64) {
        if self.primed {
            let a = 1.0 - (-dt / FILTER_S).exp();
            self.filtered += a * (error_s - self.filtered);
        } else {
            self.filtered = error_s;
            self.primed = true;
        }
        // The integral learns the drift. A step (the device lost or
        // repeated a buffer; the clock was stepped) is not drift: what is
        // integrated is capped, so a step is slewed out mostly by the
        // proportional term and barely disturbs the drift learnt.
        let learn = self.filtered.clamp(-INTEGRATE_CAP_S, INTEGRATE_CAP_S);
        self.integral =
            (self.integral + learn * dt).clamp(-MAX_CORRECTION / KI, MAX_CORRECTION / KI);
    }

    /// Relative change of the step: positive reads the grid faster.
    fn correction(&self) -> f64 {
        (KP * self.filtered + KI * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION)
    }
}

/// The output: mixer, ring of grid frames, resampler, position loop.
pub struct Engine {
    pub mixer: Mixer,
    sinc: Sinc,
    channels: usize,
    device_rate: u32,
    /// Grid frames per device frame, nominally.
    step_nominal: f64,
    /// `RING` grid frames, interleaved; frame `g` is at slot `g % RING`.
    ring: Vec<f32>,
    /// Grid frames `ring_start .. ring_end` are in the ring.
    ring_start: i64,
    ring_end: i64,
    /// The grid position of the next device frame: whole and fraction.
    pos: Option<(i64, f64)>,
    servo: Servo,
    /// The Runner's delay, ns.
    pub delay_ns: i64,
    pub stats: Arc<Stats>,
    weights: [f32; TAPS],
}

impl Engine {
    pub fn new(channels: usize, device_rate: u32, stats: Arc<Stats>) -> Engine {
        let channels = channels.max(1);
        Engine {
            mixer: Mixer::default(),
            sinc: Sinc::new(device_rate as f64 / SAMPLE_RATE as f64),
            channels,
            device_rate,
            step_nominal: SAMPLE_RATE as f64 / device_rate as f64,
            ring: vec![0.0; RING * channels],
            ring_start: 0,
            ring_end: 0,
            pos: None,
            servo: Servo::default(),
            delay_ns: 0,
            stats,
            weights: [0.0; TAPS],
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Fill `out` (interleaved, `channels` per frame) with the sound whose
    /// first frame the device plays at local time `playback_local_ns`.
    /// `snap`: `None` before the control thread's first snapshot.
    pub fn render(&mut self, out: &mut [f32], playback_local_ns: i64, snap: Option<&Snapshot>) {
        let frames = out.len() / self.channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CHUNK);
            let at = playback_local_ns
                + (done as i128 * NANOS_PER_SEC as i128 / self.device_rate as i128) as i64;
            let chunk = &mut out[done * self.channels..(done + n) * self.channels];
            self.render_chunk(chunk, n, at, snap);
            done += n;
        }
        self.stats
            .voices
            .store(self.mixer.busy(), Ordering::Relaxed);
        for (k, p) in self.stats.peaks.iter().enumerate().take(self.mixer.width()) {
            let peak = out
                .iter()
                .skip(k)
                .step_by(self.channels)
                .fold(0f32, |m, s| m.max(s.abs()));
            if peak > f32::from_bits(p.load(Ordering::Relaxed)) {
                p.store(peak.to_bits(), Ordering::Relaxed);
            }
        }
    }

    fn render_chunk(&mut self, out: &mut [f32], n: usize, at_local: i64, snap: Option<&Snapshot>) {
        let Some(snap) = snap.filter(|s| s.clock.synced) else {
            out.fill(0.0);
            self.pos = None;
            return;
        };
        let content = snap.clock.cluster(at_local) - self.delay_ns;
        let target = grid_pos(content);
        let error = self
            .pos
            .map(|(w, f)| ((target.0 - w) as f64 + (target.1 - f)) / SAMPLE_RATE as f64);
        match error {
            Some(e) if e.abs() <= REALIGN_S => {
                self.servo.update(e, n as f64 / self.device_rate as f64);
            }
            _ => {
                if self.pos.is_some() {
                    self.stats.realigns.fetch_add(1, Ordering::Relaxed);
                }
                self.pos = Some(target);
                self.ring_start = target.0 - HALF + 1;
                self.ring_end = self.ring_start;
                self.servo = Servo::default();
                self.mixer.break_continuity();
            }
        }
        let correction = self.servo.correction();
        Stats::set(&self.stats.drift_ppm, -KI * self.servo.integral * 1e6);
        Stats::set(&self.stats.sync_error_us, self.servo.filtered * 1e6);
        let step = self.step_nominal * (1.0 + correction);
        let (whole, frac) = self.pos.expect("set above");
        // Grid frames needed: around every position of this chunk.
        let last = whole + (frac + step * n as f64).floor() as i64 + HALF;
        self.fill(snap, last);
        let ch = self.channels;
        let width = self.mixer.width().min(ch);
        out.fill(0.0);
        for k in 0..n {
            let p = frac + step * k as f64;
            let i = whole + p.floor() as i64;
            self.sinc.weights(p - p.floor(), &mut self.weights);
            let frame = &mut out[k * ch..k * ch + width];
            for (t, w) in self.weights.iter().enumerate() {
                let g = i - HALF + 1 + t as i64;
                let slot = g.rem_euclid(RING as i64) as usize * ch;
                for (o, s) in frame.iter_mut().zip(&self.ring[slot..slot + width]) {
                    *o += s * w;
                }
            }
        }
        let advanced = frac + step * n as f64;
        let new_whole = whole + advanced.floor() as i64;
        self.pos = Some((new_whole, advanced - advanced.floor()));
        self.ring_start = self.ring_start.max(new_whole - HALF + 1);
    }

    /// Mix grid frames up to and including `last` into the ring.
    fn fill(&mut self, snap: &Snapshot, last: i64) {
        // Never more than the ring holds (a stalled device catching up).
        if last - self.ring_start >= RING as i64 {
            self.ring_start = last - RING as i64 + 1;
            self.ring_end = self.ring_end.max(self.ring_start);
        }
        let ch = self.channels;
        while self.ring_end <= last {
            let slot = self.ring_end.rem_euclid(RING as i64) as usize;
            let n = ((last - self.ring_end + 1) as usize).min(RING - slot);
            let region = &mut self.ring[slot * ch..(slot + n) * ch];
            region.fill(0.0);
            self.mixer.mix(snap, self.ring_end, n, ch, region);
            self.ring_end += n as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const S: i64 = NANOS_PER_SEC;

    fn tone_file(name: &str, frames: usize) -> Arc<PcmFile> {
        // Each sample holds its own frame number (scaled): positions can be
        // read back from the output.
        let samples: Vec<f32> = (0..frames)
            .map(|i| (i % 1000) as f32 / 1000.0 + 0.001)
            .collect();
        let path: PathBuf =
            std::env::temp_dir().join(format!("lcplay-mix-{name}-{}.wav", std::process::id()));
        super::super::pcm::write_wav(&path, 1, SAMPLE_RATE, &samples).unwrap();
        let f = Arc::new(PcmFile::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        f
    }

    fn voice(file: Arc<PcmFile>, start_s: f64, len_s: f64) -> Voice {
        Voice {
            key: "c".into(),
            timeline: None,
            start_s,
            end_s: start_s + len_s,
            fade_in_s: 0.0,
            fade_out_s: 0.0,
            start_ns: (start_s * S as f64) as i64,
            end_ns: ((start_s + len_s) * S as f64) as i64,
            in_samples: 0,
            loop_samples: None,
            end_samples: file.frames() as i64,
            offset_samples: 0,
            free_run: false,
            file,
            routes: vec![Route {
                src: 0,
                dst: 0,
                gain: 1.0,
            }],
        }
    }

    fn snapshot(state: TransportState) -> Snapshot {
        Snapshot {
            clock: ClockMap {
                local0: 0,
                cluster0: 0,
                slope: 1.0,
                synced: true,
            },
            main: TransportTimeline::new(state),
            tasks: Vec::new(),
            plan_id: 1,
            active: vec![true],
        }
    }

    fn playing_from(anchor_cluster_ns: i64) -> TransportState {
        TransportState {
            epoch: 1,
            playing: true,
            anchor_cluster_ns,
            ..TransportState::STOPPED
        }
    }

    fn mixer_with(v: Voice) -> Mixer {
        let mut m = Mixer::default();
        m.replace(
            Arc::new(Plan {
                id: 1,
                voices: vec![v],
            }),
            vec![VoiceState::default()],
        );
        m
    }

    #[test]
    fn grid_and_cluster_time_agree() {
        assert_eq!(grid_ns(48_000), S);
        assert_eq!(grid_ns(1), 20_833);
        assert_eq!(grid_pos(S), (48_000, 0.0));
        let (w, f) = grid_pos(S + 10_417);
        assert_eq!(w, 48_000);
        assert!((f - 0.5).abs() < 1e-3);
        assert_eq!(grid_pos(-1).0, -1);
    }

    #[test]
    fn a_cue_starts_on_its_exact_sample_and_plays_continuously() {
        // Playing since cluster 0; the cue starts at show 1 s.
        let file = tone_file("start", 96_000);
        let mut m = mixer_with(voice(file.clone(), 1.0, 1.0));
        let snap = snapshot(playing_from(0));
        let mut out = vec![0f32; 1000];
        // Grid frames 47_500 .. 48_500: the cue starts at frame 48_000.
        m.mix(&snap, 47_500, 1000, 1, &mut out);
        assert!(out[..500].iter().all(|&s| s == 0.0), "silent before");
        for k in 0..500 {
            assert_eq!(
                out[500 + k],
                file.samples()[k],
                "sample {k}, no fade at the in point"
            );
        }
    }

    #[test]
    fn pausing_fades_out_in_five_milliseconds_without_a_click() {
        let file = tone_file("pause", 96_000);
        let mut m = mixer_with(voice(file, 0.0, 2.0));
        // Play from 0, pause at cluster 0.5 s.
        let mut snap = snapshot(playing_from(0));
        snap.main.push(TransportState {
            epoch: 2,
            playing: false,
            anchor_cluster_ns: S / 2,
            anchor_show_ns: S / 2,
            ..TransportState::STOPPED
        });
        let mut out = vec![0f32; 2000];
        m.mix(&snap, 23_000, 2000, 1, &mut out);
        let pause = 24_000 - 23_000;
        assert!(out[pause - 1] > 0.0);
        // Fading, then silent.
        assert!(out[pause] > 0.0 && out[pause] <= out[pause - 1] + 0.01);
        assert!(out[pause + DECLICK as usize + 1..]
            .iter()
            .all(|&s| s == 0.0));
    }

    #[test]
    fn a_looping_file_loops_seamlessly_and_off_speed_is_silent() {
        let file = tone_file("loop", 4_800);
        let mut v = voice(file, 0.0, 10.0);
        v.loop_samples = Some(4_800);
        let mut m = mixer_with(v);
        let snap = snapshot(playing_from(0));
        // From the cue's start (no fade), up to just before the loop.
        let mut before = vec![0f32; 4_700];
        m.mix(&snap, 0, 4_700, 1, &mut before);
        let mut out = vec![0f32; 200];
        m.mix(&snap, 4_700, 200, 1, &mut out);
        let samples = |i: usize| (i % 1000) as f32 / 1000.0 + 0.001;
        assert_eq!(out[99], samples(4_799));
        assert_eq!(out[100], samples(0), "back to the start, no fade");

        let mut half = snapshot(TransportState {
            rate_permille: 500,
            ..playing_from(0)
        });
        half.active = vec![true];
        let mut m2 = mixer_with(voice(tone_file("half", 4_800), 0.0, 10.0));
        let mut out = vec![0f32; 100];
        m2.mix(&half, 1000, 100, 1, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn the_engine_locks_onto_a_device_that_runs_fast() {
        // A device 80 ppm fast: its frames are heard 80 ppm sooner than
        // nominal. After a minute the engine must be in step, having
        // measured the drift.
        let stats = Arc::new(Stats::new(1));
        let mut e = Engine::new(1, SAMPLE_RATE, stats.clone());
        let snap = snapshot(TransportState::STOPPED);
        let period = 480; // 10 ms callbacks
        let mut out = vec![0f32; period];
        let ns_per_frame = 1e9 / (SAMPLE_RATE as f64 * (1.0 + 80e-6));
        let mut played = 0u64;
        for _ in 0..6000 {
            let at = (played as f64 * ns_per_frame) as i64 + 5 * S;
            e.render(&mut out, at, Some(&snap));
            played += period as u64;
        }
        let drift = Stats::get(&stats.drift_ppm);
        let error = Stats::get(&stats.sync_error_us);
        assert!((drift - 80.0).abs() < 3.0, "drift {drift}");
        assert!(error.abs() < 20.0, "error {error} µs");
        assert_eq!(stats.realigns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_step_is_slewed_out_without_unlearning_the_drift() {
        // Locked onto a device 50 ppm fast, which then loses 6 ms (a
        // dropped buffer): no jump, back in step within a minute, and the
        // drift learnt stays near the truth meanwhile.
        let stats = Arc::new(Stats::new(1));
        let mut e = Engine::new(1, SAMPLE_RATE, stats.clone());
        let snap = snapshot(TransportState::STOPPED);
        let period = 480;
        let mut out = vec![0f32; period];
        let ns_per_frame = 1e9 / (SAMPLE_RATE as f64 * (1.0 + 50e-6));
        let mut played = 0u64;
        let mut worst_drift = 0f64;
        for k in 0..18_000 {
            // 6 ms late from minute one on.
            let late = if k >= 6000 { 6 * S / 1000 } else { 0 };
            let at = (played as f64 * ns_per_frame) as i64 + 5 * S + late;
            e.render(&mut out, at, Some(&snap));
            played += period as u64;
            if k >= 6000 {
                worst_drift = worst_drift.max((Stats::get(&stats.drift_ppm) - 50.0).abs());
            }
        }
        let error = Stats::get(&stats.sync_error_us);
        assert_eq!(stats.realigns.load(Ordering::Relaxed), 0);
        assert!(error.abs() < 50.0, "error {error} µs");
        assert!(
            worst_drift < 150.0,
            "drift estimate off by {worst_drift} ppm"
        );
    }

    #[test]
    fn a_large_error_realigns_at_once() {
        let stats = Arc::new(Stats::new(1));
        let mut e = Engine::new(1, SAMPLE_RATE, stats.clone());
        let snap = snapshot(TransportState::STOPPED);
        let mut out = vec![0f32; 480];
        e.render(&mut out, 0, Some(&snap));
        // The next callback comes 1 s late: a device that stalled.
        e.render(&mut out, S, Some(&snap));
        assert_eq!(stats.realigns.load(Ordering::Relaxed), 1);
    }
}
