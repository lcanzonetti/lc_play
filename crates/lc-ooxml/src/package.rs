//! The package layer: a ZIP archive of *parts* tied together by
//! *relationships* (Open Packaging Conventions, ECMA-376 Part 2).
//!
//! Everything above this module works with part names
//! (`ppt/slides/slide1.xml`) and relationship ids (`rId3`); this module is
//! the only one that knows about ZIP entries.
//!
//! Design choices:
//!
//! * **XML parts are read up front, binary parts on demand.** A deck's XML
//!   is small (typically a few MB) and is needed whole to build the model;
//!   its media can be hundreds of MB and is only needed by whoever renders
//!   or optimises it, so [`Package::read_part`] reads one when asked.
//! * **Hard size limits.** A ZIP entry can claim one size and inflate to
//!   another, and a small file can inflate to gigabytes ("zip bomb"). Every
//!   read is capped by counting the bytes actually produced, never by
//!   trusting the header; an XML part over [`MAX_XML_PART`] or a package
//!   whose XML totals more than [`MAX_XML_TOTAL`] is refused.
//! * **Part names are matched without regard to case**, as the conventions
//!   require, and without a leading `/`.

use std::collections::HashMap;
use std::io::{Read, Seek};

use crate::Error;

/// Largest single XML part accepted (bytes, inflated).
pub const MAX_XML_PART: u64 = 64 << 20;
/// Largest total of all XML parts (bytes, inflated).
pub const MAX_XML_TOTAL: u64 = 512 << 20;
/// Largest number of entries in the archive.
pub const MAX_ENTRIES: usize = 100_000;

/// Relationship types, matched by their last path segment so that the
/// Transitional (`http://schemas.openxmlformats.org/officeDocument/2006/
/// relationships/slide`) and Strict (`http://purl.oclc.org/ooxml/
/// officeDocument/relationships/slide`) spellings both match.
pub mod rel {
    pub const OFFICE_DOCUMENT: &str = "officeDocument";
    pub const SLIDE: &str = "slide";
    pub const SLIDE_LAYOUT: &str = "slideLayout";
    pub const SLIDE_MASTER: &str = "slideMaster";
    pub const THEME: &str = "theme";
    pub const NOTES_SLIDE: &str = "notesSlide";
}

/// One relationship of a part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    /// The full type URI as written.
    pub kind: String,
    pub target: Target,
}

impl Relationship {
    /// Whether the type's last segment is `name` (see [`rel`]).
    pub fn is(&self, name: &str) -> bool {
        self.kind.rsplit('/').next() == Some(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A part in this package, by its resolved part name.
    Part(String),
    /// Something outside the package (a linked video, a web address), as
    /// written.
    External(String),
}

/// An opened `.pptx` (or any OPC package).
pub struct Package<R> {
    archive: zip::ZipArchive<R>,
    /// Lower-cased part name → entry name as stored.
    names: HashMap<String, String>,
    /// XML parts (`.xml`, `.rels`), by lower-cased part name.
    xml: HashMap<String, String>,
}

impl<R: Read + Seek> Package<R> {
    pub fn open(reader: R) -> Result<Self, Error> {
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| Error::Zip(e.to_string()))?;
        if archive.len() > MAX_ENTRIES {
            return Err(Error::Zip(format!(
                "{} entries; at most {MAX_ENTRIES} accepted",
                archive.len()
            )));
        }
        let mut names = HashMap::new();
        let mut xml = HashMap::new();
        let mut total = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| Error::Zip(e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let stored = entry
                .name()
                .map_err(|e| Error::Zip(e.to_string()))?
                .to_string();
            let key = normalise(&stored);
            if key.ends_with(".xml") || key.ends_with(".rels") {
                let bytes = read_capped(&mut entry, MAX_XML_PART, &stored)?;
                total += bytes.len() as u64;
                if total > MAX_XML_TOTAL {
                    return Err(Error::TooLarge {
                        part: "(all XML parts together)".into(),
                        limit: MAX_XML_TOTAL,
                    });
                }
                let text = decode_xml_text(bytes, &stored)?;
                xml.insert(key.clone(), text);
            }
            names.insert(key, stored);
        }
        Ok(Package {
            archive,
            names,
            xml,
        })
    }

    /// Whether the package has this part.
    pub fn has(&self, part: &str) -> bool {
        self.names.contains_key(&normalise(part))
    }

    /// Names of all parts, as stored.
    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.names.values().map(String::as_str)
    }

    /// The text of an XML part.
    pub fn xml(&self, part: &str) -> Result<&str, Error> {
        self.xml
            .get(&normalise(part))
            .map(String::as_str)
            .ok_or_else(|| Error::Missing(part.to_string()))
    }

    /// The bytes of any part (an image, a video), refusing more than
    /// `limit` bytes once inflated.
    pub fn read_part(&mut self, part: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let stored = self
            .names
            .get(&normalise(part))
            .ok_or_else(|| Error::Missing(part.to_string()))?
            .clone();
        let mut entry = self
            .archive
            .by_name(&stored)
            .map_err(|e| Error::Zip(e.to_string()))?;
        read_capped(&mut entry, limit, &stored)
    }

    /// The relationships of `part` (from `dir/_rels/name.rels`). A part
    /// without a relationships part has none. Targets are resolved against
    /// the part's folder.
    pub fn relationships(&self, part: &str) -> Result<Vec<Relationship>, Error> {
        let rels_name = rels_part_name(part);
        let Ok(text) = self.xml(&rels_name) else {
            return Ok(Vec::new());
        };
        let doc = crate::xml::parse(text, &rels_name)?;
        let base = folder_of(part);
        let mut out = Vec::new();
        for node in doc.root_element().children().filter(|n| n.is_element()) {
            if node.tag_name().name() != "Relationship" {
                continue;
            }
            let (Some(id), Some(kind), Some(target)) = (
                node.attribute("Id"),
                node.attribute("Type"),
                node.attribute("Target"),
            ) else {
                continue;
            };
            let external = node
                .attribute("TargetMode")
                .is_some_and(|m| m.eq_ignore_ascii_case("External"));
            let target = if external {
                Target::External(target.to_string())
            } else {
                Target::Part(resolve(base, target))
            };
            out.push(Relationship {
                id: id.to_string(),
                kind: kind.to_string(),
                target,
            });
        }
        Ok(out)
    }
}

/// Lower case, forward slashes, no leading slash.
fn normalise(name: &str) -> String {
    name.trim_start_matches('/')
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// `ppt/slides/slide1.xml` → `ppt/slides/_rels/slide1.xml.rels`;
/// the package itself (`""`) → `_rels/.rels`.
pub fn rels_part_name(part: &str) -> String {
    let part = part.trim_start_matches('/');
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

fn folder_of(part: &str) -> &str {
    part.trim_start_matches('/')
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir)
}

/// Resolves a relationship target against the folder of its source part:
/// `../slideLayouts/slideLayout1.xml` from `ppt/slides` is
/// `ppt/slideLayouts/slideLayout1.xml`; a target starting with `/` is
/// relative to the package root. `..` never climbs above the root.
pub fn resolve(base: &str, target: &str) -> String {
    let target = target.split(['#', '?']).next().unwrap_or("");
    let mut segments: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    segments.join("/")
}

fn read_capped(reader: &mut impl Read, limit: u64, name: &str) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Zip(format!("{name}: {e}")))?;
    if bytes.len() as u64 > limit {
        return Err(Error::TooLarge {
            part: name.to_string(),
            limit,
        });
    }
    Ok(bytes)
}

/// XML parts are UTF-8 or UTF-16 (with a byte-order mark). PowerPoint
/// writes UTF-8; UTF-16 is accepted because the conventions allow it.
fn decode_xml_text(bytes: Vec<u8>, name: &str) -> Result<String, Error> {
    let bad = |what: &str| Error::Xml {
        part: name.to_string(),
        message: what.to_string(),
    };
    match bytes.as_slice() {
        [0xEF, 0xBB, 0xBF, rest @ ..] => {
            String::from_utf8(rest.to_vec()).map_err(|_| bad("invalid UTF-8"))
        }
        [0xFF, 0xFE, rest @ ..] => {
            utf16(rest, u16::from_le_bytes).ok_or_else(|| bad("invalid UTF-16"))
        }
        [0xFE, 0xFF, rest @ ..] => {
            utf16(rest, u16::from_be_bytes).ok_or_else(|| bad("invalid UTF-16"))
        }
        _ => String::from_utf8(bytes).map_err(|_| bad("invalid UTF-8")),
    }
}

fn utf16(bytes: &[u8], word: fn([u8; 2]) -> u16) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| word([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_and_absolute_targets() {
        assert_eq!(
            resolve("ppt/slides", "../slideLayouts/slideLayout1.xml"),
            "ppt/slideLayouts/slideLayout1.xml"
        );
        assert_eq!(resolve("ppt", "slides/slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(resolve("ppt/slides", "/ppt/media/a.png"), "ppt/media/a.png");
        assert_eq!(resolve("", "ppt/presentation.xml"), "ppt/presentation.xml");
        // Never above the root.
        assert_eq!(resolve("ppt", "../../../x.xml"), "x.xml");
        assert_eq!(resolve("ppt", "./a/./b.xml#frag"), "ppt/a/b.xml");
    }

    #[test]
    fn rels_names() {
        assert_eq!(
            rels_part_name("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
        assert_eq!(rels_part_name(""), "_rels/.rels");
        assert_eq!(
            rels_part_name("/ppt/presentation.xml"),
            "ppt/_rels/presentation.xml.rels"
        );
    }

    #[test]
    fn decodes_utf16_with_bom() {
        let text = "<a/>";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_xml_text(bytes, "x").unwrap(), text);
        assert!(decode_xml_text(vec![0xFF, 0xFE, 0x00], "x").is_err());
    }
}
