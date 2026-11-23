//! Audio: the project's buses, where each Runner sends them, and what the
//! Runners report about their sound hardware.
//!
//! ```text
//!   cue channel ──▶ project bus ──▶ device channel(s) on the Runner(s)
//!   (per cue)       (the show)       that map that bus
//! ```
//!
//! * **Buses** are logical outputs of the project ("Left", "Right", "Sub" …),
//!   numbered from 0. Nothing about a bus says where it is heard: moving a
//!   show to another rig only changes the per-Runner mapping.
//! * **Per Runner** ([`RunnerAudio`]): one output device, and for each of its
//!   channels the bus it plays (or none); a delay for lip-sync with the
//!   displays; and one input device, used only to read LTC timecode.
//! * **Per cue** ([`CueAudio`]): for each channel of the cue's file, the bus
//!   it feeds (or none), a gain and a mute.
//!
//! A bus that no channel on a Runner plays is not mixed on that Runner at
//! all. A bus may be played by several Runners: they stay within the
//! cluster clock's accuracy (well under a millisecond) of each other, which
//! is right for separate zones but not for speakers covering the same area
//! (comb filtering) — the Producer warns about that.
//!
//! Every Runner mixes at [`SAMPLE_RATE`]; the Asset Manager extracts each
//! file's audio once, at that rate, into a PCM sidecar ([`AssetAudio`]) that
//! Runners read without decoding anything at show time.

use serde::{Deserialize, Serialize};

/// The one sample rate of the system, in Hz: sidecars are made at it and
/// Runners mix at it (a device running at another rate is resampled).
pub const SAMPLE_RATE: u32 = 48_000;

/// Most buses a project may have.
pub const MAX_BUSES: usize = 128;
/// Most channels a device mapping may list (Dante Virtual Soundcard has 64;
/// MADI cards 64 to 128).
pub const MAX_DEVICE_CHANNELS: usize = 256;
/// Most channels a cue's routing may list.
pub const MAX_CUE_CHANNELS: usize = 64;

/// The project's audio routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSetup {
    /// The project's buses; a bus is referred to by its index.
    pub buses: Vec<AudioBus>,
    /// Per-Runner devices and mappings.
    pub runners: Vec<RunnerAudio>,
}

impl Default for AudioSetup {
    fn default() -> Self {
        AudioSetup {
            buses: vec![
                AudioBus {
                    name: "Left".into(),
                },
                AudioBus {
                    name: "Right".into(),
                },
            ],
            runners: Vec::new(),
        }
    }
}

impl AudioSetup {
    /// The audio settings of the Runner called `runner`, if any.
    pub fn for_runner(&self, runner: &str) -> Option<&RunnerAudio> {
        self.runners.iter().find(|r| r.runner == runner)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioBus {
    pub name: String,
}

/// One Runner's sound hardware and how the project's buses reach it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerAudio {
    /// The Runner's name (its `--name`), as for displays.
    pub runner: String,
    /// The device the buses are played on. `None`: no sound from this
    /// Runner.
    pub output: Option<AudioDeviceRef>,
    /// For each channel of the output device, from the first: the bus it
    /// plays, or `null` for silence. Channels past the end of the list are
    /// silent. Several channels may play the same bus.
    pub output_channels: Vec<Option<u32>>,
    /// Delay added to everything this Runner plays, in milliseconds: the
    /// display chain's latency (projector, LED processor) for lip-sync, or
    /// a downstream console's. Negative values play earlier — possible,
    /// since every sample is known ahead of time.
    pub delay_ms: f64,
    /// Output level in dB applied to the whole device (0 = unchanged).
    pub gain_db: f32,
    /// The device LTC timecode is read from. `None`: no LTC on this Runner.
    pub input: Option<AudioDeviceRef>,
    /// Channel of the input device carrying LTC, from 0.
    pub ltc_channel: u32,
}

impl RunnerAudio {
    /// The bus device channel `channel` plays.
    pub fn bus_of_channel(&self, channel: usize) -> Option<u32> {
        self.output_channels.get(channel).copied().flatten()
    }
}

/// A sound device, as a Runner reported it ([`AudioDeviceInfo`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDeviceRef {
    /// Driver interface: `WASAPI`, `ASIO`, `ALSA`, `CoreAudio` …
    pub host: String,
    /// The driver's stable identifier for the device, when it has one.
    pub id: String,
    /// The device's name: used when the id is not found (a device unplugged
    /// and plugged into another port gets a new id on some systems).
    pub name: String,
}

/// How a cue's sound is played.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CueAudio {
    /// For each channel of the cue's file, from the first: the bus it feeds,
    /// or `null` for none. Empty: channel *n* feeds bus *n* (a stereo file
    /// on a stereo project plays Left and Right).
    pub channel_buses: Vec<Option<u32>>,
    /// Level in dB (0 = as recorded).
    pub gain_db: f32,
    pub mute: bool,
}

impl Default for CueAudio {
    fn default() -> Self {
        CueAudio {
            channel_buses: Vec::new(),
            gain_db: 0.0,
            mute: false,
        }
    }
}

impl CueAudio {
    /// The bus channel `channel` of the file feeds.
    pub fn bus_of_channel(&self, channel: usize) -> Option<u32> {
        if self.channel_buses.is_empty() {
            u32::try_from(channel).ok()
        } else {
            self.channel_buses.get(channel).copied().flatten()
        }
    }
}

/// The audio of an asset, extracted by the Asset Manager into a PCM file:
/// WAV (RF64 when larger than 4 GB), 32-bit float, [`SAMPLE_RATE`],
/// interleaved.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AssetAudio {
    /// HTTP URL of the PCM file on the Asset Manager.
    pub url: String,
    pub size: u64,
    /// Where the PCM file is on the Asset Manager, relative to its media
    /// folder: Runners store their copy under the same name.
    pub path: String,
    pub channels: u32,
    /// Length in sample frames.
    pub frames: u64,
    /// Media time of the PCM file's first sample, in seconds: the audio
    /// stream's start minus the video stream's (a few milliseconds of
    /// encoder delay, typically). Zero for audio-only files.
    pub offset_s: f64,
}

/// A sound device a Runner has, as it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDeviceInfo {
    pub host: String,
    pub id: String,
    pub name: String,
    /// Channels of its output and input sides (0 = none).
    pub output_channels: u32,
    pub input_channels: u32,
    /// Sample rates it can open at, in Hz, as far as the driver says.
    pub sample_rates: Vec<u32>,
    /// The system's default output (or input) device.
    pub default_output: bool,
    pub default_input: bool,
}

/// What a Runner's audio output is doing, for the health report.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioHealth {
    /// `off` (no device set), `running`, `error`.
    pub state: String,
    pub error: Option<String>,
    /// The device playing, as `host: name`.
    pub device: String,
    pub channels: u32,
    /// The rate the device runs at.
    pub sample_rate: u32,
    /// Frames per callback.
    pub buffer_frames: u32,
    /// From a sample being computed to it leaving the device, in ms (as
    /// far as the driver reports it).
    pub latency_ms: f64,
    /// The device's clock against the cluster clock, in ppm (what the
    /// resampler corrects).
    pub drift_ppm: f64,
    /// Where the device is against where the cluster says it should be, in
    /// µs, after correction.
    pub sync_error_us: f64,
    /// Callbacks that found no data ready (heard as a dropout), since the
    /// device opened.
    pub underruns: u64,
    /// Times the output was re-aligned with a jump (error over 20 ms).
    pub realigns: u64,
    /// Cues being played.
    pub voices: u32,
    /// Peak level per device channel over the last report, 0..1 (for the
    /// Producer's meters).
    pub peaks: Vec<f32>,
}

/// Check a project's audio settings; the first problem, as a message.
pub(crate) fn validate(setup: &AudioSetup) -> Result<(), String> {
    if setup.buses.len() > MAX_BUSES {
        return Err(format!("audio: at most {MAX_BUSES} buses"));
    }
    let buses = setup.buses.len() as u32;
    let mut seen = std::collections::HashSet::new();
    for r in &setup.runners {
        let name = format!("audio of Runner '{}'", r.runner);
        if r.runner.is_empty() {
            return Err("audio: a Runner's settings need its name".into());
        }
        if !seen.insert(r.runner.as_str()) {
            return Err(format!("{name}: listed twice"));
        }
        if r.output_channels.len() > MAX_DEVICE_CHANNELS {
            return Err(format!(
                "{name}: at most {MAX_DEVICE_CHANNELS} device channels"
            ));
        }
        if let Some(b) = r.output_channels.iter().flatten().find(|&&b| b >= buses) {
            return Err(format!(
                "{name}: a channel plays bus {}, but the project has {buses}",
                b + 1
            ));
        }
        if !(r.delay_ms.is_finite() && (-1000.0..=1000.0).contains(&r.delay_ms)) {
            return Err(format!("{name}: delay must be -1000..1000 ms"));
        }
        if !(r.gain_db.is_finite() && (-120.0..=24.0).contains(&r.gain_db)) {
            return Err(format!("{name}: gain must be -120..24 dB"));
        }
        if r.ltc_channel as usize >= MAX_DEVICE_CHANNELS {
            return Err(format!("{name}: LTC channel out of range"));
        }
    }
    Ok(())
}

/// Check a cue's audio settings against the project's bus count.
pub(crate) fn validate_cue(audio: &CueAudio, buses: usize) -> Result<(), String> {
    if audio.channel_buses.len() > MAX_CUE_CHANNELS {
        return Err(format!("at most {MAX_CUE_CHANNELS} audio channels"));
    }
    if let Some(b) = audio
        .channel_buses
        .iter()
        .flatten()
        .find(|&&b| b as usize >= buses)
    {
        return Err(format!(
            "an audio channel feeds bus {}, but the project has {buses}",
            b + 1
        ));
    }
    if !(audio.gain_db.is_finite() && (-120.0..=24.0).contains(&audio.gain_db)) {
        return Err("audio gain must be -120..24 dB".into());
    }
    Ok(())
}

/// Linear amplitude of a level in dB.
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -120.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_starts_with_a_stereo_pair() {
        let setup: AudioSetup = serde_json::from_str("{}").unwrap();
        assert_eq!(setup.buses.len(), 2);
        assert_eq!(setup.buses[0].name, "Left");
        validate(&setup).unwrap();
    }

    #[test]
    fn unrouted_channels_are_null_and_default_routing_is_one_to_one() {
        let r: RunnerAudio = serde_json::from_value(serde_json::json!({
            "runner": "foh", "output_channels": [1, null, 0]
        }))
        .unwrap();
        assert_eq!(r.bus_of_channel(0), Some(1));
        assert_eq!(r.bus_of_channel(1), None);
        assert_eq!(r.bus_of_channel(2), Some(0));
        assert_eq!(r.bus_of_channel(3), None, "past the list: silent");

        let cue = CueAudio::default();
        assert_eq!(cue.bus_of_channel(0), Some(0));
        assert_eq!(cue.bus_of_channel(5), Some(5));
        let cue = CueAudio {
            channel_buses: vec![None, Some(0)],
            ..Default::default()
        };
        assert_eq!(cue.bus_of_channel(0), None);
        assert_eq!(cue.bus_of_channel(1), Some(0));
        assert_eq!(cue.bus_of_channel(2), None);
    }

    #[test]
    fn mappings_must_name_existing_buses() {
        let mut setup = AudioSetup::default();
        setup.runners.push(RunnerAudio {
            runner: "foh".into(),
            output_channels: vec![Some(0), Some(2)],
            ..Default::default()
        });
        assert!(validate(&setup).unwrap_err().contains("bus 3"));
        setup.runners[0].output_channels = vec![Some(0), Some(1)];
        validate(&setup).unwrap();
        setup.runners.push(setup.runners[0].clone());
        assert!(validate(&setup).unwrap_err().contains("twice"));

        let cue = CueAudio {
            channel_buses: vec![Some(4)],
            ..Default::default()
        };
        assert!(validate_cue(&cue, 2).is_err());
        assert!(validate_cue(&cue, 5).is_ok());
    }

    #[test]
    fn decibels() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-6.0) - 0.501).abs() < 1e-3);
        assert_eq!(db_to_gain(-120.0), 0.0);
    }
}
