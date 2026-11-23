//! The PCM sidecars the Asset Manager makes: WAV (RF64 above 4 GB),
//! 32-bit float, interleaved, at [`lc_proto::audio::SAMPLE_RATE`].
//!
//! A file is memory-mapped, not read: the audio callback takes samples
//! straight from the mapping, and the control thread touches the pages
//! about to be played so they are in memory before the callback gets
//! there (a page fault on a resident page is cheap; one that reads the
//! disk is what must not happen in the callback). Nothing is decoded at
//! show time.

use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context as _};

/// WAVE_FORMAT_IEEE_FLOAT and WAVE_FORMAT_EXTENSIBLE.
const FORMAT_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// The first two bytes of KSDATAFORMAT_SUBTYPE_IEEE_FLOAT (the rest is the
/// common base GUID).
const SUBTYPE_FLOAT: u16 = 3;

/// Where the samples are in a WAV file, and their layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub data_offset: u64,
    pub data_len: u64,
    pub channels: u32,
    pub sample_rate: u32,
}

/// Read a WAV or RF64 header. Only 32-bit float is accepted: that is what
/// the Asset Manager writes.
pub fn parse_header(bytes: &[u8]) -> anyhow::Result<Layout> {
    let u16_at = |at: usize| -> anyhow::Result<u16> {
        Ok(u16::from_le_bytes(
            bytes.get(at..at + 2).context("truncated")?.try_into()?,
        ))
    };
    let u32_at = |at: usize| -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(
            bytes.get(at..at + 4).context("truncated")?.try_into()?,
        ))
    };
    let u64_at = |at: usize| -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(
            bytes.get(at..at + 8).context("truncated")?.try_into()?,
        ))
    };
    let magic = bytes.get(0..4).context("not a WAV file")?;
    let rf64 = match magic {
        b"RIFF" => false,
        b"RF64" | b"BW64" => true,
        _ => bail!("not a WAV file"),
    };
    if bytes.get(8..12) != Some(b"WAVE") {
        bail!("not a WAV file");
    }
    let mut at = 12usize;
    let mut ds64_data: Option<u64> = None;
    let mut format: Option<(u32, u32)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(at + 4)? as u64;
        let body = at + 8;
        match id {
            b"ds64" => ds64_data = Some(u64_at(body + 8)?),
            b"fmt " => {
                let mut tag = u16_at(body)?;
                let channels = u16_at(body + 2)? as u32;
                let rate = u32_at(body + 4)?;
                let bits = u16_at(body + 14)?;
                if tag == FORMAT_EXTENSIBLE {
                    tag = u16_at(body + 24)?;
                    if tag != SUBTYPE_FLOAT {
                        bail!("not 32-bit float samples");
                    }
                }
                if tag != FORMAT_FLOAT || bits != 32 {
                    bail!("not 32-bit float samples");
                }
                if channels == 0 {
                    bail!("no channels");
                }
                format = Some((channels, rate));
            }
            b"data" => {
                let (channels, sample_rate) = format.context("data before format")?;
                let data_len = match (rf64, len) {
                    (true, 0xFFFF_FFFF) => ds64_data.context("RF64 without a ds64 chunk")?,
                    _ => len,
                };
                return Ok(Layout {
                    data_offset: body as u64,
                    data_len,
                    channels,
                    sample_rate,
                });
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body
            .checked_add(usize::try_from(len + (len & 1)).context("chunk too long")?)
            .context("chunk too long")?;
    }
    bail!("no data in the WAV file")
}

/// A memory-mapped PCM file.
pub struct PcmFile {
    map: memmap2::Mmap,
    layout: Layout,
    frames: u64,
}

impl PcmFile {
    pub fn open(path: &Path) -> anyhow::Result<PcmFile> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the mapping is read-only, and the Runner's cache never
        // writes into a file that is in use: new versions are written
        // under another name and renamed into place.
        let map = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("mapping {}", path.display()))?;
        let head = &map[..map.len().min(64 * 1024)];
        let layout = parse_header(head).with_context(|| path.display().to_string())?;
        if layout.sample_rate != lc_proto::audio::SAMPLE_RATE {
            bail!(
                "{}: {} Hz, expected {} Hz",
                path.display(),
                layout.sample_rate,
                lc_proto::audio::SAMPLE_RATE
            );
        }
        if layout.data_offset % 4 != 0 {
            bail!("{}: samples not aligned", path.display());
        }
        // A file cut short (still being written, a full disk) plays what
        // it has.
        let available = (map.len() as u64).saturating_sub(layout.data_offset);
        let bytes = layout.data_len.min(available);
        let frames = bytes / (4 * layout.channels as u64);
        Ok(PcmFile {
            map,
            layout,
            frames,
        })
    }

    pub fn channels(&self) -> u32 {
        self.layout.channels
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// All samples, interleaved.
    pub fn samples(&self) -> &[f32] {
        let start = self.layout.data_offset as usize;
        let end = start + (self.frames * self.layout.channels as u64 * 4) as usize;
        // Aligned (checked when opened; the mapping starts on a page) and
        // a whole number of samples.
        bytemuck::cast_slice(&self.map[start..end])
    }

    /// Bring frames `from .. from + count` into memory (reading one byte
    /// per page), so the audio callback finds them there.
    pub fn prefetch(&self, from: u64, count: u64) {
        let ch = self.layout.channels as u64;
        let start = self.layout.data_offset + from.min(self.frames) * ch * 4;
        let end = self.layout.data_offset + (from.saturating_add(count)).min(self.frames) * ch * 4;
        let mut at = start - start % 4096;
        let mut sum = 0u8;
        while at < end {
            // SAFETY: `at` is inside the mapping (end ≤ its length).
            sum = sum.wrapping_add(unsafe {
                std::ptr::read_volatile(self.map.as_ptr().add(at as usize))
            });
            at += 4096;
        }
        std::hint::black_box(sum);
    }
}

/// Write a 32-bit float WAV file (tests, and the test tone).
pub fn write_wav(path: &Path, channels: u16, rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let data_len = (samples.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&FORMAT_FLOAT.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    out.extend_from_slice(&(channels * 4).to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_maps_back() {
        let path = std::env::temp_dir().join(format!("lcplay-pcm-{}.wav", std::process::id()));
        let samples: Vec<f32> = (0..96).map(|i| i as f32).collect();
        write_wav(&path, 2, 48_000, &samples).unwrap();
        let f = PcmFile::open(&path).unwrap();
        assert_eq!((f.channels(), f.frames()), (2, 48));
        assert_eq!(f.samples()[..4], [0.0, 1.0, 2.0, 3.0]);
        f.prefetch(0, 1000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rf64_takes_its_length_from_ds64() {
        let mut h = Vec::new();
        h.extend_from_slice(b"RF64");
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        h.extend_from_slice(b"WAVEds64");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes()); // RIFF size
        h.extend_from_slice(&(6_000_000_000u64).to_le_bytes()); // data size
        h.extend_from_slice(&0u64.to_le_bytes()); // sample count
        h.extend_from_slice(&0u32.to_le_bytes()); // table length
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&40u32.to_le_bytes());
        h.extend_from_slice(&FORMAT_EXTENSIBLE.to_le_bytes());
        h.extend_from_slice(&8u16.to_le_bytes());
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&(48_000u32 * 32).to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(&22u16.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&SUBTYPE_FLOAT.to_le_bytes());
        h.extend_from_slice(&[0; 14]);
        h.extend_from_slice(b"data");
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        let l = parse_header(&h).unwrap();
        assert_eq!(l.data_len, 6_000_000_000);
        assert_eq!(l.channels, 8);
        assert_eq!(l.data_offset as usize, h.len());
    }

    #[test]
    fn ffmpegs_multichannel_files_are_read() {
        // What the Asset Manager makes: more than two channels is
        // WAVE_FORMAT_EXTENSIBLE. Skipped where ffmpeg is not installed.
        let path = std::env::temp_dir().join(format!("lcplay-pcm-6ch-{}.wav", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.5",
            ])
            .args(["-af", "pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0"])
            .args([
                "-c:a",
                "pcm_f32le",
                "-ar",
                "48000",
                "-fflags",
                "+bitexact",
                "-rf64",
                "auto",
                "-f",
                "wav",
            ])
            .arg(&path)
            .output();
        if !made.is_ok_and(|o| o.status.success()) {
            eprintln!("ffmpeg unavailable: skipped");
            return;
        }
        let f = PcmFile::open(&path).unwrap();
        assert_eq!((f.channels(), f.frames()), (6, 24_000));
        // lavfi's sine is at 1/8 of full scale.
        assert!(f.samples().iter().any(|&s| s > 0.12));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn integer_samples_are_refused() {
        let path = std::env::temp_dir().join(format!("lcplay-pcm-int-{}.wav", std::process::id()));
        write_wav(&path, 1, 48_000, &[0.0; 4]).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[20] = 1; // PCM integer
        assert!(parse_header(&bytes).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
