//! Video playback for everything that is not GPU-native HAP: an `ffmpeg`
//! child process decodes, this module keeps its output in step with the
//! show clock.
//!
//! ## Why a child process
//!
//! `ffmpeg` on the `PATH` is the one decoder that exists on every platform,
//! reads every format an operator will throw at it, and needs no native
//! libraries at build time.
//!
//! ## Keeping it fast
//!
//! * **Hardware decoding, on the card that shows the video.** H.264, HEVC,
//!   VP9, AV1 … are decoded by a GPU's video engine (NVDEC, Quick Sync,
//!   VCN) where there is one, and in software otherwise. On Windows each
//!   layer is decoded on the card that renders it: ffmpeg is started with
//!   `-hwaccel d3d11va -hwaccel_device <DXGI adapter index>` of that card
//!   (see [`HwAccel`]), so a machine with several cards spreads decoding
//!   over them instead of piling it on the primary-display card, which is
//!   where `-hwaccel auto` lands. Elsewhere, and when the card is unknown,
//!   ffmpeg picks (`-hwaccel auto`). `LCPLAY_HWACCEL=none` decodes in
//!   software; `LCPLAY_HWACCEL=auto` restores ffmpeg's own pick everywhere.
//! * **YUV, not RGB.** Video is stored as YUV 4:2:0; converting it to RGBA on
//!   the CPU is expensive and makes every frame 2.7× larger. Frames therefore
//!   leave ffmpeg as three YUV planes (1.5 bytes per pixel) and the GPU
//!   converts them while drawing (see `shaders/scene.wgsl`). Only video with
//!   an alpha channel travels as RGBA.
//!
//! What remains is one copy per frame through a pipe and into a texture —
//! fine for a few HD or 4K layers. For very large canvases, HAP
//! ([`crate::hap`]) is the format to use.
//!
//! ## How it stays in sync
//!
//! Nothing here plays "in real time". Every rendered frame, the renderer
//! says what show time it is; that maps to an exact frame number in the file
//! (`frame = floor(media_time × fps)`), and the decoder's sequential output
//! is advanced until that frame comes out. Frames the show clock has already
//! passed are skipped; if the wanted frame is not decoded yet, the newest one
//! available is shown. Two Runners given the same show time therefore show
//! the same frame of the same file, as long as their decoders keep up.
//!
//! A jump (seek, loop, scrubbing) restarts `ffmpeg` at the new position.

use std::io::{BufRead, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};

use anyhow::Context as _;

use crate::content::{Content, Encoding, Sampling, TextureFactory};
use crate::show::Timing;

/// Shortest time between two decoder restarts of one layer.
const MIN_RESTART_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Decoded frames buffered ahead of the renderer. Small on purpose: each is
/// a full uncompressed frame.
const QUEUE_FRAMES: usize = 3;

fn ffmpeg_bin() -> String {
    std::env::var("LCPLAY_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn ffprobe_bin() -> String {
    std::env::var("LCPLAY_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
}

/// A command that does not open a console window on Windows (the Runner is
/// a windowed program; every ffmpeg it starts would otherwise flash one),
/// and runs below normal priority, so that decoding gives way to the render
/// loop (see `priority.rs`).
fn quiet_command(program: String) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let below = std::env::var("LCPLAY_THREAD_PRIORITY").map_or(true, |v| v != "0");
        command.creation_flags(if below {
            CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
        } else {
            CREATE_NO_WINDOW
        });
    }
    command
}

/// The frame of a file that belongs on screen at time `t`.
///
/// `t` is the layer's timeline time — or, for a free-running layer, the
/// seconds since it went on air (the renderer works that out). `frames` is
/// the file's length in frames, if known.
pub(crate) fn media_frame(timing: &Timing, fps: f64, frames: Option<i64>, t: f64) -> i64 {
    let first = (timing.media_in.max(0.0) * fps).floor() as i64;
    let since = if timing.free_run { t } else { t - timing.start };
    let elapsed = (since.max(0.0) * fps).floor() as i64;
    match frames {
        Some(total) if total > first => {
            if timing.media_loop {
                first + elapsed % (total - first)
            } else {
                // Past the end: hold the last frame.
                (first + elapsed).min(total - 1)
            }
        }
        _ => first + elapsed,
    }
}

/// What we need to know about a file before decoding it.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction, exactly as the container states it.
    pub fps_num: u32,
    pub fps_den: u32,
    /// Total frames, if the duration is known.
    pub frames: Option<i64>,
    /// The pixel format has an alpha channel.
    pub has_alpha: bool,
    /// Colour matrix: BT.601 (standard definition) rather than BT.709.
    pub bt601: bool,
}

impl VideoInfo {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den as f64
    }

    /// Ask `ffprobe` about the first video stream of `path`.
    pub fn probe(path: &str) -> anyhow::Result<VideoInfo> {
        let out = quiet_command(ffprobe_bin())
            .args(["-v", "error", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "stream=width,height,r_frame_rate,pix_fmt,color_space:format=duration",
            ])
            .args(["-of", "json", path])
            .stdin(Stdio::null())
            .output()
            .context("running ffprobe (is ffmpeg installed and on the PATH?)")?;
        if !out.status.success() {
            anyhow::bail!(
                "ffprobe cannot read '{path}': {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        let stream = json["streams"]
            .get(0)
            .with_context(|| format!("'{path}' has no video stream"))?;
        let dim = |key: &str| {
            stream[key]
                .as_u64()
                .filter(|&v| v > 0)
                .map(|v| v as u32)
                .with_context(|| format!("'{path}': no {key}"))
        };
        // "30000/1001" or "25/1".
        let rate = stream["r_frame_rate"].as_str().unwrap_or("0/1");
        let (num, den) = rate.split_once('/').unwrap_or((rate, "1"));
        let (fps_num, fps_den) = match (num.parse::<u32>(), den.parse::<u32>()) {
            (Ok(n), Ok(d)) if n > 0 && d > 0 => (n, d),
            _ => anyhow::bail!("'{path}': unusable frame rate '{rate}'"),
        };
        let duration: Option<f64> = json["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse().ok())
            .filter(|d: &f64| d.is_finite() && *d > 0.0);
        let pix_fmt = stream["pix_fmt"].as_str().unwrap_or("");
        let height = dim("height")?;
        // Tagged colour space if there is one, else the convention: HD and
        // above is BT.709, standard definition BT.601.
        let bt601 = match stream["color_space"].as_str().unwrap_or("") {
            "bt709" => false,
            "smpte170m" | "bt470bg" => true,
            _ => height < 720,
        };
        Ok(VideoInfo {
            width: dim("width")?,
            height,
            fps_num,
            fps_den,
            frames: duration
                .map(|d| (d * fps_num as f64 / fps_den as f64).round() as i64)
                .filter(|&f| f > 0),
            // yuva420p, rgba, bgra, argb, gbrap, ya8 …
            has_alpha: matches!(
                pix_fmt,
                "rgba" | "bgra" | "argb" | "abgr" | "ya8" | "ya16le" | "ya16be"
            ) || pix_fmt.starts_with("yuva")
                || pix_fmt.starts_with("gbrap")
                || pix_fmt.starts_with("rgba64")
                || pix_fmt.starts_with("bgra64"),
            bt601,
        })
    }
}

/// How decoded frames are laid out in the pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// RGBA, premultiplied, 4 bytes per pixel.
    Rgba,
    /// Planar YUV 4:2:0: Y (w×h), U and V (⌈w/2⌉×⌈h/2⌉ each).
    Yuv420,
}

impl Layout {
    fn frame_len(self, w: u32, h: u32) -> usize {
        let (w, h) = (w as usize, h as usize);
        match self {
            Layout::Rgba => w * h * 4,
            Layout::Yuv420 => w * h + 2 * w.div_ceil(2) * h.div_ceil(2),
        }
    }
}

/// What `LCPLAY_HWACCEL` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwMode {
    /// Decode on the card that renders the layer, where that card is known
    /// (the default).
    Card,
    /// Let ffmpeg pick (`-hwaccel auto`): the behaviour before decoders were
    /// bound to cards. On Windows that is the primary-display card.
    Auto,
    /// Software decoding only.
    None,
}

impl HwMode {
    /// The mode for a value of `LCPLAY_HWACCEL` (unset: [`HwMode::Card`]).
    /// An unknown value is reported and treated as unset.
    pub fn parse(value: Option<&str>) -> HwMode {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("card") | Some("d3d11va") => HwMode::Card,
            Some("auto") => HwMode::Auto,
            Some("none") => HwMode::None,
            Some(other) => {
                if first_time(format!("hwaccel-env\0{other}")) {
                    tracing::warn!(
                        "LCPLAY_HWACCEL={other} is not one of none, auto, card: \
                         decoding on each layer's card"
                    );
                }
                HwMode::Card
            }
        }
    }

    fn from_env() -> HwMode {
        HwMode::parse(std::env::var("LCPLAY_HWACCEL").ok().as_deref())
    }
}

/// Where a decoder runs, as decided once per video layer and card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwAccel {
    /// Software.
    None,
    /// `-hwaccel auto`: ffmpeg tries the decoder's hardware types in order
    /// and takes the first device it can open, on the default adapter.
    Auto,
    /// `-hwaccel d3d11va -hwaccel_device <adapter>`: D3D11VA on the DXGI
    /// adapter with this index (Windows).
    ///
    /// Never `-hwaccel auto -hwaccel_device N`: auto hands the same N to
    /// DXVA2 (a D3D9 ordinal), D3D11VA (a DXGI index) and CUDA (a CUDA
    /// ordinal), which number cards differently. Not `cuda` either: its
    /// ordinal follows `CUDA_DEVICE_ORDER` (fastest first by default), not
    /// DXGI's order.
    D3d11va {
        adapter: u32,
        /// How ffmpeg should describe the adapter it opens
        /// ("10de:2560 (NVIDIA GeForce …)"), to check that it opened the
        /// intended one.
        expect: String,
    },
}

impl HwAccel {
    /// Pure policy: `card` is the DXGI index and description of the layer's
    /// card when it is known and ffmpeg can decode on it with D3D11VA.
    pub fn choose(mode: HwMode, card: Option<(u32, String)>) -> HwAccel {
        match (mode, card) {
            (HwMode::None, _) => HwAccel::None,
            (HwMode::Auto, _) | (HwMode::Card, None) => HwAccel::Auto,
            (HwMode::Card, Some((adapter, expect))) => HwAccel::D3d11va { adapter, expect },
        }
    }

    /// Where to decode for the card with this adapter LUID (`None`: not a
    /// D3D12 card, or not Windows). Enumerates DXGI adapters: call once per
    /// layer, not per decoder start.
    pub fn for_card(luid: Option<i64>) -> HwAccel {
        let mode = HwMode::from_env();
        let card = match mode {
            HwMode::Card => luid.and_then(card_decoder),
            _ => None,
        };
        HwAccel::choose(mode, card)
    }

    /// For the log: where this layer's frames are decoded.
    pub fn describe(&self) -> String {
        match self {
            HwAccel::None => "software decoding (LCPLAY_HWACCEL=none)".into(),
            HwAccel::Auto => "decoder chosen by ffmpeg (-hwaccel auto)".into(),
            HwAccel::D3d11va { adapter, expect } => {
                format!("D3D11VA on DXGI adapter {adapter}, {expect}")
            }
        }
    }
}

/// The DXGI index and description of the card with this LUID, if ffmpeg can
/// decode on it there.
#[cfg(windows)]
fn card_decoder(luid: i64) -> Option<(u32, String)> {
    if !ffmpeg_has_d3d11va() {
        return None;
    }
    let found = crate::dxgi::dxgi_index_of_luid(luid);
    if found.is_none() {
        tracing::warn!(
            "video: no DXGI adapter has the card's LUID {luid:#x}; \
             decoding where ffmpeg picks"
        );
    }
    found
}

#[cfg(not(windows))]
fn card_decoder(_luid: i64) -> Option<(u32, String)> {
    None
}

/// Whether the ffmpeg in use lists `d3d11va` among its hardware decoders
/// (`ffmpeg -hwaccels`). Asked once. Without it, `-hwaccel d3d11va` would
/// stop ffmpeg with an error rather than fall back, so decoding is left to
/// ffmpeg's own pick, as before.
#[cfg(windows)]
fn ffmpeg_has_d3d11va() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        let listed = quiet_command(ffmpeg_bin())
            .args(["-hide_banner", "-hwaccels"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .is_some_and(|out| hwaccels_list_d3d11va(&String::from_utf8_lossy(&out.stdout)));
        if !listed {
            tracing::warn!(
                "video: this ffmpeg has no d3d11va hardware decoder (ffmpeg -hwaccels); \
                 decoders are not bound to their card"
            );
        }
        listed
    })
}

/// Whether the output of `ffmpeg -hwaccels` lists d3d11va.
#[cfg_attr(not(windows), allow(dead_code))]
fn hwaccels_list_d3d11va(listing: &str) -> bool {
    listing.lines().any(|l| l.trim() == "d3d11va")
}

/// The ffmpeg command line (without the program) that decodes `path` from
/// frame `start_frame`, at `width × height`, into the pipe in `layout`, on
/// `hw`. Pure: what the tests check.
#[allow(clippy::too_many_arguments)]
pub fn decoder_args(
    path: &str,
    info: &VideoInfo,
    layout: Layout,
    width: u32,
    height: u32,
    start_frame: i64,
    hw: &HwAccel,
) -> Vec<String> {
    let start_s = start_frame as f64 * info.fps_den as f64 / info.fps_num as f64;
    // `fps=` makes the output strictly constant-rate, so "the n-th frame
    // out of the pipe" is a well-defined point in time.
    let mut filter = format!("fps={}/{}", info.fps_num, info.fps_den);
    let scale = if (width, height) != (info.width, info.height) {
        format!("{width}:{height}:")
    } else {
        String::new()
    };
    let pix_fmt = match layout {
        // Renderer textures are premultiplied; files with alpha are
        // straight.
        Layout::Rgba => {
            if !scale.is_empty() {
                filter.push_str(&format!(",scale={}", scale.trim_end_matches(':')));
            }
            filter.push_str(",format=rgba,premultiply=inplace=1");
            "rgba"
        }
        Layout::Yuv420 => {
            // The shader expects limited ("TV") range. Many files are
            // full range (VP9, HEVC from phones, screen recordings); a
            // plain format conversion would pass their levels through
            // and crush blacks / clip whites. Convert explicitly.
            filter.push_str(&format!(",scale={scale}out_range=tv,format=yuv420p"));
            "yuv420p"
        }
    };
    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| s.to_string()));
    // Log at info, each line tagged with its level ("[info] …"), so the
    // Runner can pick out the device ffmpeg opens and pass errors on (see
    // `forward_log`). No banner, no progress lines. No auto-rotation:
    // ffprobe reports the stored size, and the frames must come out at
    // exactly that size.
    push(&["-hide_banner", "-nostats", "-loglevel", "level+info"]);
    push(&["-nostdin", "-noautorotate"]);
    match hw {
        HwAccel::None => {}
        // ffmpeg falls back to software by itself where there is no
        // hardware decoder for the codec.
        HwAccel::Auto => push(&["-hwaccel", "auto"]),
        // Frames are downloaded to system memory (no
        // `-hwaccel_output_format`), as with auto: the pipe needs them
        // there. A codec or profile the card cannot decode is decoded in
        // software, still with no error.
        HwAccel::D3d11va { adapter, .. } => {
            let adapter = adapter.to_string();
            push(&["-hwaccel", "d3d11va", "-hwaccel_device", &adapter]);
        }
    }
    // Before `-i`: seek in the input. ffmpeg then decodes from the
    // preceding keyframe and discards up to the exact position.
    let start = format!("{start_s:.6}");
    push(&["-ss", &start, "-i", path]);
    push(&["-an", "-sn", "-vf", &filter]);
    push(&["-f", "rawvideo", "-pix_fmt", pix_fmt, "pipe:1"]);
    args
}

/// One line of ffmpeg's log, sorted.
#[derive(Debug, PartialEq, Eq)]
enum LogLine {
    /// The hardware device ffmpeg opened, as it describes it: D3D11VA's
    /// "10de:2560 (NVIDIA …)", or auto's "hwaccel type d3d11va with new
    /// default device".
    Device(String),
    Error(String),
    Warning(String),
    /// Stream information and the like.
    Other,
}

/// Sort a line logged with `-loglevel level+info`:
/// `[h264 @ 0000020c3f0e1c40] [error] message`, or `[info] message`.
fn classify(line: &str) -> LogLine {
    let line = line.trim_end();
    let (level, message) = ["panic", "fatal", "error", "warning", "info", "verbose"]
        .iter()
        .find_map(|level| {
            let tag = format!("[{level}] ");
            let at = line.find(&tag)?;
            // The context ("[h264 @ 0x…] ") before the tag is kept, without
            // its address, which changes with every run.
            let context = strip_address(&line[..at]);
            Some((*level, format!("{context}{}", &line[at + tag.len()..])))
        })
        .unwrap_or(("info", line.to_string()));
    match level {
        "panic" | "fatal" | "error" => LogLine::Error(message),
        "warning" => LogLine::Warning(message),
        _ => {
            if let Some(device) = message.split("Using device ").nth(1) {
                LogLine::Device(device.trim_end_matches('.').to_string())
            } else if let Some(auto) = message.split("Using auto ").nth(1) {
                LogLine::Device(auto.trim_end_matches('.').to_string())
            } else {
                LogLine::Other
            }
        }
    }
}

/// "[h264 @ 0000020c3f0e1c40] " → "[h264] ".
fn strip_address(context: &str) -> String {
    let mut out = String::with_capacity(context.len());
    let mut rest = context;
    while let Some(at) = rest.find(" @ ") {
        out.push_str(&rest[..at]);
        rest = &rest[at + 3..];
        let end = rest
            .find(|c: char| !c.is_ascii_hexdigit() && c != 'x')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Report something once per process (a decoder restarts at every seek and
/// loop; its device and warnings would otherwise fill the log).
fn first_time(key: String) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(false)
}

/// Pass an ffmpeg child's log on to the Runner's: errors always, warnings
/// and the opened device once per file, and a warning when the device is
/// not the card asked for.
fn forward_log(stderr: impl Read, path: &str, hw: &HwAccel) {
    for line in std::io::BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        match classify(&line) {
            LogLine::Device(device) => {
                let key = format!("device\0{path}\0{device}");
                match hw {
                    HwAccel::D3d11va { adapter, expect } if &device != expect => {
                        if first_time(key) {
                            tracing::warn!(
                                "video '{path}': ffmpeg opened {device}, \
                                 but the layer's card is {expect} (DXGI adapter {adapter}): \
                                 ffmpeg enumerates the cards in another order \
                                 (a per-app GPU preference for ffmpeg.exe?)"
                            );
                        }
                    }
                    _ => {
                        if first_time(key) {
                            tracing::info!("video '{path}': ffmpeg decodes on {device}");
                        }
                    }
                }
            }
            LogLine::Error(message) => tracing::error!("video '{path}': ffmpeg: {message}"),
            LogLine::Warning(message) => {
                if first_time(format!("warning\0{path}\0{message}")) {
                    tracing::warn!("video '{path}': ffmpeg: {message}");
                }
            }
            LogLine::Other => {}
        }
    }
}

/// One running `ffmpeg`, delivering consecutive frames from `start_frame`.
pub struct Decoder {
    child: Child,
    frames: Receiver<Vec<u8>>,
    /// Hands used buffers back to the reader thread, so steady playback
    /// allocates nothing.
    recycle: SyncSender<Vec<u8>>,
    /// Index of the frame the next `try_next` will return.
    next_frame: i64,
}

impl Decoder {
    /// Start decoding `path` at frame `start_frame`, at `width × height`,
    /// on `hw`.
    pub fn start(
        path: &str,
        info: &VideoInfo,
        layout: Layout,
        width: u32,
        height: u32,
        start_frame: i64,
        hw: &HwAccel,
    ) -> anyhow::Result<Decoder> {
        let mut child = quiet_command(ffmpeg_bin())
            .args(decoder_args(
                path,
                info,
                layout,
                width,
                height,
                start_frame,
                hw,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg (is it installed and on the PATH?)")?;
        // Read the log to its end (a full pipe would stall ffmpeg); it ends
        // when ffmpeg does.
        let stderr = child.stderr.take().expect("stderr was piped");
        let (log_path, log_hw) = (path.to_string(), hw.clone());
        let logger = std::thread::Builder::new()
            .name("lc-video-log".into())
            .spawn(move || {
                crate::priority::background_thread();
                forward_log(stderr, &log_path, &log_hw)
            });
        if let Err(e) = logger {
            // No Decoder will exist to kill it: do it here.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).context("starting video log thread");
        }
        let mut stdout = child.stdout.take().expect("stdout was piped");

        let frame_len = layout.frame_len(width, height);
        let (tx, frames) = sync_channel::<Vec<u8>>(QUEUE_FRAMES);
        let (recycle, recycled) = sync_channel::<Vec<u8>>(QUEUE_FRAMES + 2);
        std::thread::Builder::new()
            .name("lc-video".into())
            .spawn(move || {
                crate::priority::background_thread();
                loop {
                    let mut buf = recycled.try_recv().unwrap_or_default();
                    buf.resize(frame_len, 0);
                    // `read_exact` fails at end of file or when ffmpeg is
                    // killed; `send` fails when the Decoder is dropped.
                    // Either way: done. A full queue blocks here, which
                    // stalls ffmpeg on its pipe — that is the back-pressure
                    // that keeps it from running ahead.
                    if stdout.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                        break;
                    }
                }
            })
            .context("starting video reader thread")?;

        Ok(Decoder {
            child,
            frames,
            recycle,
            next_frame: start_frame,
        })
    }

    /// The next frame if one is decoded, with its index. Never blocks.
    pub fn try_next(&mut self) -> Option<(i64, Vec<u8>)> {
        match self.frames.try_recv() {
            Ok(buf) => {
                self.next_frame += 1;
                Some((self.next_frame - 1, buf))
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Block until the next frame (tests, and nothing else).
    pub fn next_blocking(&mut self) -> Option<(i64, Vec<u8>)> {
        let buf = self.frames.recv().ok()?;
        self.next_frame += 1;
        Some((self.next_frame - 1, buf))
    }

    pub fn give_back(&self, buf: Vec<u8>) {
        let _ = self.recycle.try_send(buf);
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Kill and reap, so no ffmpeg outlives its layer.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A video layer played through ffmpeg.
pub struct VideoContent {
    path: String,
    info: VideoInfo,
    layout: Layout,
    /// Decoded size (the file's, scaled down if the GPU needs it).
    size: (u32, u32),
    /// One texture (RGBA) or three (Y, U, V).
    planes: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    sampling: Sampling,
    timing: Timing,
    /// Where this layer is decoded on this card (decided once, in `new`).
    hw: HwAccel,
    decoder: Option<Decoder>,
    /// A second decoder started shortly before a loop point, already
    /// positioned at the loop start, so the wrap does not wait for ffmpeg.
    standby: Option<Decoder>,
    /// Frame currently in the texture.
    shown: Option<i64>,
    /// A decoded frame taken from the decoder but held back: it is newer
    /// than the frame another card decided on (see [`Content::follow`]).
    /// Comes before anything still in the decoder.
    pending: Option<(i64, Vec<u8>)>,
    /// The frame last due (by the clock).
    last_want: Option<i64>,
    /// When a decoder was last started; limits how often that happens.
    last_start: Option<std::time::Instant>,
    failed: bool,
}

impl VideoContent {
    /// Set up textures for a probed file. Fast: decoding starts on the first
    /// `update`.
    pub fn new(path: &str, info: VideoInfo, timing: &Timing, gpu: &TextureFactory) -> Self {
        // Stay within the GPU's texture limit, keeping the aspect.
        let max = gpu.max_dimension as f64;
        let k = (max / info.width as f64)
            .min(max / info.height as f64)
            .min(1.0);
        let even = |v: f64| (((v as u32) / 2) * 2).max(2);
        let size = if k < 1.0 {
            (even(info.width as f64 * k), even(info.height as f64 * k))
        } else {
            (info.width, info.height)
        };
        let layout = if info.has_alpha {
            Layout::Rgba
        } else {
            Layout::Yuv420
        };
        let (w, h) = size;
        let (planes, sampling) = match layout {
            Layout::Rgba => (
                vec![gpu.texture(path, w, h, wgpu::TextureFormat::Rgba8UnormSrgb)],
                Sampling::default(),
            ),
            Layout::Yuv420 => {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let r8 = wgpu::TextureFormat::R8Unorm;
                (
                    vec![
                        gpu.texture(path, w, h, r8),
                        gpu.texture(path, cw, ch, r8),
                        gpu.texture(path, cw, ch, r8),
                    ],
                    Sampling {
                        mode: if info.bt601 {
                            Encoding::Yuv601
                        } else {
                            Encoding::Yuv709
                        },
                        uv_scale: [1.0, 1.0],
                        param: [0.0; 4],
                    },
                )
            }
        };
        let bind_group = gpu.bind(
            path,
            [
                &planes[0],
                &planes[planes.len().min(2) - 1],
                &planes[planes.len() - 1],
            ],
        );
        let hw = HwAccel::for_card(gpu.luid);
        tracing::info!(
            "video '{path}': {}x{} at {:.3} fps, {} — decoded by ffmpeg, {}",
            info.width,
            info.height,
            info.fps(),
            if info.has_alpha {
                "with alpha (RGBA)"
            } else {
                "YUV 4:2:0"
            },
            hw.describe()
        );
        VideoContent {
            path: path.to_string(),
            info,
            layout,
            size,
            planes,
            bind_group,
            sampling,
            timing: timing.clone(),
            hw,
            decoder: None,
            standby: None,
            shown: None,
            pending: None,
            last_want: None,
            last_start: None,
            failed: false,
        }
    }

    fn first_frame(&self) -> i64 {
        (self.timing.media_in.max(0.0) * self.info.fps()).floor() as i64
    }

    fn start_decoder(&mut self, frame: i64) -> Option<Decoder> {
        self.last_start = Some(std::time::Instant::now());
        let (w, h) = self.size;
        match Decoder::start(&self.path, &self.info, self.layout, w, h, frame, &self.hw) {
            Ok(d) => Some(d),
            Err(e) => {
                // Report once; retrying 60 times a second would not help.
                tracing::error!("video '{}': {e:#}", self.path);
                self.failed = true;
                None
            }
        }
    }

    /// Copy one decoded frame into the texture plane(s).
    fn upload(&self, frame: &[u8], queue: &wgpu::Queue) {
        let (w, h) = self.size;
        let write =
            |texture: &wgpu::Texture, data: &[u8], bytes_per_pixel: u32, pw: u32, ph: u32| {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pw * bytes_per_pixel),
                        rows_per_image: Some(ph),
                    },
                    wgpu::Extent3d {
                        width: pw,
                        height: ph,
                        depth_or_array_layers: 1,
                    },
                );
            };
        match self.layout {
            Layout::Rgba => write(&self.planes[0], frame, 4, w, h),
            Layout::Yuv420 => {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let (y_len, c_len) = ((w * h) as usize, (cw * ch) as usize);
                write(&self.planes[0], &frame[..y_len], 1, w, h);
                write(&self.planes[1], &frame[y_len..y_len + c_len], 1, cw, ch);
                write(
                    &self.planes[2],
                    &frame[y_len + c_len..y_len + 2 * c_len],
                    1,
                    cw,
                    ch,
                );
            }
        }
    }
}

impl VideoContent {
    /// Bring the texture to frame `target`. `due` is the frame the clock
    /// asks for (what lateness is measured against). `exact`: `target` was
    /// decided by another card ([`Content::follow`]) — never show a frame
    /// newer than it, and do not restart the decoder to go back a little.
    fn advance(&mut self, due: i64, target: i64, exact: bool, gpu: &TextureFactory) {
        if self.failed {
            return;
        }
        self.last_want = Some(due);
        if self.shown == Some(target) {
            return;
        }
        let fps = self.info.fps();

        // Is the running decoder still useful? It only moves forward, so it
        // is not if the wanted frame is behind it, or so far ahead that
        // decoding up to it would take longer than starting over. A frame
        // held back counts as not yet taken.
        let reach = |d: &Decoder, pending: &Option<(i64, Vec<u8>)>| {
            pending.as_ref().map_or(d.next_frame, |(index, _)| *index)
        };
        let window = (2.0 * fps) as i64;
        let usable = self.decoder.as_ref().is_some_and(|d| {
            let first = reach(d, &self.pending);
            target < d.next_frame + window
                && (target >= first || (exact && target >= first - window))
        });
        if !usable {
            // The common backwards jump is a loop wrapping; the standby
            // decoder was started for exactly that.
            let standby_fits = self
                .standby
                .as_ref()
                .is_some_and(|d| target >= d.next_frame && target < d.next_frame + fps as i64);
            // Starting ffmpeg costs on the order of 100 ms. If the wanted
            // frame keeps landing outside the decoder's reach — reverse
            // playback, where every frame is "behind", or fast scrubbing —
            // restarting on every frame would spawn dozens of processes a
            // second and show nothing. Restarts are therefore rate-limited
            // and the picture is held in between: reverse video is choppy
            // (a few frames per second), not smooth.
            let too_soon = self
                .last_start
                .is_some_and(|t| t.elapsed() < MIN_RESTART_INTERVAL);
            if standby_fits {
                self.decoder = self.standby.take();
                self.pending = None;
            } else if self.decoder.is_some() && too_soon {
                return;
            } else {
                self.standby = None;
                self.pending = None;
                self.decoder = self.start_decoder(target);
            }
        }

        // Approaching the loop point: get the next pass ready.
        if self.timing.media_loop && self.standby.is_none() {
            if let Some(total) = self.info.frames {
                if total - target <= fps as i64 && total - self.first_frame() > (2.0 * fps) as i64 {
                    self.standby = self.start_decoder(self.first_frame());
                }
            }
        }

        // Advance to the wanted frame, skipping the ones the clock has
        // passed. If it is not decoded yet, try again next frame — the render
        // loop never waits for a decoder.
        //
        // If the decoder has *only* late frames to offer (it was just
        // started, or it cannot keep up), the newest of them is shown rather
        // than nothing: a picture that is slightly behind beats a frozen or
        // black one, and a decoder slower than real time would otherwise
        // never show anything at all.
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let mut chosen: Option<(i64, Vec<u8>)> = None;
        let mut next = self.pending.take();
        while let Some((index, frame)) = next.take().or_else(|| decoder.try_next()) {
            if exact && index > target {
                // Newer than what the other card shows: keep it for later.
                self.pending = Some((index, frame));
                break;
            }
            if let Some((_, older)) = chosen.replace((index, frame)) {
                decoder.give_back(older);
            }
            if index >= target {
                break;
            }
        }
        if let Some((index, frame)) = chosen {
            self.upload(&frame, gpu.queue);
            self.shown = Some(index);
            if let Some(decoder) = self.decoder.as_ref() {
                decoder.give_back(frame);
            }
        }
    }

    fn due(&self, t: f64) -> i64 {
        media_frame(&self.timing, self.info.fps(), self.info.frames, t)
    }
}

impl Content for VideoContent {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        let due = self.due(t);
        self.advance(due, due, false, gpu);
    }

    fn follow(&mut self, t: f64, frame: i64, gpu: &TextureFactory) {
        let due = self.due(t);
        self.advance(due, frame, true, gpu);
    }

    fn shown_frame(&self) -> Option<i64> {
        self.shown
    }

    fn idle(&mut self) {
        // Dropping a decoder kills its ffmpeg. `shown` is kept: the texture
        // still holds that frame, and the next `update` restarts from there.
        self.decoder = None;
        self.standby = None;
        self.pending = None;
    }

    fn retime(&mut self, timing: &Timing) {
        // The next `update` computes a new wanted frame from this and jumps
        // if necessary.
        self.timing = timing.clone();
    }

    fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    fn sampling(&self) -> Sampling {
        self.sampling
    }

    fn ready(&self) -> bool {
        self.shown.is_some()
    }

    fn late(&self) -> bool {
        self.shown.is_some() && self.last_want.is_some() && self.shown != self.last_want
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A 2 s, 10 fps, 64×48 test clip whose frames differ measurably: a black
    /// field with a white box that moves 4 px per frame.
    fn make_clip(dir: &Path) -> Option<String> {
        let path = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg_bin())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(r"color=black:s=64x48:r=10:d=2,format=yuv420p,geq=lum='if(between(X\,4*N\,4*N+3)\,255\,0)':cb=128:cr=128")
            .args(["-pix_fmt", "yuv420p", "-g", "5"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then(|| path.to_string_lossy().into_owned())
    }

    /// x position of the white box in a YUV frame: the first bright pixel
    /// of row 24 of the Y plane.
    fn box_x(frame: &[u8]) -> Option<usize> {
        frame[24 * 64..25 * 64].iter().position(|&y| y > 128)
    }

    #[test]
    fn decoder_delivers_the_exact_frames_asked_for() {
        let dir = std::env::temp_dir().join(format!("lcplay-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(path) = make_clip(&dir) else {
            eprintln!("SKIPPED: ffmpeg not available");
            return;
        };

        let info = VideoInfo::probe(&path).unwrap();
        assert_eq!((info.width, info.height), (64, 48));
        assert_eq!((info.fps_num, info.fps_den), (10, 1));
        assert_eq!(info.frames, Some(20));
        assert!(!info.has_alpha);
        assert!(info.bt601, "untagged and below 720 lines: BT.601");

        // From the start: frames 0, 1, 2 with the box at 0, 4, 8.
        let mut d =
            Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 0, &HwAccel::None).unwrap();
        for expect in 0..3 {
            let (index, frame) = d.next_blocking().unwrap();
            assert_eq!(index, expect);
            assert_eq!(frame.len(), 64 * 48 + 2 * 32 * 24, "Y + U + V");
            assert_eq!(box_x(&frame), Some(4 * expect as usize), "frame {index}");
        }

        // Seek to frame 7 — not a keyframe (they are every 5): still exact.
        let mut d =
            Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 7, &HwAccel::None).unwrap();
        let (index, frame) = d.next_blocking().unwrap();
        assert_eq!((index, box_x(&frame)), (7, Some(28)));
        let (index, frame) = d.next_blocking().unwrap();
        assert_eq!((index, box_x(&frame)), (8, Some(32)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_range_video_arrives_in_limited_range() {
        // A full-range ("PC") white frame must reach the shader as limited-
        // range white (Y = 235), which is what the shader assumes.
        let dir = std::env::temp_dir().join(format!("lcplay-range-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("full.webm");
        let ok = Command::new(ffmpeg_bin())
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=white:s=64x48:r=10:d=1",
            ])
            .args([
                "-vf",
                "scale=out_range=pc,format=yuv420p",
                "-color_range",
                "pc",
            ])
            .args(["-c:v", "libvpx-vp9"])
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("SKIPPED: ffmpeg with libvpx not available");
            return;
        }
        let path = path.to_string_lossy();
        let info = VideoInfo::probe(&path).unwrap();
        let mut d =
            Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 0, &HwAccel::None).unwrap();
        let (_, frame) = d.next_blocking().unwrap();
        assert!((233..=237).contains(&frame[0]), "Y of white = {}", frame[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Call `step` until `done` says so (decoding runs on other threads),
    /// for at most five seconds.
    fn until(mut step: impl FnMut(), done: impl Fn() -> bool) -> bool {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_secs(5) {
            step();
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_follower_shows_the_decided_frame_not_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("lcplay-follow-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(path) = make_clip(&dir) else {
            eprintln!("SKIPPED: ffmpeg not available");
            return;
        };
        let Some(gpu) = crate::render::Gpu::headless() else {
            eprintln!("skipping GPU test: no adapter (install mesa-vulkan-drivers)");
            return;
        };
        let info = VideoInfo::probe(&path).unwrap();
        let timing = Timing {
            start: 0.0,
            ..Default::default()
        };
        let factory = gpu.factory();
        let mut video = VideoContent::new(&path, info, &timing, &factory);

        // Another card decided on frame 3 while the clock says 5: frame 3 it
        // is, and the decoder's newer frames are held back.
        let shown = std::cell::Cell::new(None);
        let mut follow = |t: f64, frame: i64| {
            video.follow(t, frame, &factory);
            shown.set(video.shown_frame());
            assert!(
                shown.get().is_none_or(|s| s <= frame),
                "showed {shown:?} past {frame}"
            );
        };
        assert!(until(|| follow(0.55, 3), || shown.get() == Some(3)));
        // Some more refreshes on the same decision: it stays.
        for _ in 0..20 {
            follow(0.55, 3);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(shown.get(), Some(3));
        // The next decision: the frame held back is used, no restart.
        assert!(until(|| follow(0.65, 4), || shown.get() == Some(4)));
        // Deciding again by itself, it catches up with the clock.
        let video = std::cell::RefCell::new(video);
        assert!(until(
            || video.borrow_mut().update(0.75, &factory),
            || video.borrow().shown_frame() == Some(7)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn info_64x48() -> VideoInfo {
        VideoInfo {
            width: 64,
            height: 48,
            fps_num: 30000,
            fps_den: 1001,
            frames: Some(300),
            has_alpha: false,
            bt601: true,
        }
    }

    /// The arguments that follow `flag` (its value), if `flag` is there.
    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    #[test]
    fn decoder_args_bind_d3d11va_to_the_cards_adapter() {
        let info = info_64x48();
        let hw = HwAccel::D3d11va {
            adapter: 1,
            expect: "10de:2560 (NVIDIA RTX A4000)".into(),
        };
        let args = decoder_args("clip.mp4", &info, Layout::Yuv420, 64, 48, 0, &hw);
        assert_eq!(value_of(&args, "-hwaccel"), Some("d3d11va"));
        assert_eq!(value_of(&args, "-hwaccel_device"), Some("1"));
        // Never auto with a device, never CUDA.
        assert!(!args.iter().any(|a| a == "auto" || a == "cuda"));
        // The hardware options are input options: before `-i`.
        let input = args.iter().position(|a| a == "-i").unwrap();
        let hwaccel = args.iter().position(|a| a == "-hwaccel").unwrap();
        assert!(hwaccel < input);
        assert_eq!(args[input + 1], "clip.mp4");
        // Tagged levels at info, so the device line can be read.
        assert_eq!(value_of(&args, "-loglevel"), Some("level+info"));
        assert!(args.iter().any(|a| a == "-nostats"));
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
    }

    #[test]
    fn decoder_args_auto_and_none() {
        let info = info_64x48();
        let auto = decoder_args("a.mov", &info, Layout::Yuv420, 64, 48, 0, &HwAccel::Auto);
        assert_eq!(value_of(&auto, "-hwaccel"), Some("auto"));
        assert_eq!(value_of(&auto, "-hwaccel_device"), None);
        let none = decoder_args("a.mov", &info, Layout::Yuv420, 64, 48, 0, &HwAccel::None);
        assert_eq!(value_of(&none, "-hwaccel"), None);
        assert_eq!(value_of(&none, "-hwaccel_device"), None);
    }

    #[test]
    fn decoder_args_seek_scale_and_layout() {
        let info = info_64x48();
        // Frame 30 at 30000/1001 fps is 1.001 s.
        let yuv = decoder_args("a.mov", &info, Layout::Yuv420, 64, 48, 30, &HwAccel::None);
        assert_eq!(value_of(&yuv, "-ss"), Some("1.001000"));
        assert_eq!(
            value_of(&yuv, "-vf"),
            Some("fps=30000/1001,scale=out_range=tv,format=yuv420p")
        );
        assert_eq!(value_of(&yuv, "-pix_fmt"), Some("yuv420p"));
        // Scaled down, with alpha.
        let rgba = decoder_args("a.mov", &info, Layout::Rgba, 32, 24, 0, &HwAccel::None);
        assert_eq!(
            value_of(&rgba, "-vf"),
            Some("fps=30000/1001,scale=32:24,format=rgba,premultiply=inplace=1")
        );
        assert_eq!(value_of(&rgba, "-pix_fmt"), Some("rgba"));
        let scaled = decoder_args("a.mov", &info, Layout::Yuv420, 32, 24, 0, &HwAccel::None);
        assert_eq!(
            value_of(&scaled, "-vf"),
            Some("fps=30000/1001,scale=32:24:out_range=tv,format=yuv420p")
        );
    }

    #[test]
    fn hwaccel_env_and_policy() {
        assert_eq!(HwMode::parse(None), HwMode::Card);
        assert_eq!(HwMode::parse(Some("")), HwMode::Card);
        assert_eq!(HwMode::parse(Some("none")), HwMode::None);
        assert_eq!(HwMode::parse(Some("AUTO")), HwMode::Auto);
        assert_eq!(HwMode::parse(Some(" card ")), HwMode::Card);
        assert_eq!(
            HwMode::parse(Some("cuda")),
            HwMode::Card,
            "unknown: default"
        );

        let card = || Some((2, "10de:24b0 (NVIDIA RTX A4000)".to_string()));
        assert_eq!(
            HwAccel::choose(HwMode::Card, card()),
            HwAccel::D3d11va {
                adapter: 2,
                expect: "10de:24b0 (NVIDIA RTX A4000)".into()
            }
        );
        // Card unknown (not D3D12, not Windows, no d3d11va in ffmpeg): as before.
        assert_eq!(HwAccel::choose(HwMode::Card, None), HwAccel::Auto);
        assert_eq!(HwAccel::choose(HwMode::Auto, card()), HwAccel::Auto);
        assert_eq!(HwAccel::choose(HwMode::None, card()), HwAccel::None);
    }

    #[test]
    fn ffmpeg_log_lines_are_sorted() {
        // libavutil/hwcontext_d3d11va.c: "Using device %04x:%04x (%ls).\n".
        assert_eq!(
            classify("[AVHWDeviceContext @ 000001d2c5e0a940] [info] Using device 10de:2560 (NVIDIA GeForce RTX 3050 Ti Laptop GPU).\r"),
            LogLine::Device("10de:2560 (NVIDIA GeForce RTX 3050 Ti Laptop GPU)".into())
        );
        // fftools/ffmpeg_dec.c, with -hwaccel auto.
        assert_eq!(
            classify("[vist#0:0/h264 @ 0x5581e0c0] [info] Using auto hwaccel type vaapi with new default device."),
            LogLine::Device("hwaccel type vaapi with new default device".into())
        );
        assert_eq!(
            classify("[h264 @ 0x55d1c2a3b4c0] [error] Invalid NAL unit size."),
            LogLine::Error("[h264] Invalid NAL unit size.".into())
        );
        assert_eq!(
            classify("[fatal] No such file or directory"),
            LogLine::Error("No such file or directory".into())
        );
        assert_eq!(
            classify("[hevc @ 000001d2c5e0a940] [warning] Failed setup for format d3d11: hwaccel initialisation returned error."),
            LogLine::Warning("[hevc] Failed setup for format d3d11: hwaccel initialisation returned error.".into())
        );
        assert_eq!(
            classify("[info]   Stream #0:0: Video: h264"),
            LogLine::Other
        );
        assert_eq!(classify("untagged"), LogLine::Other);
    }

    #[test]
    fn hwaccels_listing() {
        let listing = "Hardware acceleration methods:\ncuda\ndxva2\nqsv\nd3d11va\nd3d12va\n\n";
        assert!(hwaccels_list_d3d11va(listing));
        assert!(!hwaccels_list_d3d11va(
            "Hardware acceleration methods:\nvdpau\nvaapi\n"
        ));
    }

    #[test]
    fn media_frame_handles_loops_holds_and_free_run() {
        let timing = Timing {
            start: 10.0,
            end: Some(30.0),
            ..Default::default()
        };
        assert_eq!(media_frame(&timing, 10.0, Some(50), 12.0), 20);
        assert_eq!(
            media_frame(&timing, 10.0, Some(50), 25.0),
            49,
            "holds the last frame"
        );
        let looping = Timing {
            media_loop: true,
            ..timing.clone()
        };
        assert_eq!(media_frame(&looping, 10.0, Some(50), 16.0), 10);
        // Free-running: `t` is seconds since going on air, whatever the
        // cue's start; held at the end unless looping.
        let free = Timing {
            free_run: true,
            ..timing.clone()
        };
        assert_eq!(media_frame(&free, 10.0, Some(50), 2.0), 20);
        assert_eq!(media_frame(&free, 10.0, Some(50), 100.0), 49);
        let free_loop = Timing {
            free_run: true,
            media_loop: true,
            ..timing
        };
        assert_eq!(media_frame(&free_loop, 10.0, Some(50), 6.0), 10);
    }
}
