//! # lc-ooxml — PowerPoint `.pptx` files into an [`lc_deck::Deck`]
//!
//! Phase 1 of the native presentation renderer: reading the file. It
//! covers the package (ZIP and relationships), the master → layout →
//! slide inheritance of positions and backgrounds, theme colours and
//! fonts, shapes, pictures, groups, tables, charts and SmartArt as frames,
//! text with its directly stated formatting, embedded media, speaker notes,
//! transitions (Morph included) and the animation tree.
//!
//! ```no_run
//! let file = std::fs::File::open("deck.pptx")?;
//! let deck = lc_ooxml::read(std::io::BufReader::new(file))?;
//! println!("{} slides, {} steps", deck.slides.len(), deck.step_count());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Modules, bottom up: [`package`] (ZIP, parts, relationships, size
//! limits), [`xml`] (namespaces, Markup Compatibility), [`color`] (theme
//! colours and transforms), `shapes` (shape trees), `timing` (transitions
//! and animations), `deck` (putting it together).
//!
//! What a file contains that this reader skips is listed in
//! [`lc_deck::Deck::warnings`]; it never fails a whole deck over one
//! unsupported object. It does fail on a file that is not a presentation,
//! is not a valid ZIP, or exceeds the size limits in [`package`].

pub mod color;
mod deck;
pub mod package;
mod shapes;
mod timing;
pub mod xml;

use std::io::{Read, Seek};

pub use lc_deck as model;
pub use package::Package;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a readable ZIP package: {0}")]
    Zip(String),
    #[error("{part}: not well-formed XML: {message}")]
    Xml { part: String, message: String },
    #[error("missing part {0}")]
    Missing(String),
    #[error("{part}: larger than the {limit}-byte limit")]
    TooLarge { part: String, limit: u64 },
    #[error("not a presentation: {0}")]
    NotPresentation(String),
}

/// Reads a whole deck from a `.pptx` (or `.ppsx`, `.potx`, `.pptm`: same
/// structure; macros are never run or read).
pub fn read<R: Read + Seek>(reader: R) -> Result<lc_deck::Deck, Error> {
    let pkg = Package::open(reader)?;
    deck::build(&pkg)
}

/// Builds the deck of a package already opened (keep the package to read
/// its media afterwards with [`Package::read_part`]).
pub fn deck<R: Read + Seek>(pkg: &Package<R>) -> Result<lc_deck::Deck, Error> {
    deck::build(pkg)
}
