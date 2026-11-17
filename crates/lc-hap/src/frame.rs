//! HAP frames: section headers, and decoding a frame into its texture
//! blocks (DXT1/DXT5/BC7…), Snappy-compressed or not, whole or in chunks.
//!
//! Reference: the HAP specification (Vidvox, `HapVideoDRAFT.md`). A frame is
//! a section: a 4-byte header (24-bit little-endian length — or 0 followed by
//! a 32-bit length — then a type byte) and its payload. The type's high
//! nibble is how the texture is stored (0xA plain, 0xB Snappy, 0xC chunked,
//! with "decode instructions"), the low nibble the texture format.

use std::sync::OnceLock;

use anyhow::{bail, Context as _};

/// Called first on every thread that unpacks chunks: the Runner lowers
/// their priority below its render thread (see `set_thread_init`).
static THREAD_INIT: OnceLock<fn()> = OnceLock::new();

/// Have every chunk-unpacking thread call `init` first (once per process;
/// later calls are ignored).
pub fn set_thread_init(init: fn()) {
    let _ = THREAD_INIT.set(init);
}

/// One section header: (payload length, type byte, header length).
pub fn section(data: &[u8]) -> anyhow::Result<(usize, u8, usize)> {
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
    texture_type: u8,
    out: &mut [u8],
    threads: usize,
) -> anyhow::Result<()> {
    let (len, kind, header) = section(frame)?;
    let payload = &frame[header..header + len];
    if kind & 0x0F != texture_type {
        bail!(
            "frame has texture type {:#x}, expected {texture_type:#x}",
            kind & 0x0F
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
                    if let Some(init) = THREAD_INIT.get() {
                        init();
                    }
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

pub(crate) fn le32_table(body: &[u8]) -> Vec<u32> {
    body.chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

// ───────────────────────────── playback ─────────────────────────────
