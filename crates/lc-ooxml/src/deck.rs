//! Assembling a [`Deck`]: presentation → slides → layouts → masters →
//! themes, with notes.
//!
//! Two passes. The first follows relationships from the presentation part
//! and parses every XML part the deck needs, once. The second builds the
//! model, master first (its theme and colour map are needed to resolve
//! anything below it), then each layout, then each slide.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};

use lc_deck::{Deck, Fill, Layer, Placeholder, Shape, Size, Slide, Theme, Transform};
use roxmltree::{Document, Node};

use crate::color::{self, Palette};
use crate::package::{rel, Package, Relationship, Target};
use crate::shapes::{self, PartCtx, ThemeInfo};
use crate::timing;
use crate::xml::{self, child, children, path, A, P};
use crate::Error;

/// Builds the deck of an opened package.
pub fn build<R: Read + Seek>(pkg: &Package<R>) -> Result<Deck, Error> {
    let index = Index::gather(pkg)?;
    let mut docs: HashMap<&str, Document> = HashMap::new();
    for part in &index.parts {
        // A part that cannot be parsed is reported where it is used.
        if let Ok(text) = pkg.xml(part) {
            if let Ok(doc) = xml::parse(text, part) {
                docs.insert(part.as_str(), doc);
            }
        }
    }
    Builder {
        index: &index,
        docs: &docs,
        warnings: RefCell::new(Vec::new()),
    }
    .deck(pkg)
}

/// The parts of a deck and how they relate, before anything is parsed.
struct Index {
    presentation: String,
    /// Slide parts in show order.
    slides: Vec<String>,
    /// Every XML part the deck needs.
    parts: HashSet<String>,
    rels: HashMap<String, Vec<Relationship>>,
}

impl Index {
    fn gather<R: Read + Seek>(pkg: &Package<R>) -> Result<Index, Error> {
        let mut index = Index {
            presentation: String::new(),
            slides: Vec::new(),
            parts: HashSet::new(),
            rels: HashMap::new(),
        };
        let root_rels = pkg.relationships("")?;
        index.presentation = root_rels
            .iter()
            .find(|r| r.is(rel::OFFICE_DOCUMENT))
            .and_then(|r| match &r.target {
                Target::Part(p) => Some(p.clone()),
                Target::External(_) => None,
            })
            .ok_or_else(|| Error::NotPresentation("no main document in the package".into()))?;
        let pres_text = pkg.xml(&index.presentation)?;
        let pres = xml::parse(pres_text, &index.presentation)?;
        if !xml::is(pres.root_element(), P, "presentation") {
            return Err(Error::NotPresentation(format!(
                "the main document is a <{}>, not a presentation",
                pres.root_element().tag_name().name()
            )));
        }
        let pres_rels = index.load_rels(pkg, &index.presentation.clone())?;
        let ids: Vec<String> = path(pres.root_element(), P, &["sldIdLst"])
            .map(|l| {
                children(l, P, "sldId")
                    .into_iter()
                    .filter_map(|s| xml::rel_attr(s, "id").map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for id in ids {
            if let Some(Target::Part(p)) = pres_rels
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.target.clone())
            {
                index.slides.push(p);
            }
        }
        index.parts.insert(index.presentation.clone());
        // Follow each slide down to its theme, collecting parts.
        let mut queue: Vec<String> = index.slides.clone();
        let mut seen = HashSet::new();
        while let Some(part) = queue.pop() {
            if !seen.insert(part.clone()) {
                continue;
            }
            index.parts.insert(part.clone());
            for r in index.load_rels(pkg, &part)? {
                let Target::Part(target) = &r.target else {
                    continue;
                };
                let follow = [
                    rel::SLIDE_LAYOUT,
                    rel::SLIDE_MASTER,
                    rel::THEME,
                    rel::NOTES_SLIDE,
                ]
                .iter()
                .any(|k| r.is(k))
                    || r.kind.ends_with("/diagramData");
                if follow {
                    queue.push(target.clone());
                }
            }
        }
        Ok(index)
    }

    fn load_rels<R: Read + Seek>(
        &mut self,
        pkg: &Package<R>,
        part: &str,
    ) -> Result<Vec<Relationship>, Error> {
        let rels = pkg.relationships(part)?;
        self.rels.insert(part.to_string(), rels.clone());
        Ok(rels)
    }

    fn rels(&self, part: &str) -> &[Relationship] {
        self.rels.get(part).map_or(&[], Vec::as_slice)
    }

    fn target(&self, part: &str, kind: &str) -> Option<String> {
        self.rels(part)
            .iter()
            .find(|r| r.is(kind))
            .and_then(|r| match &r.target {
                Target::Part(p) => Some(p.clone()),
                Target::External(_) => None,
            })
    }

    fn rel_map(&self, part: &str) -> HashMap<String, Target> {
        self.rels(part)
            .iter()
            .map(|r| (r.id.clone(), r.target.clone()))
            .collect()
    }
}

struct Builder<'b, 'd, 'p> {
    index: &'b Index,
    docs: &'b HashMap<&'d str, Document<'p>>,
    warnings: RefCell<Vec<String>>,
}

/// A master once built: what layouts and slides below it need.
struct Master<'d, 'p> {
    theme: ThemeInfo<'d, 'p>,
    palette: Palette,
    placeholders: Vec<(Placeholder, Transform)>,
    decoration: Vec<Shape>,
    background: Option<Fill>,
}

struct Layout {
    name: String,
    master: String,
    placeholders: Vec<(Placeholder, Transform)>,
    decoration: Vec<Shape>,
    background: Option<Fill>,
    show_master_shapes: bool,
}

impl<'b, 'd, 'p> Builder<'b, 'd, 'p>
where
    'b: 'd,
{
    fn warn(&self, m: impl Into<String>) {
        self.warnings.borrow_mut().push(m.into());
    }

    fn root(&self, part: &str) -> Option<Node<'b, 'p>> {
        match self.docs.get(part) {
            Some(d) => Some(d.root_element()),
            None => {
                self.warn(format!("{part}: missing or not readable"));
                None
            }
        }
    }

    fn deck<R: Read + Seek>(self, _pkg: &Package<R>) -> Result<Deck, Error> {
        let pres = self
            .root(&self.index.presentation)
            .ok_or_else(|| Error::NotPresentation("presentation part not readable".into()))?;
        let size = child(pres, P, "sldSz")
            .map(|s| Size {
                cx: xml::int(s, "cx").unwrap_or(0),
                cy: xml::int(s, "cy").unwrap_or(0),
            })
            .unwrap_or(Size {
                // PowerPoint's default 4:3 size, when none is stated.
                cx: 9_144_000,
                cy: 6_858_000,
            });

        let mut masters: HashMap<String, Master> = HashMap::new();
        let mut layouts: HashMap<String, Layout> = HashMap::new();
        let mut slides = Vec::new();
        let mut first_theme: Option<Theme> = None;

        for (i, slide_part) in self.index.slides.iter().enumerate() {
            let layout_part = self.index.target(slide_part, rel::SLIDE_LAYOUT);
            let Some(layout_part) = layout_part else {
                self.warn(format!("{slide_part}: no layout; skipped"));
                continue;
            };
            if !layouts.contains_key(&layout_part) {
                let Some(master_part) = self.index.target(&layout_part, rel::SLIDE_MASTER) else {
                    self.warn(format!("{layout_part}: no master; slide skipped"));
                    continue;
                };
                if !masters.contains_key(&master_part) {
                    let Some(m) = self.master(&master_part) else {
                        continue;
                    };
                    first_theme.get_or_insert_with(|| Theme {
                        name: m.theme.name.clone(),
                        major_font: m.theme.major_font.clone(),
                        minor_font: m.theme.minor_font.clone(),
                    });
                    masters.insert(master_part.clone(), m);
                }
                let Some(l) = self.layout(&layout_part, &master_part, &masters[&master_part])
                else {
                    continue;
                };
                layouts.insert(layout_part.clone(), l);
            }
            let layout = &layouts[&layout_part];
            let master = &masters[&layout.master];
            if let Some(s) = self.slide(slide_part, i as u32 + 1, layout, master) {
                slides.push(s);
            }
        }

        let mut warnings = self.warnings.into_inner();
        let mut seen = HashSet::new();
        warnings.retain(|w| seen.insert(w.clone()));
        Ok(Deck {
            size,
            theme: first_theme.unwrap_or_default(),
            slides,
            warnings,
        })
    }

    fn theme(&self, master_part: &str) -> ThemeInfo<'b, 'p> {
        let mut info = ThemeInfo {
            name: String::new(),
            major_font: String::new(),
            minor_font: String::new(),
            scheme: color::Scheme::default(),
            fill_styles: Vec::new(),
            line_styles: Vec::new(),
            background_styles: Vec::new(),
        };
        let Some(theme_part) = self.index.target(master_part, rel::THEME) else {
            self.warn(format!(
                "{master_part}: no theme; theme colours will not resolve"
            ));
            return info;
        };
        let Some(theme) = self.root(&theme_part) else {
            return info;
        };
        info.name = theme.attribute("name").unwrap_or("").to_string();
        let elements = child(theme, A, "themeElements");
        if let Some(scheme) = elements.and_then(|e| child(e, A, "clrScheme")) {
            info.scheme = color::Scheme::read(scheme);
        }
        let font = |which: &str| {
            elements
                .and_then(|e| path(e, A, &["fontScheme", which, "latin"]))
                .and_then(|l| l.attribute("typeface"))
                .unwrap_or("")
                .to_string()
        };
        info.major_font = font("majorFont");
        info.minor_font = font("minorFont");
        let list = |name: &str| {
            elements
                .and_then(|e| path(e, A, &["fmtScheme", name]))
                .map(xml::elements)
                .unwrap_or_default()
        };
        info.fill_styles = list("fillStyleLst");
        info.line_styles = list("lnStyleLst");
        info.background_styles = list("bgFillStyleLst");
        info
    }

    fn ctx<'c>(
        &'c self,
        part: &'c str,
        palette: Palette,
        theme: &'c ThemeInfo<'b, 'p>,
        inherited: &'c [(Placeholder, Transform)],
        diagrams: &'c HashMap<String, String>,
    ) -> PartCtx<'c, 'b, 'p> {
        PartCtx {
            part,
            palette,
            rels: self.index.rel_map(part),
            theme,
            inherited,
            warnings: &self.warnings,
            diagram_drawings: diagrams,
        }
    }

    fn master(&self, part: &str) -> Option<Master<'b, 'p>> {
        let root = self.root(part)?;
        let theme = self.theme(part);
        let map = child(root, P, "clrMap")
            .map(color::Map::read)
            .unwrap_or_default();
        let palette = Palette {
            scheme: theme.scheme.clone(),
            map,
            placeholder: None,
        };
        let none = HashMap::new();
        let ctx = self.ctx(part, palette.clone(), &theme, &[], &none);
        let csld = child(root, P, "cSld");
        let (placeholders, decoration) =
            split(csld.map_or_else(Vec::new, |c| tree(&ctx, c, Layer::Master)));
        let background = csld.and_then(|c| background(&ctx, c));
        drop(ctx);
        Some(Master {
            theme,
            palette,
            placeholders,
            decoration,
            background,
        })
    }

    fn layout(&self, part: &str, master_part: &str, master: &Master<'b, 'p>) -> Option<Layout> {
        let root = self.root(part)?;
        let palette = palette_for(root, &master.palette);
        let none = HashMap::new();
        let ctx = self.ctx(part, palette, &master.theme, &master.placeholders, &none);
        let csld = child(root, P, "cSld");
        let (placeholders, decoration) =
            split(csld.map_or_else(Vec::new, |c| tree(&ctx, c, Layer::Layout)));
        Some(Layout {
            name: csld
                .and_then(|c| c.attribute("name"))
                .unwrap_or("")
                .to_string(),
            master: master_part.to_string(),
            placeholders,
            decoration,
            background: csld
                .and_then(|c| background(&ctx, c))
                .or_else(|| master.background.clone()),
            show_master_shapes: xml::boolean(root, "showMasterSp").unwrap_or(true),
        })
    }

    fn slide(
        &self,
        part: &str,
        number: u32,
        layout: &Layout,
        master: &Master<'b, 'p>,
    ) -> Option<Slide> {
        let root = self.root(part)?;
        let palette = palette_for(root, &master.palette);
        let diagrams = self.diagram_drawings(part);
        let ctx = self.ctx(
            part,
            palette,
            &master.theme,
            &layout.placeholders,
            &diagrams,
        );
        let csld = child(root, P, "cSld");

        let show_template = xml::boolean(root, "showMasterSp").unwrap_or(true);
        let mut shapes = Vec::new();
        if show_template {
            if layout.show_master_shapes {
                shapes.extend(master.decoration.iter().cloned());
            }
            shapes.extend(layout.decoration.iter().cloned());
        }
        shapes.extend(csld.map_or_else(Vec::new, |c| tree(&ctx, c, Layer::Slide)));

        let transition = child(root, P, "transition").map(timing::transition);
        let timing = child(root, P, "timing").and_then(timing::timing);
        if let Some(t) = &timing {
            let n = t.interactive_sequences();
            if n > 0 {
                ctx.warn(format!(
                    "{n} animation trigger(s) on shape clicks will not play"
                ));
            }
        }
        let notes = self
            .index
            .target(part, rel::NOTES_SLIDE)
            .and_then(|n| self.docs.get(n.as_str()))
            .map(|d| notes_text(d.root_element()))
            .unwrap_or_default();
        Some(Slide {
            number,
            hidden: xml::boolean(root, "show") == Some(false),
            layout: layout.name.clone(),
            background: csld
                .and_then(|c| background(&ctx, c))
                .or_else(|| layout.background.clone()),
            shapes,
            transition,
            timing,
            notes,
            source: part.to_string(),
        })
    }

    /// SmartArt data part → its stored drawing part, for one slide.
    fn diagram_drawings(&self, slide_part: &str) -> HashMap<String, String> {
        let rels = self.index.rel_map(slide_part);
        let mut out = HashMap::new();
        for r in self.index.rels(slide_part) {
            let (true, Target::Part(data)) = (r.kind.ends_with("/diagramData"), &r.target) else {
                continue;
            };
            let drawing = self
                .docs
                .get(data.as_str())
                .and_then(|d| shapes::diagram_drawing_rel(d.root_element()))
                .and_then(|id| match rels.get(&id) {
                    Some(Target::Part(p)) => Some(p.clone()),
                    _ => None,
                });
            if let Some(d) = drawing {
                out.insert(data.clone(), d);
            }
        }
        out
    }
}

/// The colour palette of a layout or slide: the master's, with the part's
/// own colour-map override if it has one.
fn palette_for(root: Node, master: &Palette) -> Palette {
    let mut p = master.clone();
    if let Some(over) =
        path(root, P, &["clrMapOvr"]).and_then(|o| child(o, A, "overrideClrMapping"))
    {
        p.map = color::Map::read(over);
    }
    p
}

fn tree(ctx: &PartCtx, csld: Node, layer: Layer) -> Vec<Shape> {
    child(csld, P, "spTree").map_or_else(Vec::new, |t| shapes::shape_tree(ctx, t, layer))
}

/// Placeholders (kept as positions for the level below) and decoration
/// (drawn on every slide below).
fn split(shapes: Vec<Shape>) -> (Vec<(Placeholder, Transform)>, Vec<Shape>) {
    let mut placeholders = Vec::new();
    let mut decoration = Vec::new();
    for s in shapes {
        match &s.placeholder {
            Some(ph) => placeholders.push((ph.clone(), s.transform)),
            None => decoration.push(s),
        }
    }
    (placeholders, decoration)
}

/// `p:bg`: its own fill (`p:bgPr`) or a theme background style (`p:bgRef`).
fn background(ctx: &PartCtx, csld: Node) -> Option<Fill> {
    let bg = child(csld, P, "bg")?;
    if let Some(pr) = child(bg, P, "bgPr") {
        return shapes::fill(ctx, pr, &ctx.palette);
    }
    let r = child(bg, P, "bgRef")?;
    let idx = xml::uint(r, "idx")? as usize;
    let (list, pos) = if idx >= 1001 {
        (&ctx.theme.background_styles, idx - 1001)
    } else {
        (&ctx.theme.fill_styles, idx.checked_sub(1)?)
    };
    let palette = ctx
        .palette
        .with_placeholder(color::color_child(r).and_then(|c| color::resolve(c, &ctx.palette)));
    shapes::fill_element(ctx, *list.get(pos)?, &palette)
}

/// The text of a notes page: its body placeholder(s), paragraphs joined by
/// line breaks.
fn notes_text(root: Node) -> String {
    let Some(tree) = path(root, P, &["cSld", "spTree"]) else {
        return String::new();
    };
    let mut paragraphs = Vec::new();
    for sp in tree.descendants().filter(|n| xml::is(*n, P, "sp")) {
        let is_body = path(sp, P, &["nvSpPr", "nvPr", "ph"])
            .is_some_and(|ph| ph.attribute("type") == Some("body"));
        if !is_body {
            continue;
        }
        let Some(body) = child(sp, P, "txBody") else {
            continue;
        };
        for p in children(body, A, "p") {
            let mut text = String::new();
            for r in xml::elements(p) {
                match r.tag_name().name() {
                    "r" | "fld" => {
                        if let Some(t) = child(r, A, "t") {
                            text.push_str(t.text().unwrap_or(""));
                        }
                    }
                    "br" => text.push('\n'),
                    _ => {}
                }
            }
            paragraphs.push(text);
        }
    }
    // Trailing empty paragraphs are PowerPoint's default empty body.
    while paragraphs.last().is_some_and(String::is_empty) {
        paragraphs.pop();
    }
    paragraphs.join("\n")
}
