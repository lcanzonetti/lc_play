//! The sound devices, through the operating system's own drivers (cpal):
//! listing them for the Producer, and opening the one the show names.
//!
//! * **Listing** goes through every driver interface cpal has here (WASAPI
//!   on Windows, plus ASIO when built with the `asio` feature; ALSA on
//!   Linux; CoreAudio on macOS). A device that cannot say what it supports
//!   (an ASIO driver while another ASIO driver is open — ASIO loads one
//!   driver per process) is listed with what is known, never dropped
//!   silently.
//! * **Opening**: the device runs at *its own* rate (WASAPI's mix format,
//!   the ASIO driver's current rate). Asking WASAPI for 48 kHz on a device
//!   set to 44.1 would put Windows' own converter in the path; ours is
//!   better, and it corrects the clock drift anyway. All of its output
//!   channels are opened, in the best sample format it offers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context as _};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use lc_proto::audio::{AudioDeviceInfo, AudioDeviceRef};

/// The rates listed as supported, when a device accepts a range.
const COMMON_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Every sound device of every driver interface.
pub fn list() -> Vec<AudioDeviceInfo> {
    let mut out = Vec::new();
    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("audio: {} unavailable: {e}", host_id.name());
                continue;
            }
        };
        let id_of = |d: Option<cpal::Device>| d.and_then(|d| d.id().ok()).map(|id| id.to_string());
        let default_out = id_of(host.default_output_device());
        let default_in = id_of(host.default_input_device());
        let devices = match host.devices() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("audio: listing {} devices: {e}", host_id.name());
                continue;
            }
        };
        for device in devices {
            let Ok(id) = device.id().map(|id| id.to_string()) else {
                continue;
            };
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| id.clone());
            let mut rates = Vec::new();
            let mut channels = |configs: Result<Vec<cpal::SupportedStreamConfigRange>, _>| {
                let configs = configs.unwrap_or_default();
                for c in &configs {
                    for r in COMMON_RATES {
                        if c.contains_rate(r) && !rates.contains(&r) {
                            rates.push(r);
                        }
                    }
                }
                configs
                    .iter()
                    .map(|c| c.channels() as u32)
                    .max()
                    .unwrap_or(0)
            };
            let output_channels = channels(
                device
                    .supported_output_configs()
                    .map(|c| c.collect::<Vec<_>>()),
            );
            let input_channels = channels(
                device
                    .supported_input_configs()
                    .map(|c| c.collect::<Vec<_>>()),
            );
            rates.sort_unstable();
            out.push(AudioDeviceInfo {
                host: host_id.name().to_string(),
                default_output: default_out.as_deref() == Some(id.as_str()),
                default_input: default_in.as_deref() == Some(id.as_str()),
                id,
                name,
                output_channels,
                input_channels,
                sample_rates: rates,
            });
        }
    }
    out
}

/// Find the device `r` names: by its id, else by host and name (a device
/// moved to another USB port gets a new id on some systems).
pub fn find(r: &AudioDeviceRef) -> anyhow::Result<cpal::Device> {
    if let Ok(id) = r.id.parse::<cpal::DeviceId>() {
        if let Ok(host) = cpal::host_from_id(id.host()) {
            if let Some(d) = host.device_by_id(&id) {
                return Ok(d);
            }
        }
    }
    for host_id in cpal::available_hosts() {
        if !r.host.is_empty() && !r.host.eq_ignore_ascii_case(host_id.name()) {
            continue;
        }
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.output_devices() else {
            continue;
        };
        for d in devices {
            if d.description().is_ok_and(|desc| desc.name() == r.name) {
                return Ok(d);
            }
        }
    }
    bail!("{}: {} not found", r.host, r.name)
}

/// What happens to an open output, as the driver tells it (written from
/// the driver's threads).
#[derive(Default)]
pub struct Events {
    /// Buffers the device ran out of data on.
    pub xruns: AtomicU64,
    /// The stream is gone (device unplugged, driver reset): reopen it.
    pub failed: AtomicBool,
    pub error: Mutex<Option<String>>,
}

/// An open output stream. Dropping it stops the sound.
pub struct Output {
    _stream: cpal::Stream,
    pub label: String,
    pub channels: usize,
    pub sample_rate: u32,
    pub events: Arc<Events>,
}

/// What the stream's callback is asked to do: fill `out` (interleaved
/// f32, zeroed) whose first frame is heard at local time `playback_ns`.
pub trait Fill: Send + 'static {
    fn fill(&mut self, out: &mut [f32], playback_ns: i64);
    /// The local clock (the one `playback_ns` is on), read in the callback.
    fn local_now_ns(&self) -> i64;
}

/// Open device `r` for output. `make` builds the callback's state once the
/// channel count and rate are known.
pub fn open<F: Fill>(
    r: &AudioDeviceRef,
    make: impl FnOnce(usize, u32) -> F,
) -> anyhow::Result<Output> {
    let device = find(r)?;
    let label = format!(
        "{}: {}",
        r.host,
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| r.name.clone())
    );
    let default = device
        .default_output_config()
        .with_context(|| format!("{label}: no output"))?;
    let rate = default.sample_rate();
    // All channels at the device's own rate, in the best format.
    let rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 4,
        SampleFormat::I32 => 3,
        SampleFormat::I24 => 2,
        SampleFormat::I16 => 1,
        _ => 0,
    };
    let best = device
        .supported_output_configs()
        .map(|c| c.collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.contains_rate(rate) && rank(c.sample_format()) > 0)
        .max_by_key(|c| (c.channels(), rank(c.sample_format())))
        .map(|c| c.with_sample_rate(rate))
        .unwrap_or(default);
    let config = cpal::StreamConfig {
        channels: best.channels(),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let channels = config.channels as usize;
    let feed = make(channels, rate);
    let events = Arc::new(Events::default());
    let stream = match best.sample_format() {
        SampleFormat::F32 => build::<f32, F>(&device, config, feed, events.clone()),
        SampleFormat::I32 => build::<i32, F>(&device, config, feed, events.clone()),
        SampleFormat::I24 => build::<cpal::I24, F>(&device, config, feed, events.clone()),
        SampleFormat::I16 => build::<i16, F>(&device, config, feed, events.clone()),
        other => Err(anyhow!("{label}: {other} samples are not supported")),
    }?;
    stream
        .play()
        .with_context(|| format!("{label}: starting"))?;
    Ok(Output {
        _stream: stream,
        label,
        channels,
        sample_rate: rate,
        events,
    })
}

/// Most frames converted at once (the callback works through bigger
/// buffers in pieces): the scratch buffer is allocated before the stream
/// starts, never in the callback.
const SCRATCH_FRAMES: usize = 4096;

fn build<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut feed: F,
    events: Arc<Events>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
    F: Fill,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate as i64;
    let mut scratch = vec![0f32; SCRATCH_FRAMES * channels];
    let on_error = events.clone();
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                // The driver says how far ahead of this callback its first
                // frame is heard; that distance, added to our own clock
                // read now, is when on our clock (the two clocks need not
                // share an origin, only a rate over a few milliseconds).
                let ts = info.timestamp();
                let ahead = ts
                    .playback
                    .checked_duration_since(ts.callback)
                    .map_or(0, |d| d.as_nanos() as i64);
                let mut at = feed.local_now_ns() + ahead;
                for piece in data.chunks_mut(SCRATCH_FRAMES * channels) {
                    let frames = piece.len() / channels;
                    let buf = &mut scratch[..piece.len()];
                    buf.fill(0.0);
                    feed.fill(buf, at);
                    for (o, s) in piece.iter_mut().zip(buf.iter()) {
                        *o = T::from_sample(s.clamp(-1.0, 1.0));
                    }
                    at += frames as i64 * 1_000_000_000 / rate;
                }
            },
            move |e: cpal::Error| match e.kind() {
                cpal::ErrorKind::Xrun => {
                    on_error.xruns.fetch_add(1, Ordering::Relaxed);
                }
                cpal::ErrorKind::RealtimeDenied => {
                    tracing::warn!("audio: real-time priority refused: {e}");
                }
                cpal::ErrorKind::DeviceChanged => {}
                _ => {
                    tracing::error!("audio: {e}");
                    *on_error.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                    on_error.failed.store(true, Ordering::Relaxed);
                }
            },
            None,
        )
        .context("opening the output")?;
    Ok(stream)
}
