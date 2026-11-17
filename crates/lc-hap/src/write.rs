//! Writing a QuickTime (`.mov`) file of video frames: the frames go into one
//! `mdat`, written as they come; the sample table (`moov`) at the end. Just
//! what a HAP file needs — one video track, constant frame rate, no audio —
//! in the 64-bit forms throughout, so files of any size and length work.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Context as _;

pub struct MovWriter {
    out: BufWriter<File>,
    /// Where the `mdat` box starts (its 64-bit size is patched at the end).
    mdat_at: u64,
    /// Bytes written so far: the next frame's offset.
    pos: u64,
    frames: Vec<(u64, u32)>,
    codec: [u8; 4],
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    depth: u16,
}

impl MovWriter {
    /// Start a file of `codec` frames, `width`×`height`, at
    /// `fps_num / fps_den` frames per second. `alpha` sets the depth field
    /// (32 rather than 24) that players read as "has alpha".
    pub fn create(
        path: &Path,
        codec: [u8; 4],
        (width, height): (u32, u32),
        (fps_num, fps_den): (u32, u32),
        alpha: bool,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            width > 0 && height > 0 && width <= 0xFFFF && height <= 0xFFFF,
            "frame size"
        );
        anyhow::ensure!(fps_num > 0 && fps_den > 0, "frame rate");
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        // ftyp: QuickTime.
        let ftyp = boxed(
            b"ftyp",
            &[b"qt  ".as_slice(), &[0, 0, 0, 0], b"qt  "].concat(),
        );
        out.write_all(&ftyp)?;
        let mdat_at = ftyp.len() as u64;
        // mdat with a 64-bit size, patched by `finish`.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(MovWriter {
            out,
            mdat_at,
            pos: mdat_at + 16,
            frames: Vec::new(),
            codec,
            width,
            height,
            fps_num,
            fps_den,
            depth: if alpha { 32 } else { 24 },
        })
    }

    /// Append one frame.
    pub fn write_frame(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let size = u32::try_from(data.len()).context("frame larger than 4 GB")?;
        self.out.write_all(data)?;
        self.frames.push((self.pos, size));
        self.pos += data.len() as u64;
        Ok(())
    }

    /// Write the sample table and close the file.
    pub fn finish(mut self) -> anyhow::Result<()> {
        let moov = self.moov();
        self.out.write_all(&moov)?;
        let mdat_size = self.pos - self.mdat_at;
        let mut file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(self.mdat_at + 8))?;
        file.write_all(&mdat_size.to_be_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    fn moov(&self) -> Vec<u8> {
        let n = self.frames.len() as u64;
        let duration = n * self.fps_den as u64; // in timescale (fps_num) units
        let matrix: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];
        let matrix: Vec<u8> = matrix.iter().flat_map(|v| v.to_be_bytes()).collect();

        let mut mvhd = vec![1, 0, 0, 0];
        mvhd.extend([0u8; 16]); // creation, modification
        mvhd.extend(self.fps_num.to_be_bytes());
        mvhd.extend(duration.to_be_bytes());
        mvhd.extend(0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend(0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend([0u8; 10]);
        mvhd.extend(&matrix);
        mvhd.extend([0u8; 24]);
        mvhd.extend(2u32.to_be_bytes()); // next track id

        let mut tkhd = vec![1, 0, 0, 0x0F]; // enabled, in movie, in preview
        tkhd.extend([0u8; 16]);
        tkhd.extend(1u32.to_be_bytes()); // track id
        tkhd.extend([0u8; 4]);
        tkhd.extend(duration.to_be_bytes());
        tkhd.extend([0u8; 8]);
        tkhd.extend([0u8; 8]); // layer, group, volume, reserved
        tkhd.extend(&matrix);
        tkhd.extend((self.width << 16).to_be_bytes());
        tkhd.extend((self.height << 16).to_be_bytes());

        let mut mdhd = vec![1, 0, 0, 0];
        mdhd.extend([0u8; 16]);
        mdhd.extend(self.fps_num.to_be_bytes());
        mdhd.extend(duration.to_be_bytes());
        mdhd.extend(0x55C4u16.to_be_bytes()); // language: und
        mdhd.extend([0u8; 2]);

        let mut hdlr = vec![0, 0, 0, 0];
        hdlr.extend(b"mhlr");
        hdlr.extend(b"vide");
        hdlr.extend([0u8; 12]);
        hdlr.extend(b"\x0cVideoHandler");

        let vmhd = boxed(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        let dref = boxed(
            b"dref",
            &[
                &[0u8, 0, 0, 0, 0, 0, 0, 1][..],
                &boxed(b"url ", &[0, 0, 0, 1]),
            ]
            .concat(),
        );
        let dinf = boxed(b"dinf", &dref);

        // The video sample entry (86 bytes).
        let mut entry = Vec::new();
        entry.extend([0u8; 6]);
        entry.extend(1u16.to_be_bytes()); // data reference index
        entry.extend([0u8; 4]); // version, revision
        entry.extend(b"lcpl"); // vendor
        entry.extend([0u8; 8]); // temporal, spatial quality
        entry.extend((self.width as u16).to_be_bytes());
        entry.extend((self.height as u16).to_be_bytes());
        entry.extend(0x0048_0000u32.to_be_bytes()); // 72 dpi
        entry.extend(0x0048_0000u32.to_be_bytes());
        entry.extend([0u8; 4]); // data size
        entry.extend(1u16.to_be_bytes()); // frames per sample
        let mut name = [0u8; 32];
        let label = b"Hap";
        name[0] = label.len() as u8;
        name[1..=label.len()].copy_from_slice(label);
        entry.extend(name);
        entry.extend(self.depth.to_be_bytes());
        entry.extend((-1i16).to_be_bytes()); // no colour table
        let stsd = boxed(
            b"stsd",
            &[&[0u8, 0, 0, 0, 0, 0, 0, 1][..], &boxed(&self.codec, &entry)].concat(),
        );
        let mut stts = vec![0, 0, 0, 0];
        stts.extend(1u32.to_be_bytes());
        stts.extend((n as u32).to_be_bytes());
        stts.extend(self.fps_den.to_be_bytes());
        let mut stsc = vec![0, 0, 0, 0];
        stsc.extend(1u32.to_be_bytes());
        stsc.extend([0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]); // 1 sample per chunk
        let mut stsz = vec![0, 0, 0, 0, 0, 0, 0, 0];
        stsz.extend((n as u32).to_be_bytes());
        let mut co64 = vec![0, 0, 0, 0];
        co64.extend((n as u32).to_be_bytes());
        for &(offset, size) in &self.frames {
            stsz.extend(size.to_be_bytes());
            co64.extend(offset.to_be_bytes());
        }
        let stbl = boxed(
            b"stbl",
            &[
                stsd,
                boxed(b"stts", &stts),
                boxed(b"stsc", &stsc),
                boxed(b"stsz", &stsz),
                boxed(b"co64", &co64),
            ]
            .concat(),
        );
        let minf = boxed(b"minf", &[vmhd, dinf, stbl].concat());
        let mdia = boxed(
            b"mdia",
            &[boxed(b"mdhd", &mdhd), boxed(b"hdlr", &hdlr), minf].concat(),
        );
        let trak = boxed(b"trak", &[boxed(b"tkhd", &tkhd), mdia].concat());
        boxed(b"moov", &[boxed(b"mvhd", &mvhd), trak].concat())
    }
}

/// A box (atom): 32-bit size, type, payload.
fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + payload.len());
    b.extend(((8 + payload.len()) as u32).to_be_bytes());
    b.extend(kind);
    b.extend(payload);
    b
}
