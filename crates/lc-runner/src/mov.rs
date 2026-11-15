//! Just enough of the QuickTime / MP4 file format to find every video frame
//! in a file: where each one is, and how big.
//!
//! With that table a player has random access to any frame — read `size`
//! bytes at `offset` — which is exactly what an intra-frame codec like HAP
//! needs: seeking, looping and reverse playback cost nothing.
//!
//! The format is a tree of *atoms* (`size, type, payload`). The frame table
//! of a track is spread over five of them:
//!
//! ```text
//! moov ─ trak ─ mdia ─┬ mdhd   timescale (ticks per second)
//!                     ├ hdlr   handler type: "vide" for video
//!                     └ minf ─ stbl ─┬ stsd   codec four-cc, width, height
//!                                    ├ stts   duration of each sample
//!                                    ├ stsz   size of each sample
//!                                    ├ stsc   how many samples per chunk
//!                                    └ stco / co64   file offset of each chunk
//! ```
//!
//! Files this reader cannot time exactly are refused, and then played
//! through ffmpeg instead: fragmented MP4 (`moof`), edit lists that do more
//! than map the whole track, and variable frame rates.
//!
//! Everything read here comes from a file the operator dropped in: every
//! count is checked against the size of its table before anything is
//! allocated, so a damaged or hostile file is an error, never a crash.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, Context as _};

/// The first video track of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoTrack {
    /// Sample-entry four-cc, e.g. `Hap1`, `avc1`.
    pub codec: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction: `timescale / sample duration`.
    pub fps_num: u32,
    pub fps_den: u32,
    /// (file offset, size) of every frame, in presentation order.
    pub frames: Vec<(u64, u32)>,
}

impl VideoTrack {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den as f64
    }

    pub fn codec_str(&self) -> String {
        String::from_utf8_lossy(&self.codec).into_owned()
    }
}

/// Read the video track of a QuickTime/MP4 file. Errors for anything else
/// (including valid files this reader does not cover); callers then fall
/// back to ffmpeg.
pub fn read_video_track(path: &str) -> anyhow::Result<VideoTrack> {
    let mut file = File::open(path).with_context(|| format!("opening '{path}'"))?;
    let len = file.metadata()?.len();
    // Find `moov` among the top-level atoms (it may be at the end) and read
    // it whole: it holds only tables, typically kilobytes to a few MB.
    let mut pos = 0;
    while pos < len {
        let (header, size, kind) = atom_header_at(&mut file, pos, len)?;
        if &kind == b"moov" {
            let body_len = usize::try_from(size - header).context("moov too large")?;
            if body_len > 512 << 20 {
                bail!("moov atom of {body_len} bytes is implausible");
            }
            let mut moov = vec![0u8; body_len];
            file.seek(SeekFrom::Start(pos + header))?;
            file.read_exact(&mut moov)?;
            return parse_moov(&moov);
        }
        if &kind == b"moof" {
            bail!("fragmented MP4 is not supported");
        }
        pos += size;
    }
    bail!("no moov atom: not a QuickTime/MP4 file")
}

/// Header of the atom at `pos` in the file: (header length, total size, type).
fn atom_header_at(file: &mut File, pos: u64, end: u64) -> anyhow::Result<(u64, u64, [u8; 4])> {
    let mut h = [0u8; 16];
    file.seek(SeekFrom::Start(pos))?;
    let n = file.read(&mut h)?;
    if n < 8 {
        bail!("truncated atom header at {pos}");
    }
    let size32 = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
    let kind: [u8; 4] = h[4..8].try_into().unwrap();
    let (header, size) = match size32 {
        0 => (8, end - pos), // extends to the end of the file
        1 => {
            if n < 16 {
                bail!("truncated 64-bit atom header at {pos}");
            }
            (16, u64::from_be_bytes(h[8..16].try_into().unwrap()))
        }
        s => (8, s),
    };
    if size < header || pos.checked_add(size).is_none_or(|e| e > end) {
        bail!(
            "atom '{}' at {pos} has an invalid size",
            String::from_utf8_lossy(&kind)
        );
    }
    Ok((header, size, kind))
}

/// Iterate the child atoms of an in-memory atom body: (type, body).
fn children(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if data.len() < 8 {
            return None;
        }
        let size32 = u32::from_be_bytes(data[0..4].try_into().ok()?) as usize;
        let kind: [u8; 4] = data[4..8].try_into().ok()?;
        let (header, size) = match size32 {
            0 => (8, data.len()),
            1 => {
                let big = u64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
                (16, usize::try_from(big).ok()?)
            }
            s => (8, s),
        };
        if size < header || size > data.len() {
            return None; // malformed: stop rather than misread
        }
        let body = &data[header..size];
        data = &data[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be32(data: &[u8], at: usize) -> anyhow::Result<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .context("table truncated")
}

fn be64(data: &[u8], at: usize) -> anyhow::Result<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .context("table truncated")
}

fn parse_moov(moov: &[u8]) -> anyhow::Result<VideoTrack> {
    for (kind, trak) in children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        // hdlr: version/flags(4) pre_defined(4) handler_type(4)
        let is_video = child(mdia, b"hdlr").and_then(|h| h.get(8..12)) == Some(b"vide");
        if !is_video {
            continue;
        }
        check_edit_list(trak)?;
        return parse_video_trak(mdia).context("reading the video track");
    }
    bail!("no video track")
}

/// Number of entries of a table atom (count at `count_at`, entries of
/// `entry` bytes from `count_at + 4`), checked against the atom's size.
fn table_count(atom: &[u8], count_at: usize, entry: usize) -> anyhow::Result<usize> {
    let count = be32(atom, count_at)? as usize;
    let room = atom.len().saturating_sub(count_at + 4) / entry;
    if count > room {
        bail!("table claims {count} entries but has room for {room}");
    }
    Ok(count)
}

/// An edit list that only maps the whole track from its start is harmless;
/// anything else (an initial delay, a trimmed start, several segments)
/// changes which frame plays when, and is left to ffmpeg.
fn check_edit_list(trak: &[u8]) -> anyhow::Result<()> {
    let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) else {
        return Ok(());
    };
    let version = elst.first().copied().unwrap_or(0);
    let count = table_count(elst, 4, if version == 1 { 20 } else { 12 })?;
    if count == 0 {
        return Ok(());
    }
    let media_time = if version == 1 {
        be64(elst, 8 + 8)? as i64
    } else {
        be32(elst, 8 + 4)? as i32 as i64
    };
    if count > 1 || media_time != 0 {
        bail!("edit list ({count} entries, starting at media time {media_time}) not supported");
    }
    Ok(())
}

fn parse_video_trak(mdia: &[u8]) -> anyhow::Result<VideoTrack> {
    // mdhd: version 0 → timescale at 12; version 1 → at 20.
    let mdhd = child(mdia, b"mdhd").context("no mdhd")?;
    let timescale = match mdhd.first() {
        Some(1) => be32(mdhd, 20)?,
        _ => be32(mdhd, 12)?,
    };
    let stbl = child(mdia, b"minf")
        .and_then(|m| child(m, b"stbl"))
        .context("no sample table")?;

    // stsd: version/flags(4) count(4), then the first sample entry:
    // size(4) four-cc(4) reserved(6) ref(2) … width(2) at 32, height(2) at 34.
    let stsd = child(stbl, b"stsd").context("no stsd")?;
    let entry = stsd.get(8..).context("empty stsd")?;
    let codec: [u8; 4] = entry
        .get(4..8)
        .context("stsd truncated")?
        .try_into()
        .unwrap();
    let width = u16::from_be_bytes(
        entry
            .get(32..34)
            .context("stsd truncated")?
            .try_into()
            .unwrap(),
    );
    let height = u16::from_be_bytes(
        entry
            .get(34..36)
            .context("stsd truncated")?
            .try_into()
            .unwrap(),
    );

    // stts: the frame duration. Take the most common sample delta (files of
    // constant rate have exactly one entry).
    let stts = child(stbl, b"stts").context("no stts")?;
    let mut delta = 0;
    let mut most = 0;
    let mut others = 0u64;
    for i in 0..table_count(stts, 4, 8)? {
        let (count, d) = (be32(stts, 8 + 8 * i)?, be32(stts, 12 + 8 * i)?);
        if count > most {
            others += most as u64;
            (most, delta) = (count, d);
        } else {
            others += count as u64;
        }
    }
    if timescale == 0 || delta == 0 {
        bail!("no usable frame rate");
    }
    // A single odd frame (often the last) is tolerated; a variable frame
    // rate is not: frame N would not be at N / fps.
    if others > 1 {
        bail!("variable frame rate");
    }

    // stsz: one size for all samples, or a table.
    // stco / co64: chunk offsets.
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..table_count(stco, 4, 4)?)
            .map(|i| be32(stco, 8 + 4 * i).map(u64::from))
            .collect::<anyhow::Result<_>>()?
    } else if let Some(co64) = child(stbl, b"co64") {
        (0..table_count(co64, 4, 8)?)
            .map(|i| be64(co64, 8 + 8 * i))
            .collect::<anyhow::Result<_>>()?
    } else {
        bail!("no chunk offsets");
    };

    // stsc: runs of (first chunk, samples per chunk); a run lasts until the
    // next run's first chunk.
    let stsc = child(stbl, b"stsc").context("no stsc")?;
    let runs: Vec<(u32, u32)> = (0..table_count(stsc, 4, 12)?)
        .map(|i| Ok((be32(stsc, 8 + 12 * i)?, be32(stsc, 12 + 12 * i)?)))
        .collect::<anyhow::Result<_>>()?;
    // How many samples the chunks can hold at most: bounds the sample count.
    let mut capacity = 0u64;
    for (ri, &(first, per_chunk)) in runs.iter().enumerate() {
        let last = runs
            .get(ri + 1)
            .map_or(offsets.len() as u64 + 1, |r| r.0 as u64);
        capacity += last.saturating_sub(first as u64) * per_chunk as u64;
    }

    // stsz: one size for all samples, or a table.
    let stsz = child(stbl, b"stsz").context("no stsz")?;
    let uniform = be32(stsz, 4)?;
    let count = if uniform != 0 {
        be32(stsz, 8)? as usize
    } else {
        table_count(stsz, 8, 4)?
    };
    if count as u64 > capacity {
        bail!("{count} frames claimed, but the chunks hold at most {capacity}");
    }
    let sizes: Vec<u32> = if uniform != 0 {
        vec![uniform; count]
    } else {
        (0..count)
            .map(|i| be32(stsz, 12 + 4 * i))
            .collect::<anyhow::Result<_>>()?
    };

    let mut frames = Vec::with_capacity(count);
    let mut sample = 0usize;
    for (ri, &(first, per_chunk)) in runs.iter().enumerate() {
        let last = runs.get(ri + 1).map_or(offsets.len() as u32 + 1, |r| r.0);
        for chunk in first..last {
            let index = (chunk as usize)
                .checked_sub(1)
                .context("chunk numbers start at 1")?;
            let mut at = *offsets.get(index).context("chunk index out of range")?;
            for _ in 0..per_chunk {
                let Some(&size) = sizes.get(sample) else {
                    break;
                };
                frames.push((at, size));
                at = at
                    .checked_add(size as u64)
                    .context("frame beyond any file")?;
                sample += 1;
            }
        }
    }
    if frames.len() != count {
        bail!(
            "sample table is inconsistent ({} of {count} frames located)",
            frames.len()
        );
    }
    Ok(VideoTrack {
        codec,
        width: width as u32,
        height: height as u32,
        fps_num: timescale,
        fps_den: delta,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an atom.
    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    fn be(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    #[test]
    fn locates_frames_across_chunks() {
        // 5 frames of sizes 10..14 in chunks of 2, 2, 1 at offsets 100, 200, 300.
        let mut entry = vec![0u8; 78];
        entry[0..4].copy_from_slice(&78u32.to_be_bytes());
        entry[4..8].copy_from_slice(b"Hap1");
        entry[32..34].copy_from_slice(&1920u16.to_be_bytes());
        entry[34..36].copy_from_slice(&1080u16.to_be_bytes());
        let stsd = [be(&[0, 1]), entry].concat();
        let stbl = [
            atom(b"stsd", &stsd),
            atom(b"stts", &be(&[0, 1, 5, 1001])),
            atom(b"stsz", &be(&[0, 0, 5, 10, 11, 12, 13, 14])),
            atom(b"stsc", &be(&[0, 2, 1, 2, 1, 3, 1, 1])),
            atom(b"stco", &be(&[0, 3, 100, 200, 300])),
        ]
        .concat();
        let mdia = [
            atom(b"mdhd", &be(&[0, 0, 0, 60000, 0, 0])),
            atom(b"hdlr", &[be(&[0, 0]), b"vide".to_vec()].concat()),
            atom(b"minf", &atom(b"stbl", &stbl)),
        ]
        .concat();
        let moov = atom(b"trak", &atom(b"mdia", &mdia));

        let track = parse_moov(&moov).unwrap();
        assert_eq!(track.codec_str(), "Hap1");
        assert_eq!((track.width, track.height), (1920, 1080));
        assert_eq!((track.fps_num, track.fps_den), (60000, 1001));
        assert_eq!(
            track.frames,
            vec![(100, 10), (110, 11), (200, 12), (212, 13), (300, 14)]
        );
    }

    #[test]
    fn hostile_tables_are_errors_not_crashes() {
        // A sample table claiming four billion frames (the reviewer's crash).
        let stbl = |stsz: Vec<u8>, stsc: Vec<u8>| {
            let mut entry = vec![0u8; 78];
            entry[4..8].copy_from_slice(b"Hap1");
            let stsd = [be(&[0, 1]), entry].concat();
            [
                atom(b"stsd", &stsd),
                atom(b"stts", &be(&[0, 1, 5, 1])),
                atom(b"stsz", &stsz),
                atom(b"stsc", &stsc),
                atom(b"stco", &be(&[0, 1, 100])),
            ]
            .concat()
        };
        let moov = |stbl: Vec<u8>| {
            let mdia = [
                atom(b"mdhd", &be(&[0, 0, 0, 30, 0, 0])),
                atom(b"hdlr", &[be(&[0, 0]), b"vide".to_vec()].concat()),
                atom(b"minf", &atom(b"stbl", &stbl)),
            ]
            .concat();
            atom(b"trak", &atom(b"mdia", &mdia))
        };
        let huge = moov(stbl(be(&[0, 1000, 0xFFFF_FFF0]), be(&[0, 1, 1, 1, 1])));
        assert!(parse_moov(&huge).is_err());
        let huge_table = moov(stbl(be(&[0, 0, 0xFFFF_FFF0]), be(&[0, 1, 1, 1, 1])));
        assert!(parse_moov(&huge_table).is_err());
        let chunk_zero = moov(stbl(be(&[0, 10, 1]), be(&[0, 1, 0, 1, 1])));
        assert!(parse_moov(&chunk_zero).is_err());
    }

    #[test]
    fn rejects_files_without_a_video_track() {
        assert!(parse_moov(&atom(b"trak", &atom(b"mdia", b""))).is_err());
        assert!(parse_moov(b"garbage").is_err());
    }
}
