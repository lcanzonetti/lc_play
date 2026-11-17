//! # lc-hap — HAP video in QuickTime files
//!
//! Shared by the Runner (which plays HAP: [`mov`] finds the frames,
//! [`frame`] unpacks them for the GPU) and the Asset Manager (which
//! re-chunks HAP files losslessly: [`rechunk`], writing with [`write`]).
//!
//! HAP stores each frame as GPU texture blocks (DXT1, DXT5, BC7…), usually
//! Snappy-compressed, optionally split into *chunks* compressed separately so
//! that a player can unpack one frame on several CPU cores at once.

pub mod frame;
pub mod mov;
pub mod rechunk;
pub mod write;

/// The texture layout of a single-texture HAP variant, by its four-cc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Texture {
    /// The texture-format nibble a frame carries (low nibble of its type).
    pub section_type: u8,
    /// Bytes per 4×4 block.
    pub block_bytes: usize,
    /// Whether the variant has alpha.
    pub alpha: bool,
}

impl Texture {
    /// `Hap1` (DXT1), `Hap5` (DXT5), `HapY` (YCoCg DXT5), `Hap7` (BC7),
    /// `HapA` (alpha only, RGTC1). `HapM` (HAP Q Alpha: two textures) is not
    /// a single-texture variant.
    pub fn of(codec: &[u8; 4]) -> Option<Texture> {
        let t = |section_type, block_bytes, alpha| Texture {
            section_type,
            block_bytes,
            alpha,
        };
        match codec {
            b"Hap1" => Some(t(0x0B, 8, false)),
            b"Hap5" => Some(t(0x0E, 16, true)),
            b"HapY" => Some(t(0x0F, 16, false)),
            b"Hap7" => Some(t(0x0C, 16, true)),
            b"HapA" => Some(t(0x01, 8, true)),
            _ => None,
        }
    }

    /// Bytes of texture in a frame of this size.
    pub fn len(&self, width: u32, height: u32) -> usize {
        width.div_ceil(4) as usize * height.div_ceil(4) as usize * self.block_bytes
    }
}
