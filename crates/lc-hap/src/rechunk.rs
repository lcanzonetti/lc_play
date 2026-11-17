//! Lossless re-chunking of HAP files.
//!
//! The texture of every frame is kept bit for bit; only how it is stored
//! changes: split into `chunks` parts (so a player can unpack a frame on that
//! many CPU cores at once) and each part Snappy-compressed or stored plain.
//! Re-encoding with ffmpeg would instead decode to RGB and compress the
//! texture again — a generation loss each time, and much slower.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context as _};

use crate::frame::{decode_frame, section};
use crate::mov::read_video_track;
use crate::write::MovWriter;
use crate::Texture;

/// How a HAP file's frames are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub chunks: usize,
    /// Whether any part is Snappy-compressed.
    pub snappy: bool,
}

/// The storage of one frame.
pub fn frame_layout(frame: &[u8]) -> anyhow::Result<Layout> {
    let (len, kind, header) = section(frame)?;
    let payload = &frame[header..header + len];
    Ok(match kind >> 4 {
        0xA => Layout {
            chunks: 1,
            snappy: false,
        },
        0xB => Layout {
            chunks: 1,
            snappy: true,
        },
        0xC => {
            let (ilen, ikind, ih) = section(payload)?;
            if ikind != 0x01 {
                bail!("chunked frame without decode instructions");
            }
            let mut rest = &payload[ih..ih + ilen];
            let mut compressors: &[u8] = &[];
            while !rest.is_empty() {
                let (l, k, h) = section(rest)?;
                if k == 0x02 {
                    compressors = &rest[h..h + l];
                }
                rest = &rest[h + l..];
            }
            Layout {
                chunks: compressors.len().max(1),
                snappy: compressors.contains(&0x0B),
            }
        }
        other => bail!("unknown HAP storage {other:#x}"),
    })
}

/// The storage of a HAP file's first frame, and its codec.
pub fn file_layout(path: &Path) -> anyhow::Result<([u8; 4], Layout)> {
    let track = read_video_track(&path.to_string_lossy())?;
    let &(offset, size) = track.frames.first().context("no frames")?;
    let mut file = File::open(path)?;
    let mut frame = vec![0; size as usize];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut frame)?;
    Ok((track.codec, frame_layout(&frame)?))
}

/// A section: header (3-byte length, or 0 and a 4-byte one) and payload.
fn push_section(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    let len = payload.len();
    if len < (1 << 24) && len > 0 {
        out.extend(&(len as u32).to_le_bytes()[..3]);
        out.push(kind);
    } else {
        out.extend([0, 0, 0, kind]);
        out.extend((len as u32).to_le_bytes());
    }
    out.extend(payload);
}

/// Encode a frame's texture as HAP, in `chunks` parts (block-aligned),
/// each Snappy-compressed if `snappy` and if that makes it smaller.
pub fn encode_frame(texture: &[u8], tex: Texture, chunks: usize, snappy: bool) -> Vec<u8> {
    let blocks = texture.len() / tex.block_bytes;
    let chunks = chunks.clamp(1, 64).min(blocks.max(1));
    // Exactly `chunks` parts, as even as blocks allow: the first
    // `blocks % chunks` parts get one block more. (Rounding the part size up
    // instead could give fewer parts than asked, and a file that never
    // matches its rule.)
    let (base, extra) = (blocks / chunks, blocks % chunks);
    let mut parts: Vec<&[u8]> = Vec::with_capacity(chunks);
    let mut rest = texture;
    for i in 0..chunks {
        let n = (base + usize::from(i < extra)) * tex.block_bytes;
        let n = if i + 1 == chunks {
            rest.len()
        } else {
            n.min(rest.len())
        };
        let (part, tail) = rest.split_at(n);
        parts.push(part);
        rest = tail;
    }
    // Compress the parts in parallel: this is the slow step.
    let packed: Vec<(u8, Vec<u8>)> = std::thread::scope(|s| {
        let jobs: Vec<_> = parts
            .iter()
            .map(|part| {
                s.spawn(move || {
                    if snappy {
                        if let Ok(c) = snap::raw::Encoder::new().compress_vec(part) {
                            if c.len() < part.len() {
                                return (0x0B, c);
                            }
                        }
                    }
                    (0x0A, part.to_vec())
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().expect("compress"))
            .collect()
    });

    let mut out = Vec::with_capacity(texture.len() / 2 + 64);
    if packed.len() == 1 {
        let (storage, data) = &packed[0];
        push_section(&mut out, (storage << 4) | tex.section_type, data);
        return out;
    }
    let mut instructions = Vec::new();
    let compressors: Vec<u8> = packed.iter().map(|(c, _)| *c).collect();
    push_section(&mut instructions, 0x02, &compressors);
    let sizes: Vec<u8> = packed
        .iter()
        .flat_map(|(_, d)| (d.len() as u32).to_le_bytes())
        .collect();
    push_section(&mut instructions, 0x03, &sizes);
    let mut payload = Vec::new();
    push_section(&mut payload, 0x01, &instructions);
    for (_, d) in &packed {
        payload.extend(d);
    }
    push_section(&mut out, 0xC0 | tex.section_type, &payload);
    out
}

/// Re-chunk the HAP file `src` into `dst` (see the module docs).
/// `progress` is called with the fraction done after each frame.
pub fn rechunk(
    src: &Path,
    dst: &Path,
    chunks: usize,
    snappy: bool,
    progress: &mut dyn FnMut(f64),
) -> anyhow::Result<()> {
    let track = read_video_track(&src.to_string_lossy())?;
    let tex = Texture::of(&track.codec).with_context(|| {
        format!(
            "'{}' is not a single-texture HAP file (codec {})",
            src.display(),
            track.codec_str()
        )
    })?;
    let len = tex.len(track.width, track.height);
    let mut input = File::open(src)?;
    let mut writer = MovWriter::create(
        dst,
        track.codec,
        (track.width, track.height),
        (track.fps_num, track.fps_den),
        tex.alpha,
    )?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let (mut packet, mut texture) = (Vec::new(), vec![0u8; len]);
    let total = track.frames.len().max(1);
    for (i, &(offset, size)) in track.frames.iter().enumerate() {
        packet.resize(size as usize, 0);
        input.seek(SeekFrom::Start(offset))?;
        input.read_exact(&mut packet)?;
        decode_frame(&packet, tex.section_type, &mut texture, threads)
            .with_context(|| format!("frame {i}"))?;
        writer.write_frame(&encode_frame(&texture, tex, chunks, snappy))?;
        progress((i + 1) as f64 / total as f64);
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tex() -> Texture {
        Texture::of(b"HapY").unwrap()
    }

    #[test]
    fn encoded_frames_decode_to_the_same_texture() {
        // A texture with some structure (compressible) and some noise.
        let len = tex().len(64, 32);
        let texture: Vec<u8> = (0..len)
            .map(|i| ((i * 7) % 251) as u8 ^ (i / 97) as u8)
            .collect();
        for (chunks, snappy) in [(1, false), (1, true), (4, true), (7, false), (64, true)] {
            let frame = encode_frame(&texture, tex(), chunks, snappy);
            let mut out = vec![0u8; len];
            decode_frame(&frame, tex().section_type, &mut out, 3).unwrap();
            assert_eq!(out, texture, "chunks {chunks} snappy {snappy}");
            let layout = frame_layout(&frame).unwrap();
            assert_eq!(layout.chunks, chunks.min(len / 16), "chunks {chunks}");
        }
    }

    #[test]
    fn a_whole_file_is_rechunked_losslessly() {
        // Needs ffmpeg with the HAP encoder to make a source file.
        let dir = std::env::temp_dir().join(format!("lc-hap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mov");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=128x72:rate=25",
                "-t",
                "1",
            ])
            .args(["-c:v", "hap", "-format", "hap_q"])
            .arg(&src)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("no ffmpeg with HAP: skipped");
            return;
        }
        let dst = dir.join("dst.mov");
        let mut last = 0.0;
        rechunk(&src, &dst, 8, true, &mut |p| last = p).unwrap();
        assert_eq!(last, 1.0);

        let (a, b) = (
            read_video_track(&src.to_string_lossy()).unwrap(),
            read_video_track(&dst.to_string_lossy()).unwrap(),
        );
        assert_eq!((b.codec, b.width, b.height), (a.codec, a.width, a.height));
        assert_eq!(
            (b.fps_num as u64 * a.fps_den as u64),
            (a.fps_num as u64 * b.fps_den as u64)
        );
        assert_eq!(b.frames.len(), a.frames.len());
        let read = |path: &Path, (offset, size): (u64, u32)| {
            let mut f = File::open(path).unwrap();
            let mut v = vec![0; size as usize];
            f.seek(SeekFrom::Start(offset)).unwrap();
            f.read_exact(&mut v).unwrap();
            v
        };
        let len = tex().len(a.width, a.height);
        for i in 0..a.frames.len() {
            let (mut ta, mut tb) = (vec![0; len], vec![0; len]);
            decode_frame(&read(&src, a.frames[i]), tex().section_type, &mut ta, 2).unwrap();
            let fb = read(&dst, b.frames[i]);
            decode_frame(&fb, tex().section_type, &mut tb, 2).unwrap();
            assert_eq!(ta, tb, "frame {i} is bit-identical");
            assert_eq!(frame_layout(&fb).unwrap().chunks, 8);
        }
        assert_eq!(
            file_layout(&dst).unwrap().1,
            Layout {
                chunks: 8,
                snappy: true
            }
        );
        // And ffmpeg reads the result.
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&dst)
            .args(["-f", "null", "-"])
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg decodes the re-chunked file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
