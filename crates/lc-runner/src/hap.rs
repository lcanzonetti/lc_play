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
//!       │  (chunks of one frame in parallel) into ordinary memory, then copy
//!       │  the texture into a mapped GPU upload buffer
//!   ready frames ──▶ renderer records "copy buffer → texture" ──▶ GPU samples
//! ```
//!
//! ### Upload buffers
//!
//! Getting a frame to the GPU means copying its texture data (4 MB for a 4K
//! DXT1 frame, 11 MB for 10K) into memory the GPU can read. Done on the
//! render thread (`Queue::write_texture`), that copy cost about 0.5–0.8 ms
//! per MB on a laptop — three HAP layers took 12–16 ms of a 16.7 ms frame.
//! So each layer has a small ring of upload buffers ([`SLOTS`]), kept mapped
//! while they are free:
//!
//! ```text
//!   free (mapped) ──worker writes frame──▶ ready ──renderer: unmap, record
//!        ▲                                          copy, submit with frame
//!        └──── mapped again once the GPU has copied it ◀──────┘
//! ```
//!
//! The buffers are made by the workers, one at a time as they need them
//! (up to [`SLOTS`]): wgpu fills a new mapped buffer with zeros, 4–11 MB
//! each, which took 100–200 ms for eight 10K buffers on the render thread.
//!
//! Workers write into a buffer only after unpacking into ordinary memory:
//! upload memory is write-combined, fast to write in order and very slow to
//! read, and Snappy reads back what it has written. Rows are padded to the
//! pitch the GPU copy requires (256 bytes). The render thread only unmaps
//! and records the copy; the mapping is requested again after the frame
//! that copied from the buffer has been submitted.
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
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context as _};

use crate::content::{
    placeholder_static, Content, Encoding, Sampling, StaticContent, TextureFactory,
};
use crate::show::Timing;
use crate::video::{media_frame, VideoContent, VideoInfo};
use lc_hap::frame::decode_frame;
use lc_hap::mov::{read_video_track, VideoTrack};

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

/// Upload buffers per playing layer: the prefetch window, a frame per
/// worker, and the ones the GPU is still copying from.
const SLOTS: usize = 8;

/// Bytes per row of 4×4 blocks in an upload buffer: the row of the texture,
/// padded to the alignment GPU buffer-to-texture copies require.
fn upload_pitch(format: HapFormat, width: u32) -> u32 {
    let row = width.div_ceil(4) * format.block_bytes() as u32;
    row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// Copy texture rows of `row` bytes from `src` into `dst`, where they are
/// `pitch` bytes apart (the padding is left as it is: the GPU ignores it).
/// Written in order, as write-combined upload memory wants.
fn copy_rows(src: &[u8], dst: &mut [u8], row: usize, pitch: usize) {
    if row == pitch {
        dst[..src.len()].copy_from_slice(src);
        return;
    }
    for (s, d) in src.chunks_exact(row).zip(dst.chunks_exact_mut(pitch)) {
        d[..row].copy_from_slice(s);
    }
}

/// The mapped memory of an upload buffer, owned by exactly one party at a
/// time (the free list, a worker, the ready list).
///
/// It stays valid until the render thread unmaps the buffer, which it does
/// only once this value has come back to it (from the ready list) or every
/// worker has stopped: wgpu (24) keeps a buffer's mapping until `unmap`; the
/// view the pointer was taken from is dropped at once, its drop being
/// bookkeeping only (`CoreBufferMappedRange` has an empty `Drop`).
struct Mapped {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: plain memory, with a single owner at any time (see above).
unsafe impl Send for Mapped {}

impl Mapped {
    /// The mapped memory of `buffer`, which must be mapped for writing.
    fn of(buffer: &wgpu::Buffer) -> Mapped {
        let mut view = buffer.slice(..).get_mapped_range_mut();
        let bytes = view.as_mut();
        Mapped {
            ptr: NonNull::new(bytes.as_mut_ptr()).expect("mapped memory"),
            len: bytes.len(),
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: valid and exclusively ours (see the type's docs).
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

/// What the decoding workers share with the renderer.
struct Shared {
    state: Mutex<Sched>,
    wake: Condvar,
}

struct Sched {
    /// The frames wanted soonest, in order (the one on screen first).
    wanted: Vec<i64>,
    /// Decoded frames, by frame: the upload buffer (slot) holding each.
    ready: BTreeMap<i64, (usize, Mapped)>,
    /// Upload buffers that are mapped and free to fill.
    free: Vec<(usize, Mapped)>,
    /// Upload buffers made so far (slots `0..created`), up to [`SLOTS`].
    created: usize,
    /// Buffers a worker has made, for the renderer to keep: (slot, buffer).
    /// Taken in the same lock as a ready frame, so a frame's buffer is
    /// always known to the renderer by the time it takes the frame.
    made: Vec<(usize, wgpu::Buffer)>,
    /// Frames a worker is busy with.
    busy: HashSet<i64>,
    /// Frames that failed to decode (reported once, not retried).
    failed: HashSet<i64>,
    stop: bool,
}

/// Map callbacks report here: (generation of the buffers, slot).
type MappedSlots = Arc<Mutex<Vec<(u64, usize)>>>;

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
    /// The upload buffers while playing (empty when idle), by slot; made by
    /// the workers.
    uploads: Vec<Option<wgpu::Buffer>>,
    /// Bumped whenever `uploads` is made anew: a map callback of an older
    /// buffer must not be taken for one of the new ones.
    generation: u64,
    /// Slots whose copy to the texture was recorded, with the upload batch
    /// it is in; mapped again once that batch has been submitted.
    in_flight: Vec<(usize, u64)>,
    /// Slots whose mapping has completed (from the map callbacks).
    mapped: MappedSlots,
    /// Frame now in the texture.
    shown: Option<i64>,
    /// Last frame due, to tell the direction of playback.
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
                    free: Vec::new(),
                    created: 0,
                    made: Vec::new(),
                    busy: HashSet::new(),
                    failed: HashSet::new(),
                    stop: false,
                }),
                wake: Condvar::new(),
            }),
            workers: Vec::new(),
            uploads: Vec::new(),
            generation: 0,
            in_flight: Vec::new(),
            mapped: Arc::default(),
            shown: None,
            last_want: None,
            step: 1,
        }
    }

    /// Start the decoding workers; they make the upload buffers as they
    /// need them (both go again in `idle`).
    fn start_workers(&mut self, gpu: &TextureFactory) {
        if !self.workers.is_empty() {
            return;
        }
        let pitch = upload_pitch(self.format, self.track.width);
        let uploads = Uploads {
            device: gpu.device.clone(),
            size: pitch as u64 * self.track.height.div_ceil(4) as u64,
            pitch,
        };
        self.generation += 1;
        self.shared.state.lock().unwrap().stop = false;
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        // Two frame-level workers keep the pipeline full; each also unpacks
        // the chunks of its frame in parallel.
        // Chunk threads (in lc-hap) give way to the render thread too.
        lc_hap::frame::set_thread_init(crate::priority::background_thread);
        let workers = 2.min(cores);
        let chunk_threads = (cores / workers).clamp(1, 8);
        for _ in 0..workers {
            let (shared, track, path, format, uploads) = (
                self.shared.clone(),
                self.track.clone(),
                self.path.clone(),
                self.format,
                uploads.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name("lc-hap".into())
                .spawn(move || {
                    crate::priority::background_thread();
                    worker(shared, track, path, format, chunk_threads, uploads)
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
            // A frame that failed (a read error) gets another chance later.
            st.failed.clear();
        }
        self.shared.wake.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        // No worker writes any more: the mapped memory can go, then the
        // buffers (an idle layer holds none: 4–11 MB each at 4K–10K).
        {
            let mut st = self.shared.state.lock().unwrap();
            st.ready.clear();
            st.free.clear();
            st.made.clear();
            st.created = 0;
        }
        self.uploads.clear();
        self.in_flight.clear();
        self.mapped.lock().unwrap().clear();
    }

    /// Give upload buffers back to the workers: request the mapping of those
    /// the GPU has been asked to copy from (their frame is submitted), and
    /// hand out those whose mapping has completed.
    fn reclaim_uploads(&mut self, gpu: &TextureFactory) {
        let submitted = gpu.uploads_submitted();
        let (generation, uploads, mapped) = (self.generation, &self.uploads, &self.mapped);
        self.in_flight.retain(|&(slot, batch)| {
            if batch >= submitted {
                return true; // its copy is not submitted yet
            }
            let done = mapped.clone();
            // Completes once the GPU has finished with the buffer; the
            // callback runs in a later submit or poll on this thread.
            uploads[slot]
                .as_ref()
                .expect("a recorded buffer is known")
                .slice(..)
                .map_async(wgpu::MapMode::Write, move |result| {
                    if result.is_ok() {
                        done.lock().unwrap().push((generation, slot));
                    }
                });
            false
        });
        let ready: Vec<usize> = std::mem::take(&mut *self.mapped.lock().unwrap())
            .into_iter()
            .filter(|&(g, _)| g == generation)
            .map(|(_, slot)| slot)
            .collect();
        if ready.is_empty() {
            return;
        }
        let mut st = self.shared.state.lock().unwrap();
        for slot in ready {
            let buffer = self.uploads[slot]
                .as_ref()
                .expect("a mapped buffer is known");
            st.free.push((slot, Mapped::of(buffer)));
        }
        drop(st);
        self.shared.wake.notify_all();
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

/// What a worker needs to make upload buffers.
#[derive(Clone)]
struct Uploads {
    device: wgpu::Device,
    size: u64,
    pitch: u32,
}

impl Uploads {
    /// A new upload buffer, mapped. (wgpu fills it with zeros: the slow part,
    /// kept off the render thread.)
    fn make(&self) -> (wgpu::Buffer, Mapped) {
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hap-upload"),
            size: self.size,
            usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        });
        let mapped = Mapped::of(&buffer);
        (buffer, mapped)
    }
}

/// Where a worker puts the frame it took on.
enum Slot {
    Free(usize, Mapped),
    /// None free, fewer than `SLOTS` made: make this one.
    New(usize),
}

fn worker(
    shared: Arc<Shared>,
    track: Arc<VideoTrack>,
    path: String,
    format: HapFormat,
    chunk_threads: usize,
    uploads: Uploads,
) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("video '{path}': {e}");
            return;
        }
    };
    let row = track.width.div_ceil(4) as usize * format.block_bytes();
    // Unpacked here, in ordinary memory, then copied to the upload buffer.
    // Every decode overwrites all of it (the chunk sizes are checked to add
    // up to it), so it is allocated once.
    let mut texture = vec![0u8; texture_len(format, track.width, track.height)];
    let mut packet = Vec::new();
    loop {
        // Pick the soonest wanted frame nobody has or is working on, and an
        // upload buffer to put it in.
        let (frame, slot) = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                let next = st.wanted.iter().copied().find(|f| {
                    !st.ready.contains_key(f) && !st.busy.contains(f) && !st.failed.contains(f)
                });
                match next {
                    Some(f) if !st.free.is_empty() => {
                        st.busy.insert(f);
                        let (slot, mapped) = st.free.pop().expect("not empty");
                        break (f, Slot::Free(slot, mapped));
                    }
                    Some(f) if st.created < SLOTS => {
                        st.busy.insert(f);
                        st.created += 1;
                        break (f, Slot::New(st.created - 1));
                    }
                    // Nothing to do, or every buffer taken: wait for the
                    // renderer.
                    _ => st = shared.wake.wait(st).unwrap(),
                }
            }
        };
        let (slot, mut mapped) = match slot {
            Slot::Free(slot, mapped) => (slot, mapped),
            Slot::New(slot) => {
                let (buffer, mapped) = uploads.make();
                shared.state.lock().unwrap().made.push((slot, buffer));
                (slot, mapped)
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
            decode_frame(&packet, format.section_type(), &mut texture, chunk_threads)?;
            copy_rows(&texture, mapped.bytes(), row, uploads.pitch as usize);
            Ok(())
        })();

        let mut st = shared.state.lock().unwrap();
        st.busy.remove(&frame);
        match result {
            // Keep it only if it is still wanted (playback may have jumped).
            Ok(()) if !st.stop && st.wanted.contains(&frame) => {
                st.ready.insert(frame, (slot, mapped));
            }
            Ok(()) => st.free.push((slot, mapped)),
            Err(e) => {
                st.free.push((slot, mapped));
                if st.failed.is_empty() {
                    tracing::error!("video '{path}', frame {frame}: {e:#}");
                }
                st.failed.insert(frame);
            }
        }
        drop(st);
        // A buffer is free again, or a frame ready.
        shared.wake.notify_all();
    }
}

impl HapContent {
    fn due(&self, t: f64) -> i64 {
        let total = Some(self.track.frames.len() as i64);
        media_frame(&self.timing, self.track.fps(), total, t)
    }

    /// Bring the texture to frame `want` (the one due, or the one another
    /// card decided on: [`Content::follow`]); `due` is the frame the clock
    /// asks for. HAP shows exactly the frame asked for or holds the last
    /// one, so following needs nothing more than asking for that frame.
    fn advance(&mut self, due: i64, want: i64, gpu: &TextureFactory) {
        if let Some(last) = self.last_want {
            // The stride of playback, in frames per rendered frame: 1 at
            // normal speed, 2 at double speed (or with a 60 fps clip on a
            // 30 fps output), negative in reverse. Prefetching follows it, so
            // the frames decoded are the ones that will be wanted. A jump of
            // more than half the loop is a loop wrapping round, not a change
            // of direction: the stride is kept.
            self.step = next_stride(self.step, last, due, self.loop_span());
        }
        self.last_want = Some(due);
        self.start_workers(gpu);
        self.reclaim_uploads(gpu);

        let window = self.window(want);
        let frame = {
            let mut st = self.shared.state.lock().unwrap();
            // Drop what is no longer needed, publish what is. Frames no
            // longer wanted give their buffers back (still mapped).
            let dropped: Vec<i64> = st
                .ready
                .keys()
                .copied()
                .filter(|f| !window.contains(f))
                .collect();
            for f in dropped {
                if let Some(buffer) = st.ready.remove(&f) {
                    st.free.push(buffer);
                }
            }
            st.wanted = window;
            // Buffers the workers have made; taken with any ready frame.
            for (slot, buffer) in st.made.drain(..) {
                if self.uploads.len() <= slot {
                    self.uploads.resize(slot + 1, None);
                }
                self.uploads[slot] = Some(buffer);
            }
            self.shared.wake.notify_all();
            if self.shown == Some(want) {
                return;
            }
            st.ready
                .remove(&want)
                .map(|(slot, mapped)| (want, slot, mapped))
        };
        // The wanted frame is not decoded yet: keep showing the previous one
        // (never wait — every output on the machine would stall).
        // Its mapped memory came back with it (no worker can write there any
        // more): unmap, and have the GPU copy the frame into the texture
        // ahead of drawing it.
        let Some((index, slot, _mapped)) = frame else {
            return;
        };
        let buffer = self.uploads[slot].as_ref().expect("taken with its frame");
        buffer.unmap();
        let (blocks_w, blocks_h) = (self.track.width.div_ceil(4), self.track.height.div_ceil(4));
        let pitch = upload_pitch(self.format, self.track.width);
        let texture = &self.texture;
        let batch = gpu.record_upload(|encoder| {
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch),
                        rows_per_image: Some(blocks_h),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: blocks_w * 4,
                    height: blocks_h * 4,
                    depth_or_array_layers: 1,
                },
            );
        });
        self.in_flight.push((slot, batch));
        self.shown = Some(index);
    }
}

impl Content for HapContent {
    fn update(&mut self, t: f64, gpu: &TextureFactory) {
        let due = self.due(t);
        self.advance(due, due, gpu);
    }

    fn follow(&mut self, t: f64, frame: i64, gpu: &TextureFactory) {
        let due = self.due(t);
        self.advance(due, frame, gpu);
    }

    fn shown_frame(&self) -> Option<i64> {
        self.shown
    }

    fn due_frame(&self) -> Option<i64> {
        self.last_want
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

    fn follow(&mut self, t: f64, frame: i64, gpu: &TextureFactory) {
        if matches!(self.state, State::Loading(_)) {
            // Still being examined: the plain update finishes that.
            self.update(t, gpu);
            return;
        }
        if let Some(inner) = self.inner() {
            inner.follow(t, frame, gpu);
        }
    }

    fn shown_frame(&self) -> Option<i64> {
        match &self.state {
            State::Hap(c) => c.shown_frame(),
            State::Ffmpeg(c) => c.shown_frame(),
            _ => None,
        }
    }

    fn due_frame(&self) -> Option<i64> {
        match &self.state {
            State::Hap(c) => c.due_frame(),
            State::Ffmpeg(c) => c.due_frame(),
            _ => None,
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

    fn ready(&self) -> bool {
        match &self.state {
            State::Hap(c) => c.ready(),
            State::Ffmpeg(c) => c.ready(),
            State::Failed(_) => true,
            State::Loading(_) => false,
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
                    decode_frame(&packet, expect.section_type(), &mut out, 4)
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
            decode_frame(&packet, format.section_type(), &mut out, cores).unwrap();
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
    fn upload_rows_are_padded_to_the_copy_alignment() {
        // 4K DXT1: 960 blocks × 8 bytes, already a multiple of 256.
        assert_eq!(upload_pitch(HapFormat::Dxt1, 3840), 7680);
        // 10K DXT1 (9978 px): 2495 blocks × 8 = 19960 → 19968.
        assert_eq!(upload_pitch(HapFormat::Dxt1, 9978), 19968);
        // 68 px HAP Q: 17 blocks × 16 = 272 → 512.
        assert_eq!(upload_pitch(HapFormat::YCoCgDxt5, 68), 512);

        let src: Vec<u8> = (0..30).collect(); // three rows of 10
        let mut dst = vec![0xEEu8; 3 * 16];
        copy_rows(&src, &mut dst, 10, 16);
        for (r, row) in dst.chunks(16).enumerate() {
            assert_eq!(&row[..10], &src[r * 10..r * 10 + 10]);
            assert!(row[10..].iter().all(|&b| b == 0xEE), "padding untouched");
        }
        // Unpadded: one plain copy.
        let mut dst = vec![0u8; 30];
        copy_rows(&src, &mut dst, 10, 10);
        assert_eq!(dst, src);
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
            assert!(decode_frame(bad, HapFormat::Dxt1.section_type(), &mut out, 2).is_err());
        }
    }
}
