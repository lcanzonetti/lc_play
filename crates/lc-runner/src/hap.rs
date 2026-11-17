//! HAP video, decoded by the GPU.
//!
//! HAP (Vidvox) stores every frame as a GPU texture in a block-compressed
//! format (DXT1/BC1, DXT5/BC3, BC7), usually wrapped in Snappy compression.
//! Playing it therefore needs almost no CPU: read the frame from disk, undo
//! Snappy (a fast byte-oriented decompressor), hand the blocks to the GPU,
//! which samples them natively. This is why media servers use HAP for very
//! large canvases — and why decoding it to RGB pixels first, as a generic
//! decoder does, is the slow way (that is what made a 10K HAP play at 0 fps).
//!
//! Per frame of a 9978×2176 HAP (DXT1), roughly: 11 MB read, 11 MB unpacked,
//! 11 MB uploaded. The same frame as RGBA would be 87 MB.
//!
//! ## Pipeline
//!
//! ```text
//!   renderer: "frame N, moving forward"
//!       │
//!   workers (threads): read frames N, N+1, N+2 … from disk, unpack Snappy
//!       │  (chunks of one frame in parallel)
//!   ready frames ──▶ renderer uploads frame N to the texture ──▶ GPU samples
//! ```
//!
//! Every frame is addressed directly in the file ([`crate::mov`]), so seeks,
//! loops and reverse playback are as cheap as playing forward.
//!
//! Supported: Hap (`Hap1`), Hap Alpha (`Hap5`), Hap Q (`HapY`), Hap R
//! (`Hap7`), in QuickTime/MP4 files. Hap Q Alpha (`HapM`) and Hap Alpha-Only
//! (`HapA`), and HAP in other containers, are played through ffmpeg.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context as _};

use crate::content::{
    placeholder_static, Content, Encoding, Sampling, StaticContent, TextureFactory,
};
use crate::mov::{read_video_track, VideoTrack};
use crate::show::Timing;
use crate::video::{media_frame, VideoContent, VideoInfo};

/// Frames decoded ahead of the one on screen.
const AHEAD: i64 = 4;

/// Largest playback stride (frames per rendered frame) prefetched for: at
/// 16× speed on a 60 fps clip and 60 fps output.
const MAX_STRIDE: i64 = 16;

/// The texture layout of a HAP variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapFormat {
    /// `Hap1`: RGB DXT1.
    Dxt1,
    /// `Hap5`: RGBA DXT5.
    Dxt5,
    /// `HapY`: scaled YCoCg in DXT5.
    YCoCgDxt5,
    /// `Hap7`: RGBA BC7.
    Bc7,
}

impl HapFormat {
    fn from_codec(codec: &[u8; 4]) -> Option<HapFormat> {
        match codec {
            b"Hap1" => Some(HapFormat::Dxt1),
            b"Hap5" => Some(HapFormat::Dxt5),
            b"HapY" => Some(HapFormat::YCoCgDxt5),
            b"Hap7" => Some(HapFormat::Bc7),
            _ => None,
        }
    }

    /// The texture-format nibble a frame of this variant carries.
    fn section_type(self) -> u8 {
        match self {
            HapFormat::Dxt1 => 0x0B,
            HapFormat::Dxt5 => 0x0E,
            HapFormat::YCoCgDxt5 => 0x0F,
            HapFormat::Bc7 => 0x0C,
        }
    }

    /// Bytes per 4×4 block.
    fn block_bytes(self) -> usize {
        match self {
            HapFormat::Dxt1 => 8,
            _ => 16,
        }
    }

    fn texture_format(self) -> wgpu::TextureFormat {
        match self {
            HapFormat::Dxt1 => wgpu::TextureFormat::Bc1RgbaUnormSrgb,
            HapFormat::Dxt5 => wgpu::TextureFormat::Bc3RgbaUnormSrgb,
            // Not sRGB: the values are YCoCg, converted in the shader.
            HapFormat::YCoCgDxt5 => wgpu::TextureFormat::Bc3RgbaUnorm,
            HapFormat::Bc7 => wgpu::TextureFormat::Bc7RgbaUnormSrgb,
        }
    }

    fn encoding(self) -> Encoding {
        match self {
            // DXT1 in HAP is RGB: ignore the format's 1-bit alpha.
            HapFormat::Dxt1 => Encoding::Opaque,
            HapFormat::Dxt5 | HapFormat::Bc7 => Encoding::Straight,
            HapFormat::YCoCgDxt5 => Encoding::ScaledYCoCg,
        }
    }
}

/// Size in bytes of the texture data of one frame.
fn texture_len(format: HapFormat, width: u32, height: u32) -> usize {
    width.div_ceil(4) as usize * height.div_ceil(4) as usize * format.block_bytes()
}

/// One section header: (payload length, type byte, header length).
fn section(data: &[u8]) -> anyhow::Result<(usize, u8, usize)> {
    if data.len() < 4 {
        bail!("truncated section header");
    }
    let small = u32::from_le_bytes([data[0], data[1], data[2], 0]) as usize;
    let kind = data[3];
    let (len, header) = if small == 0 {
        let big = data.get(4..8).context("truncated long section header")?;
        (u32::from_le_bytes(big.try_into().unwrap()) as usize, 8)
    } else {
        (small, 4)
    };
    if header + len > data.len() {
        bail!("section of {len} bytes overruns the frame");
    }
    Ok((len, kind, header))
}

/// Decode one HAP frame into texture blocks (`out`, already the right
/// length). Chunked frames are unpacked on up to `threads` threads.
pub fn decode_frame(
    frame: &[u8],
    format: HapFormat,
    out: &mut [u8],
    threads: usize,
) -> anyhow::Result<()> {
    let (len, kind, header) = section(frame)?;
    let payload = &frame[header..header + len];
    if kind & 0x0F != format.section_type() {
        bail!(
            "frame has texture type {:#x}, expected {:#x}",
            kind & 0x0F,
            format.section_type()
        );
    }
    match kind >> 4 {
        // Uncompressed.
        0xA => {
            if payload.len() != out.len() {
                bail!(
                    "frame holds {} bytes of texture, expected {}",
                    payload.len(),
                    out.len()
                );
            }
            out.copy_from_slice(payload);
        }
        // One Snappy block.
        0xB => unsnap(payload, out)?,
        // "Complex": decode instructions, then chunks.
        0xC => decode_chunked(payload, out, threads)?,
        other => bail!("unknown HAP compressor {other:#x}"),
    }
    Ok(())
}

fn unsnap(input: &[u8], out: &mut [u8]) -> anyhow::Result<()> {
    let n = snap::raw::decompress_len(input)?;
    if n != out.len() {
        bail!("chunk unpacks to {n} bytes, expected {}", out.len());
    }
    snap::raw::Decoder::new().decompress(input, out)?;
    Ok(())
}

/// A chunked frame: the texture is split into chunks compressed separately,
/// so they can be unpacked in parallel.
fn decode_chunked(payload: &[u8], out: &mut [u8], threads: usize) -> anyhow::Result<()> {
    let (instr_len, kind, header) = section(payload)?;
    if kind != 0x01 {
        bail!("chunked frame without decode instructions");
    }
    let instructions = &payload[header..header + instr_len];
    let data = &payload[header + instr_len..];

    let (mut compressors, mut sizes, mut offsets) = (None, None, None);
    let mut rest = instructions;
    while !rest.is_empty() {
        let (len, kind, header) = section(rest)?;
        let body = &rest[header..header + len];
        match kind {
            0x02 => compressors = Some(body),
            0x03 => sizes = Some(le32_table(body)),
            0x04 => offsets = Some(le32_table(body)),
            _ => {} // e.g. frame dimensions: not needed
        }
        rest = &rest[header + len..];
    }
    let compressors = compressors.context("no chunk compressor table")?;
    let sizes = sizes.context("no chunk size table")?;
    if sizes.len() != compressors.len() {
        bail!("chunk tables disagree");
    }
    // Where each chunk is in `data`: given, or one after another.
    let starts: Vec<usize> = match offsets {
        Some(o) if o.len() == sizes.len() => o.iter().map(|&v| v as usize).collect(),
        _ => sizes
            .iter()
            .scan(0usize, |at, &size| {
                let start = *at;
                *at += size as usize;
                Some(start)
            })
            .collect(),
    };
    let inputs: Vec<(&[u8], u8)> = starts
        .iter()
        .zip(&sizes)
        .zip(compressors)
        .map(|((&start, &size), &c)| {
            data.get(start..start + size as usize)
                .map(|bytes| (bytes, c))
                .context("chunk outside the frame")
        })
        .collect::<anyhow::Result<_>>()?;

    // Each chunk's unpacked length decides where in the texture it goes.
    let lengths: Vec<usize> = inputs
        .iter()
        .map(|&(bytes, c)| match c {
            0x0A => Ok(bytes.len()),
            0x0B => Ok(snap::raw::decompress_len(bytes)?),
            other => bail!("unknown chunk compressor {other:#x}"),
        })
        .collect::<anyhow::Result<_>>()?;
    if lengths.iter().sum::<usize>() != out.len() {
        bail!(
            "chunks unpack to {} bytes, expected {}",
            lengths.iter().sum::<usize>(),
            out.len()
        );
    }

    // Disjoint slices of `out`, one per chunk.
    let mut targets = Vec::with_capacity(lengths.len());
    let mut remaining = out;
    for &n in &lengths {
        let (head, tail) = remaining.split_at_mut(n);
        targets.push(head);
        remaining = tail;
    }
    let jobs: Vec<_> = inputs.into_iter().zip(targets).collect();
    let unpack = |(input, c): (&[u8], u8), target: &mut [u8]| -> anyhow::Result<()> {
        match c {
            0x0A => {
                target.copy_from_slice(input);
                Ok(())
            }
            _ => unsnap(input, target),
        }
    };
    let threads = threads.clamp(1, jobs.len().max(1));
    if threads == 1 {
        for (input, target) in jobs {
            unpack(input, target)?;
        }
        return Ok(());
    }
    // Split the chunks into `threads` groups and unpack the groups in
    // parallel.
    let per = jobs.len().div_ceil(threads);
    let mut groups: Vec<Vec<_>> = Vec::new();
    let mut it = jobs.into_iter();
    loop {
        let group: Vec<_> = it.by_ref().take(per).collect();
        if group.is_empty() {
            break;
        }
        groups.push(group);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = groups
            .into_iter()
            .map(|group| {
                scope.spawn(move || {
                    crate::priority::background_thread();
                    for (input, target) in group {
                        unpack(input, target)?;
                    }
                    Ok::<(), anyhow::Error>(())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .map_err(|_| anyhow::anyhow!("unpack thread panicked"))??;
        }
        Ok(())
    })
}

fn le32_table(body: &[u8]) -> Vec<u32> {
    body.chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

// ───────────────────────────── playback ─────────────────────────────

/// What the decoding workers share with the renderer.
struct Shared {
    state: Mutex<Sched>,
    wake: Condvar,
}

struct Sched {
    /// The frames wanted soonest, in order (the one on screen first).
    wanted: Vec<i64>,
    /// Decoded texture data, by frame.
    ready: BTreeMap<i64, Vec<u8>>,
    /// Frames a worker is busy with.
    busy: HashSet<i64>,
    /// Frames that failed to decode (reported once, not retried).
    failed: HashSet<i64>,
    /// Texture-sized buffers to decode into again. Allocating and zeroing a
    /// fresh one per frame (8 MB at 4K HAP Q) cost about as much as a third
    /// of the decompression itself; every decode overwrites the whole
    /// buffer (the chunk sizes are checked to add up to it), so none needs
    /// clearing.
    spare: Vec<Vec<u8>>,
    stop: bool,
}

/// Buffers kept for reuse per video: the prefetch window plus a margin.
const SPARE_BUFFERS: usize = 8;

/// Keep a texture buffer for the next decode (up to `SPARE_BUFFERS`).
fn recycle(st: &mut Sched, buffer: Vec<u8>) {
    // A stopped layer keeps nothing.
    if !st.stop && st.spare.len() < SPARE_BUFFERS {
        st.spare.push(buffer);
    }
}

/// A HAP video layer.
pub struct HapContent {
    path: String,
    track: Arc<VideoTrack>,
    format: HapFormat,
    timing: Timing,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    sampling: Sampling,
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Frame now in the texture.
    shown: Option<i64>,
    /// Last frame asked for, to tell the direction of playback.
    last_want: Option<i64>,
    step: i64,
}

impl HapContent {
    /// Frames in one pass of a looping clip (`None` if it does not loop).
    fn loop_span(&self) -> Option<i64> {
        let total = self.track.frames.len() as i64;
        let first = (self.timing.media_in.max(0.0) * self.track.fps()).floor() as i64;
        self.timing
            .media_loop
            .then_some(total - first)
            .filter(|&n| n > 0)
    }

    fn new(path: &str, track: VideoTrack, timing: &Timing, gpu: &TextureFactory) -> Self {
        let format = HapFormat::from_codec(&track.codec).expect("checked by open_video");
        // Compressed textures come in whole 4×4 blocks.
        let (tw, th) = (track.width.div_ceil(4) * 4, track.height.div_ceil(4) * 4);
        let texture = gpu.texture(path, tw, th, format.texture_format());
        let bind_group = gpu.bind(path, [&texture, &texture, &texture]);
        let sampling = Sampling {
            mode: format.encoding(),
            uv_scale: [
                track.width as f32 / tw as f32,
                track.height as f32 / th as f32,
            ],
            param: [0.0; 4],
        };
        tracing::info!(
            "video '{path}': {}x{} at {:.3} fps, {} frames, HAP ({}) — decoded by the GPU",
            track.width,
            track.height,
            track.fps(),
            track.frames.len(),
            track.codec_str()
        );
        HapContent {
            path: path.to_string(),
            track: Arc::new(track),
            format,
            timing: timing.clone(),
            texture,
            bind_group,
            sampling,
            shared: Arc::new(Shared {
                state: Mutex::new(Sched {
                    wanted: Vec::new(),
                    ready: BTreeMap::new(),
                    spare: Vec::new(),
                    busy: HashSet::new(),
                    failed: HashSet::new(),
                    stop: false,
                }),
                wake: Condvar::new(),
            }),
            workers: Vec::new(),
            shown: None,
            last_want: None,
            step: 1,
        }
    }

    /// Start the decoding workers (they stop again in `idle`).
    fn start_workers(&mut self) {
        if !self.workers.is_empty() {
            return;
        }
        self.shared.state.lock().unwrap().stop = false;
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        // Two frame-level workers keep the pipeline full; each also unpacks
        // the chunks of its frame in parallel.
        let workers = 2.min(cores);
        let chunk_threads = (cores / workers).clamp(1, 8);
        for _ in 0..workers {
            let (shared, track, path, format) = (
                self.shared.clone(),
                self.track.clone(),
                self.path.clone(),
                self.format,
            );
            let spawned = std::thread::Builder::new()
                .name("lc-hap".into())
                .spawn(move || {
                    crate::priority::background_thread();
                    worker(shared, track, path, format, chunk_threads)
                });
            match spawned {
                Ok(handle) => self.workers.push(handle),
                Err(e) => tracing::error!("cannot start a HAP decoding thread: {e}"),
            }
        }
    }

    fn stop_workers(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.stop = true;
            st.ready.clear();
            // An idle layer holds no texture buffers (8 MB each at 4K HAP Q).
            st.spare = Vec::new();
            // A frame that failed (a read error) gets another chance later.
            st.failed.clear();
        }
        self.shared.wake.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        // A worker that was mid-frame may have handed in its result since.
        let mut st = self.shared.state.lock().unwrap();
        st.ready.clear();
        st.spare = Vec::new();
    }

    /// The frames to have ready, soonest first: the wanted one, then the
    /// next few in the direction of playback (wrapping when the clip loops).
    fn window(&self, want: i64) -> Vec<i64> {
        let total = self.track.frames.len() as i64;
        let first = (self.timing.media_in.max(0.0) * self.track.fps()).floor() as i64;
        let loops = self.timing.media_loop;
        prefetch_window(want, self.step, total, first, loops)
    }
}

/// The playback stride after the wanted frame moved from `last` to `want`:
/// the actual difference, unless it is no movement, an implausible jump, or
/// (for a loop of `span` frames) a loop wrapping round — then the previous
/// stride stands.
fn next_stride(previous: i64, last: i64, want: i64, span: Option<i64>) -> i64 {
    let delta = want - last;
    let wrapped = span.is_some_and(|span| delta.abs() > span / 2);
    if delta != 0 && !wrapped && delta.abs() <= MAX_STRIDE {
        delta
    } else {
        previous
    }
}

/// The frames to have decoded, soonest first: `want`, then the next ones at
/// `step` apart, wrapping into `first..total` if the clip loops and stopping
/// at its ends otherwise.
fn prefetch_window(want: i64, step: i64, total: i64, first: i64, loops: bool) -> Vec<i64> {
    let mut frames = Vec::with_capacity(AHEAD as usize);
    for k in 0..AHEAD {
        let mut f = want + k * step;
        if loops && total > first {
            f = first + (f - first).rem_euclid(total - first);
        } else if !(0..total).contains(&f) {
            break;
        }
        if !frames.contains(&f) {
            frames.push(f);
        }
    }
    frames
}

fn worker(
    shared: Arc<Shared>,
    track: Arc<VideoTrack>,
    path: String,
    format: HapFormat,
    chunk_threads: usize,
) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("video '{path}': {e}");
            return;
        }
    };
    let len = texture_len(format, track.width, track.height);
    let mut packet = Vec::new();
    loop {
        // Pick the soonest wanted frame nobody has or is working on, and a
        // buffer to decode it into.
        let (frame, mut out) = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                let next = st.wanted.iter().copied().find(|f| {
                    !st.ready.contains_key(f) && !st.busy.contains(f) && !st.failed.contains(f)
                });
                match next {
                    Some(f) => {
                        st.busy.insert(f);
                        let buffer = st.spare.pop().filter(|b| b.len() == len);
                        break (f, buffer.unwrap_or_else(|| vec![0u8; len]));
                    }
                    None => st = shared.wake.wait(st).unwrap(),
                }
            }
        };

        let result = (|| -> anyhow::Result<()> {
            let &(offset, size) = track
                .frames
                .get(frame as usize)
                .context("frame out of range")?;
            packet.resize(size as usize, 0);
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut packet)?;
            decode_frame(&packet, format, &mut out, chunk_threads)
        })();

        let mut st = shared.state.lock().unwrap();
        st.busy.remove(&frame);
        match result {
            // Keep it only if it is still wanted (playback may have jumped).
            Ok(()) if !st.stop && st.wanted.contains(&frame) => {
                st.ready.insert(frame, out);
            }
            Ok(()) => recycle(&mut st, out),
            Err(e) => {
                recycle(&mut st, out);
                if st.failed.is_empty() {
                    tracing::error!("video '{path}', frame {frame}: {e:#}");
                }
                st.failed.insert(frame);
            }
        }
    }
}

impl Content for HapContent {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        let total = Some(self.track.frames.len() as i64);
        let want = media_frame(&self.timing, self.track.fps(), total, t);
        if let Some(last) = self.last_want {
            // The stride of playback, in frames per rendered frame: 1 at
            // normal speed, 2 at double speed (or with a 60 fps clip on a
            // 30 fps output), negative in reverse. Prefetching follows it, so
            // the frames decoded are the ones that will be wanted. A jump of
            // more than half the loop is a loop wrapping round, not a change
            // of direction: the stride is kept.
            self.step = next_stride(self.step, last, want, self.loop_span());
        }
        self.last_want = Some(want);
        self.start_workers();

        let window = self.window(want);
        let frame = {
            let mut st = self.shared.state.lock().unwrap();
            // Drop what is no longer needed, publish what is.
            // Frames no longer wanted give their buffers back.
            let dropped: Vec<i64> = st
                .ready
                .keys()
                .copied()
                .filter(|f| !window.contains(f))
                .collect();
            for f in dropped {
                if let Some(buffer) = st.ready.remove(&f) {
                    recycle(&mut st, buffer);
                }
            }
            st.wanted = window;
            self.shared.wake.notify_all();
            if self.shown == Some(want) {
                return;
            }
            // Taken out, not copied: the texture is about to hold it.
            st.ready.remove(&want).map(|data| (want, data))
        };
        // The wanted frame is not decoded yet: keep showing the previous one
        // (never wait — every output on the machine would stall).
        let Some((index, data)) = frame else {
            return;
        };
        let (blocks_w, blocks_h) = (self.track.width.div_ceil(4), self.track.height.div_ceil(4));
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(blocks_w * self.format.block_bytes() as u32),
                rows_per_image: Some(blocks_h),
            },
            wgpu::Extent3d {
                width: blocks_w * 4,
                height: blocks_h * 4,
                depth_or_array_layers: 1,
            },
        );
        self.shown = Some(index);
        // `write_texture` has copied it: the buffer can take the next frame.
        recycle(&mut self.shared.state.lock().unwrap(), data);
    }

    fn idle(&mut self) {
        self.stop_workers();
        self.last_want = None;
    }

    fn retime(&mut self, timing: &Timing) {
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

impl Drop for HapContent {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

// ───────────────────────────── opening a video ─────────────────────────────

/// What the loader thread found out about a video file.
enum Probed {
    Hap(VideoTrack),
    Other(VideoInfo),
}

/// A video layer: transparent while its file is examined on a worker thread,
/// then whichever player suits the file.
pub struct LazyVideo {
    path: String,
    timing: Timing,
    state: State,
    /// Shown while loading.
    loading: StaticContent,
}

enum State {
    Loading(Receiver<anyhow::Result<Probed>>),
    Hap(HapContent),
    Ffmpeg(VideoContent),
    Failed(StaticContent),
}

/// Open a video file for a layer. Returns at once; the file is examined in
/// the background.
pub fn open_video(path: &str, timing: &Timing, gpu: &TextureFactory) -> Box<dyn Content> {
    let (tx, rx) = std::sync::mpsc::channel();
    let owned = path.to_string();
    let (supports_bc, max) = (gpu.supports_bc, gpu.max_dimension);
    let spawned = std::thread::Builder::new()
        .name("lc-video-open".into())
        .spawn(move || {
            crate::priority::background_thread();
            let result = (|| -> anyhow::Result<Probed> {
                if !std::path::Path::new(&owned).is_file() {
                    bail!("video file '{owned}' not found");
                }
                // HAP in QuickTime: our own GPU path. Anything else (or HAP
                // this GPU cannot take): ffmpeg.
                if let Ok(track) = read_video_track(&owned) {
                    let fits =
                        track.width.div_ceil(4) * 4 <= max && track.height.div_ceil(4) * 4 <= max;
                    if HapFormat::from_codec(&track.codec).is_some() && supports_bc && fits {
                        return Ok(Probed::Hap(track));
                    }
                    if HapFormat::from_codec(&track.codec).is_some() {
                        tracing::warn!(
                            "video '{owned}' is HAP but this GPU cannot decode it ({}); \
                             falling back to software decoding, which is slow",
                            if supports_bc {
                                "too large"
                            } else {
                                "no BC texture support"
                            }
                        );
                    }
                }
                Ok(Probed::Other(VideoInfo::probe(&owned)?))
            })();
            let _ = tx.send(result);
        });
    let (tx_fail, rx_fail);
    let rx = match spawned {
        Ok(_) => rx,
        Err(e) => {
            (tx_fail, rx_fail) = std::sync::mpsc::channel();
            let _ = tx_fail.send(Err(anyhow::anyhow!("cannot start a loader thread: {e}")));
            rx_fail
        }
    };
    Box::new(LazyVideo {
        path: path.to_string(),
        timing: timing.clone(),
        state: State::Loading(rx),
        loading: gpu.transparent(),
    })
}

impl LazyVideo {
    fn inner(&mut self) -> Option<&mut dyn Content> {
        match &mut self.state {
            State::Hap(c) => Some(c),
            State::Ffmpeg(c) => Some(c),
            _ => None,
        }
    }
}

impl Content for LazyVideo {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        if let State::Loading(rx) = &self.state {
            self.state = match rx.try_recv() {
                Err(TryRecvError::Empty) => return,
                Ok(Ok(Probed::Hap(track))) => {
                    State::Hap(HapContent::new(&self.path, track, &self.timing, gpu))
                }
                Ok(Ok(Probed::Other(info))) => {
                    State::Ffmpeg(VideoContent::new(&self.path, info, &self.timing, gpu))
                }
                Ok(Err(e)) => {
                    tracing::error!("{e:#}; showing placeholder");
                    State::Failed(placeholder_static(gpu))
                }
                Err(TryRecvError::Disconnected) => State::Failed(placeholder_static(gpu)),
            };
        }
        if let Some(inner) = self.inner() {
            inner.update(t, gpu);
        }
    }

    fn retime(&mut self, timing: &Timing) {
        self.timing = timing.clone();
        if let Some(inner) = self.inner() {
            inner.retime(timing);
        }
    }

    fn idle(&mut self) {
        if let Some(inner) = self.inner() {
            inner.idle();
        }
    }

    fn bind_group(&self) -> &wgpu::BindGroup {
        match &self.state {
            State::Hap(c) if c.ready() => c.bind_group(),
            State::Ffmpeg(c) if c.ready() => c.bind_group(),
            State::Failed(p) => &p.bind_group,
            _ => &self.loading.bind_group,
        }
    }

    fn sampling(&self) -> Sampling {
        match &self.state {
            State::Hap(c) if c.ready() => c.sampling(),
            State::Ffmpeg(c) if c.ready() => c.sampling(),
            _ => Sampling::default(),
        }
    }

    fn late(&self) -> bool {
        match &self.state {
            State::Hap(c) => c.late(),
            State::Ffmpeg(c) => c.late(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Encode a test clip with ffmpeg's HAP encoder, if it has one.
    fn make_hap(dir: &std::path::Path, format: &str, chunks: u32) -> Option<String> {
        let path = dir.join(format!("{format}-{chunks}.mov"));
        let ok = Command::new(std::env::var("LCPLAY_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()))
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            // 10 fps, 2 s: a white bar moving 4 px per frame, as in the
            // ffmpeg-path test. (The HAP encoder only takes multiples of 4.)
            .arg(r"color=black:s=68x52:r=10:d=2,format=yuv420p,geq=lum='if(between(X\,4*N\,4*N+3)\,255\,0)':cb=128:cr=128")
            .args([
                "-c:v",
                "hap",
                "-format",
                format,
                "-chunks",
                &chunks.to_string(),
            ])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then(|| path.to_string_lossy().into_owned())
    }

    #[test]
    fn hap_frames_unpack_to_whole_textures() {
        let dir = std::env::temp_dir().join(format!("lcplay-hap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, expect) in [
            ("hap", HapFormat::Dxt1),
            ("hap_alpha", HapFormat::Dxt5),
            ("hap_q", HapFormat::YCoCgDxt5),
        ] {
            for chunks in [1, 4] {
                let Some(path) = make_hap(&dir, format, chunks) else {
                    eprintln!("SKIPPED: ffmpeg with a HAP encoder is not available");
                    return;
                };
                let track = read_video_track(&path).unwrap();
                assert_eq!(HapFormat::from_codec(&track.codec), Some(expect), "{path}");
                assert_eq!((track.width, track.height), (68, 52));
                assert_eq!(track.frames.len(), 20);
                assert!((track.fps() - 10.0).abs() < 1e-9);

                let mut file = File::open(&path).unwrap();
                let mut out = vec![0u8; texture_len(expect, 68, 52)];
                for &(offset, size) in &track.frames {
                    let mut packet = vec![0u8; size as usize];
                    file.seek(SeekFrom::Start(offset)).unwrap();
                    file.read_exact(&mut packet).unwrap();
                    decode_frame(&packet, expect, &mut out, 4)
                        .unwrap_or_else(|e| panic!("{path}: {e:#}"));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Throughput of the CPU side of HAP playback (read + unpack), on a file
    /// given in `LCPLAY_BENCH_HAP`:
    /// `LCPLAY_BENCH_HAP=big.mov cargo test -p lc-runner --release -- --ignored --nocapture bench`
    #[test]
    #[ignore]
    fn bench_hap_unpack() {
        let Ok(path) = std::env::var("LCPLAY_BENCH_HAP") else {
            return;
        };
        let track = read_video_track(&path).unwrap();
        let format = HapFormat::from_codec(&track.codec).unwrap();
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let mut out = vec![0u8; texture_len(format, track.width, track.height)];
        let mut file = File::open(&path).unwrap();
        let start = std::time::Instant::now();
        for &(offset, size) in &track.frames {
            let mut packet = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.read_exact(&mut packet).unwrap();
            decode_frame(&packet, format, &mut out, cores).unwrap();
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / track.frames.len() as f64;
        println!(
            "{}x{} {}: {per:.1} ms per frame on one worker ({} cores) → {:.0} fps; texture {:.1} MB/frame",
            track.width,
            track.height,
            track.codec_str(),
            cores,
            1000.0 / per,
            out.len() as f64 / 1e6
        );
    }

    #[test]
    fn prefetch_follows_speed_direction_and_loops() {
        // Normal speed, double speed, reverse.
        assert_eq!(prefetch_window(10, 1, 60, 0, false), [10, 11, 12, 13]);
        assert_eq!(prefetch_window(10, 2, 60, 0, false), [10, 12, 14, 16]);
        assert_eq!(prefetch_window(10, -1, 60, 0, false), [10, 9, 8, 7]);
        // Near the end of a looping clip: wraps to its start.
        assert_eq!(prefetch_window(58, 1, 60, 0, true), [58, 59, 0, 1]);
        // Not looping: stops at the end.
        assert_eq!(prefetch_window(58, 1, 60, 0, false), [58, 59]);

        // The stride follows the movement…
        assert_eq!(next_stride(1, 10, 12, None), 2);
        assert_eq!(next_stride(1, 10, 9, None), -1);
        // …but a loop wrapping (59 → 0) is not a reversal, and standing
        // still or a seek keeps the stride.
        assert_eq!(next_stride(1, 59, 0, Some(60)), 1);
        assert_eq!(next_stride(2, 10, 10, None), 2);
        assert_eq!(next_stride(1, 10, 500, None), 1);
    }

    #[test]
    fn malformed_frames_are_errors_not_panics() {
        let mut out = vec![0u8; 64];
        for bad in [
            &b""[..],
            b"\x01\x00",
            b"\xff\xff\xff\xbb",
            b"\x00\x00\x00\xcb\x05\x00\x00\x00",
        ] {
            assert!(decode_frame(bad, HapFormat::Dxt1, &mut out, 2).is_err());
        }
    }
}
