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
//! * **Hardware decoding.** ffmpeg is started with `-hwaccel auto`: H.264,
//!   HEVC, VP9, AV1 … are decoded by the GPU's video engine (NVDEC, Quick
//!   Sync, D3D11VA, VideoToolbox) where the machine has one, and in software
//!   otherwise. `LCPLAY_HWACCEL=none` turns it off.
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

use std::io::Read;
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
/// a windowed program; every ffmpeg it starts would otherwise flash one).
fn quiet_command(program: String) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
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
    /// Start decoding `path` at frame `start_frame`, at `width × height`.
    pub fn start(
        path: &str,
        info: &VideoInfo,
        layout: Layout,
        width: u32,
        height: u32,
        start_frame: i64,
    ) -> anyhow::Result<Decoder> {
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
        let mut command = quiet_command(ffmpeg_bin());
        // No auto-rotation: ffprobe reports the stored size, and the frames
        // must come out at exactly that size.
        command.args(["-v", "error", "-nostdin", "-noautorotate"]);
        if std::env::var("LCPLAY_HWACCEL").map_or(true, |v| v != "none") {
            // Decode on the GPU's video engine where there is one; ffmpeg
            // falls back to software by itself when there is not.
            command.args(["-hwaccel", "auto"]);
        }
        let mut child = command
            // Before `-i`: seek in the input. ffmpeg then decodes from the
            // preceding keyframe and discards up to the exact position.
            .args(["-ss", &format!("{start_s:.6}"), "-i", path])
            .args(["-an", "-sn", "-vf", &filter])
            .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting ffmpeg (is it installed and on the PATH?)")?;
        let mut stdout = child.stdout.take().expect("stdout was piped");

        let frame_len = layout.frame_len(width, height);
        let (tx, frames) = sync_channel::<Vec<u8>>(QUEUE_FRAMES);
        let (recycle, recycled) = sync_channel::<Vec<u8>>(QUEUE_FRAMES + 2);
        std::thread::Builder::new()
            .name("lc-video".into())
            .spawn(move || loop {
                let mut buf = recycled.try_recv().unwrap_or_default();
                buf.resize(frame_len, 0);
                // `read_exact` fails at end of file or when ffmpeg is killed;
                // `send` fails when the Decoder is dropped. Either way: done.
                // A full queue blocks here, which stalls ffmpeg on its pipe —
                // that is the back-pressure that keeps it from running ahead.
                if stdout.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                    break;
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
    decoder: Option<Decoder>,
    /// A second decoder started shortly before a loop point, already
    /// positioned at the loop start, so the wrap does not wait for ffmpeg.
    standby: Option<Decoder>,
    /// Frame currently in the texture.
    shown: Option<i64>,
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
        tracing::info!(
            "video '{path}': {}x{} at {:.3} fps, {} — decoded by ffmpeg",
            info.width,
            info.height,
            info.fps(),
            if info.has_alpha {
                "with alpha (RGBA)"
            } else {
                "YUV 4:2:0"
            }
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
            decoder: None,
            standby: None,
            shown: None,
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
        match Decoder::start(&self.path, &self.info, self.layout, w, h, frame) {
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

impl Content for VideoContent {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        if self.failed {
            return;
        }
        let want = media_frame(&self.timing, self.info.fps(), self.info.frames, t);
        if self.shown == Some(want) {
            return;
        }
        let fps = self.info.fps();

        // Is the running decoder still useful? It only moves forward, so it
        // is not if the wanted frame is behind it, or so far ahead that
        // decoding up to it would take longer than starting over.
        let usable = self
            .decoder
            .as_ref()
            .is_some_and(|d| want >= d.next_frame && want < d.next_frame + (2.0 * fps) as i64);
        if !usable {
            // The common backwards jump is a loop wrapping; the standby
            // decoder was started for exactly that.
            let standby_fits = self
                .standby
                .as_ref()
                .is_some_and(|d| want >= d.next_frame && want < d.next_frame + fps as i64);
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
            } else if self.decoder.is_some() && too_soon {
                return;
            } else {
                self.standby = None;
                self.decoder = self.start_decoder(want);
            }
        }

        // Approaching the loop point: get the next pass ready.
        if self.timing.media_loop && self.standby.is_none() {
            if let Some(total) = self.info.frames {
                if total - want <= fps as i64 && total - self.first_frame() > (2.0 * fps) as i64 {
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
        while let Some((index, frame)) = decoder.try_next() {
            if let Some((_, older)) = chosen.replace((index, frame)) {
                decoder.give_back(older);
            }
            if index >= want {
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

    fn idle(&mut self) {
        // Dropping a decoder kills its ffmpeg. `shown` is kept: the texture
        // still holds that frame, and the next `update` restarts from there.
        self.decoder = None;
        self.standby = None;
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
        let mut d = Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 0).unwrap();
        for expect in 0..3 {
            let (index, frame) = d.next_blocking().unwrap();
            assert_eq!(index, expect);
            assert_eq!(frame.len(), 64 * 48 + 2 * 32 * 24, "Y + U + V");
            assert_eq!(box_x(&frame), Some(4 * expect as usize), "frame {index}");
        }

        // Seek to frame 7 — not a keyframe (they are every 5): still exact.
        let mut d = Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 7).unwrap();
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
        let mut d = Decoder::start(&path, &info, Layout::Yuv420, 64, 48, 0).unwrap();
        let (_, frame) = d.next_blocking().unwrap();
        assert!((233..=237).contains(&frame[0]), "Y of white = {}", frame[0]);
        let _ = std::fs::remove_dir_all(&dir);
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
