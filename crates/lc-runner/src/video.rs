//! Video playback: an `ffmpeg` child process decodes, this module keeps its
//! output in step with the show clock.
//!
//! ## Why a child process
//!
//! `ffmpeg` on the `PATH` is the one decoder that exists on every platform,
//! reads every format an operator will throw at it, and needs no native
//! libraries at build time. It writes raw RGBA frames to a pipe; we read
//! them. The cost is a memory copy per frame (about 8 MB at 1080p), which is
//! fine for a first working system and the wrong answer for a 4K60 wall — a
//! GPU-decoded codec (HAP, or hardware H.264/HEVC) replaces [`Decoder`]
//! without touching anything else. *This is a correctness-first
//! implementation, not a performance one.*
//!
//! ## How it stays in sync
//!
//! Nothing here plays "in real time". Every rendered frame, the renderer
//! says what show time it is; that maps to an exact frame number in the file
//! (`frame = floor(media_time × fps)`), and the decoder's sequential output
//! is advanced until that frame comes out. Frames the show clock has already
//! passed are dropped; if the wanted frame is not decoded yet, the previous
//! one stays up. Two Runners given the same show time therefore show the
//! same frame of the same file, however differently their decoders are
//! scheduled.
//!
//! A jump (seek, loop, scrubbing) restarts `ffmpeg` at the new position.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};

use anyhow::Context as _;

use crate::content::{Content, StaticContent, TextureFactory};
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
}

impl VideoInfo {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den as f64
    }

    /// Ask `ffprobe` about the first video stream of `path`.
    pub fn probe(path: &str) -> anyhow::Result<VideoInfo> {
        let out = Command::new(ffprobe_bin())
            .args(["-v", "error", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "stream=width,height,r_frame_rate,pix_fmt:format=duration",
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
        Ok(VideoInfo {
            width: dim("width")?,
            height: dim("height")?,
            fps_num,
            fps_den,
            frames: duration
                .map(|d| (d * fps_num as f64 / fps_den as f64).round() as i64)
                .filter(|&f| f > 0),
            // yuva420p, rgba, bgra, argb, gbrap, ya8 …
            has_alpha: pix_fmt.contains('a') && !pix_fmt.starts_with("gray"),
        })
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
    /// Start decoding `path` at frame `start_frame`, scaled to
    /// `width × height` (the texture size).
    pub fn start(
        path: &str,
        info: &VideoInfo,
        width: u32,
        height: u32,
        start_frame: i64,
    ) -> anyhow::Result<Decoder> {
        let start_s = start_frame as f64 * info.fps_den as f64 / info.fps_num as f64;
        // `fps=` makes the output strictly constant-rate, so "the n-th frame
        // out of the pipe" is a well-defined point in time. Renderer textures
        // are premultiplied; files with alpha are straight.
        let mut filter = format!(
            "fps={}/{},scale={width}:{height}",
            info.fps_num, info.fps_den
        );
        if info.has_alpha {
            filter.push_str(",format=rgba,premultiply=inplace=1");
        }
        let mut child = Command::new(ffmpeg_bin())
            // No auto-rotation: ffprobe reports the stored size, and the
            // frames must come out at exactly that size (phone footage with
            // a rotation flag would otherwise be squashed).
            .args(["-v", "error", "-nostdin", "-noautorotate"])
            // Before `-i`: seek in the input. ffmpeg then decodes from the
            // preceding keyframe and discards up to the exact position.
            .args(["-ss", &format!("{start_s:.6}"), "-i", path])
            .args(["-an", "-sn", "-vf", &filter])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting ffmpeg (is it installed and on the PATH?)")?;
        let mut stdout = child.stdout.take().expect("stdout was piped");

        let frame_len = width as usize * height as usize * 4;
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

/// A video layer's content: a texture that follows the show clock.
pub struct VideoContent {
    path: String,
    info: VideoInfo,
    /// Texture size (the file's size, scaled down if the GPU needs it).
    size: (u32, u32),
    target: StaticContent,
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
    pub fn open(path: &str, timing: &Timing, factory: &TextureFactory) -> anyhow::Result<Self> {
        if !Path::new(path).is_file() {
            anyhow::bail!("video file '{path}' not found");
        }
        let info = VideoInfo::probe(path)?;
        // Stay within the GPU's texture limit, keeping the aspect.
        let max = factory.max_dimension as f64;
        let k = (max / info.width as f64)
            .min(max / info.height as f64)
            .min(1.0);
        let size = (
            ((info.width as f64 * k) as u32).max(1),
            ((info.height as f64 * k) as u32).max(1),
        );
        // Black until the first frame arrives.
        let black = vec![0u8; size.0 as usize * size.1 as usize * 4];
        let target = factory.upload_rgba8(path, size.0, size.1, &black);
        tracing::info!(
            "video '{path}': {}x{} at {:.3} fps{}",
            info.width,
            info.height,
            info.fps(),
            if info.has_alpha { ", with alpha" } else { "" }
        );
        Ok(VideoContent {
            path: path.to_string(),
            info,
            size,
            target,
            timing: timing.clone(),
            decoder: None,
            standby: None,
            shown: None,
            last_start: None,
            failed: false,
        })
    }

    /// First frame of the cue (its in-point), as a frame index.
    fn first_frame(&self) -> i64 {
        (self.timing.media_in.max(0.0) * self.info.fps()).floor() as i64
    }

    /// The frame of the file that belongs on screen at show time `t`.
    fn frame_at(&self, t: f64) -> i64 {
        let first = self.first_frame();
        let elapsed = ((t - self.timing.start).max(0.0) * self.info.fps()).floor() as i64;
        match self.info.frames {
            Some(total) if total > first => {
                if self.timing.media_loop {
                    first + elapsed % (total - first)
                } else {
                    // Past the end: hold the last frame.
                    (first + elapsed).min(total - 1)
                }
            }
            _ => first + elapsed,
        }
    }

    fn start_decoder(&mut self, frame: i64) -> Option<Decoder> {
        self.last_start = Some(std::time::Instant::now());
        match Decoder::start(&self.path, &self.info, self.size.0, self.size.1, frame) {
            Ok(d) => Some(d),
            Err(e) => {
                // Report once; retrying 60 times a second would not help.
                tracing::error!("video '{}': {e:#}", self.path);
                self.failed = true;
                None
            }
        }
    }
}

impl Content for VideoContent {
    fn update(&mut self, t: f64, queue: &wgpu::Queue) {
        if self.failed {
            return;
        }
        let want = self.frame_at(t);
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
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.target.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.size.0 * 4),
                    rows_per_image: Some(self.size.1),
                },
                wgpu::Extent3d {
                    width: self.size.0,
                    height: self.size.1,
                    depth_or_array_layers: 1,
                },
            );
            self.shown = Some(index);
            decoder.give_back(frame);
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
        &self.target.bind_group
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2 s, 10 fps, 64×48 test clip whose frames differ measurably: a black
    /// field with a white box that moves 4 px per frame.
    fn make_clip(dir: &Path) -> Option<String> {
        let path = dir.join("clip.mp4");
        let ok = Command::new(ffmpeg_bin())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=black:s=64x48:r=10:d=2,drawbox=x=4*n:y=0:w=4:h=48:color=white:t=fill")
            .args(["-pix_fmt", "yuv420p", "-g", "5"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then(|| path.to_string_lossy().into_owned())
    }

    /// x position of the white box in a frame (None if there is none).
    fn box_x(frame: &[u8]) -> Option<usize> {
        // Row 24, RGBA: find the first bright pixel.
        let row = &frame[24 * 64 * 4..25 * 64 * 4];
        row.chunks(4).position(|px| px[0] > 128)
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

        // From the start: frames 0, 1, 2 with the box at 0, 4, 8.
        let mut d = Decoder::start(&path, &info, 64, 48, 0).unwrap();
        for expect in 0..3 {
            let (index, frame) = d.next_blocking().unwrap();
            assert_eq!(index, expect);
            assert_eq!(frame.len(), 64 * 48 * 4);
            assert_eq!(box_x(&frame), Some(4 * expect as usize), "frame {index}");
        }

        // Seek to frame 7 — not a keyframe (they are every 5): still exact.
        let mut d = Decoder::start(&path, &info, 64, 48, 7).unwrap();
        let (index, frame) = d.next_blocking().unwrap();
        assert_eq!((index, box_x(&frame)), (7, Some(28)));
        let (index, frame) = d.next_blocking().unwrap();
        assert_eq!((index, box_x(&frame)), (8, Some(32)));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
