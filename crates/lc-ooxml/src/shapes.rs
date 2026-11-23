//! A shape tree (`p:spTree`) to [`lc_deck::Shape`]s: shapes, pictures,
//! groups, graphic frames and connectors, with their fills, outlines and
//! text.
//!
//! Inheritance handled here:
//!
//! * A placeholder without its own `a:xfrm` takes the position of the
//!   matching placeholder one level up (slide → layout → master), passed in
//!   as [`PartCtx::inherited`].
//! * A shape that states no fill or outline in `p:spPr` takes them from its
//!   `p:style` reference into the theme's style lists, coloured with the
//!   reference's colour (`phClr`).
//!
//! Not handled yet (later phases, by design): the text style cascade,
//! custom geometry paths, effects, 3D.

use std::cell::RefCell;
use std::collections::HashMap;

use lc_deck::{
    Autofit, Crop, Fill, FrameContent, Geometry, GradientStop, Layer, Line, Media, MediaKind,
    Paragraph, Placeholder, Rect, Run, Shape, ShapeKind, TextBody, Transform,
};
use roxmltree::Node;

use crate::color::{self, Palette};
use crate::package::Target;
use crate::xml::{self, child, children, path, A, DSP, P, P14};

/// What the theme contributes to shapes.
pub struct ThemeInfo<'a, 'i> {
    pub name: String,
    pub major_font: String,
    pub minor_font: String,
    pub scheme: color::Scheme,
    /// `a:fmtScheme/a:fillStyleLst` children (referenced as 1, 2, 3).
    pub fill_styles: Vec<Node<'a, 'i>>,
    /// `a:lnStyleLst` children.
    pub line_styles: Vec<Node<'a, 'i>>,
    /// `a:bgFillStyleLst` children (referenced as 1001, 1002, 1003).
    pub background_styles: Vec<Node<'a, 'i>>,
}

/// One XML part being turned into shapes.
pub struct PartCtx<'c, 'a, 'i> {
    pub part: &'c str,
    pub palette: Palette,
    /// Relationship id → target.
    pub rels: HashMap<String, Target>,
    pub theme: &'c ThemeInfo<'a, 'i>,
    /// Placeholders one level up, with their resolved positions.
    pub inherited: &'c [(Placeholder, Transform)],
    pub warnings: &'c RefCell<Vec<String>>,
    /// Relationships of a SmartArt data part, keyed by the data part name;
    /// filled by the caller (needs the package).
    pub diagram_drawings: &'c HashMap<String, String>,
}

impl PartCtx<'_, '_, '_> {
    pub fn warn(&self, message: impl Into<String>) {
        self.warnings
            .borrow_mut()
            .push(format!("{}: {}", self.part, message.into()));
    }

    /// The package part a relationship id points at.
    pub fn part_of(&self, id: &str) -> Option<String> {
        match self.rels.get(id)? {
            Target::Part(p) => Some(p.clone()),
            Target::External(_) => None,
        }
    }

    fn link_of(&self, id: &str) -> Option<String> {
        match self.rels.get(id)? {
            Target::External(l) => Some(l.clone()),
            Target::Part(_) => None,
        }
    }
}

/// All shapes in a `p:spTree` (or a group), in drawing order.
pub fn shape_tree(ctx: &PartCtx, tree: Node, layer: Layer) -> Vec<Shape> {
    let mut out = Vec::new();
    for node in xml::elements(tree) {
        let name = node.tag_name().name();
        if !xml::is(node, P, name) {
            continue;
        }
        let shape = match name {
            "sp" => Some(shape(ctx, node, layer)),
            "pic" => Some(picture(ctx, node, layer)),
            "grpSp" => Some(group(ctx, node, layer)),
            "graphicFrame" => Some(frame(ctx, node, layer)),
            "cxnSp" => Some(connector(ctx, node, layer)),
            // Non-visual properties of the tree itself.
            "nvGrpSpPr" | "grpSpPr" | "extLst" => None,
            other => {
                ctx.warn(format!("skipped a <{other}> (not supported)"));
                None
            }
        };
        out.extend(shape);
    }
    out
}

/// The `cNvPr` (id, name, hidden) and `nvPr` of a shape's non-visual
/// properties element (`nvSpPr`, `nvPicPr`…).
fn identity<'a, 'i>(node: Node<'a, 'i>, nv: &str) -> (u32, String, bool, Option<Node<'a, 'i>>) {
    let nv_node = child(node, P, nv);
    let c = nv_node.and_then(|n| child(n, P, "cNvPr"));
    let id = c.and_then(|c| xml::uint(c, "id")).unwrap_or(0);
    let name = c
        .and_then(|c| c.attribute("name"))
        .unwrap_or("")
        .to_string();
    let hidden = c.and_then(|c| xml::boolean(c, "hidden")).unwrap_or(false);
    (id, name, hidden, nv_node.and_then(|n| child(n, P, "nvPr")))
}

fn placeholder_of(nv_pr: Option<Node>) -> Option<Placeholder> {
    let ph = child(nv_pr?, P, "ph")?;
    Some(Placeholder {
        kind: ph.attribute("type").unwrap_or("obj").to_string(),
        index: xml::uint(ph, "idx").unwrap_or(0),
    })
}

/// Reads `a:xfrm` (or `p:xfrm` in graphic frames).
pub fn transform(xfrm: Node) -> Transform {
    let off = child(xfrm, A, "off");
    let ext = child(xfrm, A, "ext");
    Transform {
        x: off.and_then(|n| xml::int(n, "x")).unwrap_or(0),
        y: off.and_then(|n| xml::int(n, "y")).unwrap_or(0),
        cx: ext.and_then(|n| xml::int(n, "cx")).unwrap_or(0),
        cy: ext.and_then(|n| xml::int(n, "cy")).unwrap_or(0),
        rotation: xml::int(xfrm, "rot").unwrap_or(0) as f64 / 60_000.0,
        flip_h: xml::boolean(xfrm, "flipH").unwrap_or(false),
        flip_v: xml::boolean(xfrm, "flipV").unwrap_or(false),
    }
}

/// The shape's own transform, or the inherited one for a placeholder.
fn placed(ctx: &PartCtx, xfrm: Option<Node>, ph: Option<&Placeholder>, what: &str) -> Transform {
    if let Some(x) = xfrm {
        return transform(x);
    }
    if let Some(ph) = ph {
        if let Some(t) = inherited_transform(ctx.inherited, ph) {
            return t;
        }
        ctx.warn(format!(
            "placeholder {what} ({} {}) has no position anywhere",
            ph.kind, ph.index
        ));
    }
    Transform::default()
}

/// Finds the placeholder one level up that `ph` inherits from: the one
/// with the same index, else the first of the same kind. Kinds are
/// compared loosely, as PowerPoint does: a centred title takes a title's
/// place, and content placeholders (body, subtitle, object, picture,
/// chart, table…) take the body's.
pub fn inherited_transform(
    list: &[(Placeholder, Transform)],
    ph: &Placeholder,
) -> Option<Transform> {
    let by_index = list
        .iter()
        .find(|(p, _)| ph.index != 0 && p.index == ph.index);
    let by_kind = || {
        list.iter()
            .find(|(p, _)| kind_class(&p.kind) == kind_class(&ph.kind))
    };
    by_index.or_else(by_kind).map(|(_, t)| *t)
}

fn kind_class(kind: &str) -> &str {
    match kind {
        "title" | "ctrTitle" => "title",
        "body" | "subTitle" | "obj" | "pic" | "chart" | "tbl" | "dgm" | "media" | "clipArt" => {
            "body"
        }
        other => other,
    }
}

fn shape(ctx: &PartCtx, node: Node, layer: Layer) -> Shape {
    let (id, name, hidden, nv_pr) = identity(node, "nvSpPr");
    let placeholder = placeholder_of(nv_pr);
    let sp_pr = child(node, P, "spPr");
    let style = child(node, P, "style");
    let transform = placed(
        ctx,
        sp_pr.and_then(|s| child(s, A, "xfrm")),
        placeholder.as_ref(),
        &name,
    );
    let fill = sp_pr
        .and_then(|s| fill(ctx, s, &ctx.palette))
        .or_else(|| style.and_then(|s| style_fill(ctx, s)))
        .unwrap_or_default();
    let line = sp_pr
        .and_then(|s| child(s, A, "ln"))
        .map(|ln| line(ctx, ln, &ctx.palette))
        .or_else(|| style.and_then(|s| style_line(ctx, s)));
    let text = child(node, P, "txBody").map(|t| text_body(ctx, t));
    Shape {
        id,
        name,
        hidden,
        layer,
        transform,
        placeholder,
        kind: ShapeKind::Shape {
            geometry: sp_pr.map(geometry).unwrap_or_default(),
            fill,
            line,
            text,
        },
    }
}

fn connector(ctx: &PartCtx, node: Node, layer: Layer) -> Shape {
    let (id, name, hidden, _) = identity(node, "nvCxnSpPr");
    let sp_pr = child(node, P, "spPr");
    let style = child(node, P, "style");
    Shape {
        id,
        name,
        hidden,
        layer,
        transform: sp_pr
            .and_then(|s| child(s, A, "xfrm"))
            .map(transform)
            .unwrap_or_default(),
        placeholder: None,
        kind: ShapeKind::Connector {
            geometry: sp_pr.map(geometry).unwrap_or_default(),
            line: sp_pr
                .and_then(|s| child(s, A, "ln"))
                .map(|ln| line(ctx, ln, &ctx.palette))
                .or_else(|| style.and_then(|s| style_line(ctx, s))),
        },
    }
}

fn picture(ctx: &PartCtx, node: Node, layer: Layer) -> Shape {
    let (id, name, hidden, nv_pr) = identity(node, "nvPicPr");
    let placeholder = placeholder_of(nv_pr);
    let sp_pr = child(node, P, "spPr");
    let blip_fill = child(node, P, "blipFill");
    let image = blip_fill
        .and_then(|b| child(b, A, "blip"))
        .and_then(|b| xml::rel_attr(b, "embed"))
        .and_then(|id| ctx.part_of(id));
    let crop = blip_fill
        .and_then(|b| child(b, A, "srcRect"))
        .map(|r| Crop {
            left: xml::fraction(r, "l").unwrap_or(0.0),
            top: xml::fraction(r, "t").unwrap_or(0.0),
            right: xml::fraction(r, "r").unwrap_or(0.0),
            bottom: xml::fraction(r, "b").unwrap_or(0.0),
        })
        .unwrap_or_default();
    Shape {
        id,
        name: name.clone(),
        hidden,
        layer,
        transform: placed(
            ctx,
            sp_pr.and_then(|s| child(s, A, "xfrm")),
            placeholder.as_ref(),
            &name,
        ),
        placeholder,
        kind: ShapeKind::Picture {
            image,
            crop,
            media: nv_pr.and_then(|n| media(ctx, n)),
        },
    }
}

/// A video or audio clip on a picture. PowerPoint 2010 and later embed the
/// clip and point at it from `p14:media` (`r:embed`); `a:videoFile` /
/// `a:audioFile` hold the older link, which for an embedded clip points at
/// the same part.
fn media(ctx: &PartCtx, nv_pr: Node) -> Option<Media> {
    let (kind, link_node) = match child(nv_pr, A, "videoFile") {
        Some(v) => (MediaKind::Video, Some(v)),
        None => (MediaKind::Audio, Some(child(nv_pr, A, "audioFile")?)),
    };
    let embedded = path(nv_pr, P, &["extLst"])
        .map(|ext| {
            ext.descendants()
                .find(|n| xml::is(*n, P14, "media"))
                .and_then(|m| xml::rel_attr(m, "embed"))
                .and_then(|id| ctx.part_of(id))
        })
        .unwrap_or(None);
    let linked_id = link_node.and_then(|n| xml::rel_attr(n, "link"));
    let part = embedded.or_else(|| linked_id.and_then(|id| ctx.part_of(id)));
    let link = if part.is_none() {
        linked_id.and_then(|id| ctx.link_of(id))
    } else {
        None
    };
    if part.is_none() && link.is_none() {
        ctx.warn("a media clip whose file could not be found");
    }
    Some(Media { kind, part, link })
}

fn group(ctx: &PartCtx, node: Node, layer: Layer) -> Shape {
    let (id, name, hidden, _) = identity(node, "nvGrpSpPr");
    let xfrm = path(node, P, &["grpSpPr"]).and_then(|g| child(g, A, "xfrm"));
    let child_space = xfrm
        .map(|x| {
            let off = child(x, A, "chOff");
            let ext = child(x, A, "chExt");
            Rect {
                x: off.and_then(|n| xml::int(n, "x")).unwrap_or(0),
                y: off.and_then(|n| xml::int(n, "y")).unwrap_or(0),
                cx: ext.and_then(|n| xml::int(n, "cx")).unwrap_or(0),
                cy: ext.and_then(|n| xml::int(n, "cy")).unwrap_or(0),
            }
        })
        .unwrap_or_default();
    Shape {
        id,
        name,
        hidden,
        layer,
        transform: xfrm.map(transform).unwrap_or_default(),
        placeholder: None,
        kind: ShapeKind::Group {
            child_space,
            children: shape_tree(ctx, node, layer),
        },
    }
}

fn frame(ctx: &PartCtx, node: Node, layer: Layer) -> Shape {
    let (id, name, hidden, nv_pr) = identity(node, "nvGraphicFramePr");
    let placeholder = placeholder_of(nv_pr);
    let data = path(node, A, &["graphic", "graphicData"])
        .or_else(|| child(node, A, "graphic").and_then(|g| child(g, A, "graphicData")));
    let uri = data.and_then(|d| d.attribute("uri")).unwrap_or("");
    let content = match uri.rsplit('/').next().unwrap_or("") {
        "table" => {
            let tbl = data.and_then(|d| child(d, A, "tbl"));
            FrameContent::Table {
                rows: tbl.map_or(0, |t| children(t, A, "tr").len() as u32),
                columns: tbl
                    .and_then(|t| child(t, A, "tblGrid"))
                    .map_or(0, |g| children(g, A, "gridCol").len() as u32),
            }
        }
        "chart" => FrameContent::Chart {
            part: data
                .and_then(first_element)
                .and_then(|c| xml::rel_attr(c, "id"))
                .and_then(|id| ctx.part_of(id)),
        },
        "diagram" => {
            let data_part = data
                .and_then(first_element)
                .and_then(|r| xml::rel_attr(r, "dm"))
                .and_then(|id| ctx.part_of(id));
            FrameContent::Diagram {
                drawing: data_part.and_then(|p| ctx.diagram_drawings.get(&p).cloned()),
            }
        }
        _ => {
            ctx.warn(format!(
                "graphic frame '{name}' of kind {uri} shown as its frame only"
            ));
            FrameContent::Other {
                uri: uri.to_string(),
            }
        }
    };
    Shape {
        id,
        name: name.clone(),
        hidden,
        layer,
        // Graphic frames keep their transform in p:xfrm, not a:xfrm.
        transform: placed(ctx, child(node, P, "xfrm"), placeholder.as_ref(), &name),
        placeholder,
        kind: ShapeKind::Frame { content },
    }
}

fn first_element<'a, 'i>(n: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    xml::elements(n).into_iter().next()
}

/// The SmartArt drawing part a data part names in its extension list
/// (`dsp:dataModelExt relId`), resolved against the *slide's*
/// relationships (that is where PowerPoint puts it).
pub fn diagram_drawing_rel(data_doc: Node) -> Option<String> {
    data_doc
        .descendants()
        .find(|n| xml::is(*n, DSP, "dataModelExt"))
        .and_then(|n| n.attribute("relId"))
        .map(str::to_string)
}

fn geometry(sp_pr: Node) -> Geometry {
    if let Some(p) = child(sp_pr, A, "prstGeom") {
        let adjust = child(p, A, "avLst")
            .map(|av| {
                children(av, A, "gd")
                    .into_iter()
                    .filter_map(|gd| {
                        let name = gd.attribute("name")?.to_string();
                        let value = gd
                            .attribute("fmla")?
                            .strip_prefix("val ")?
                            .trim()
                            .parse()
                            .ok()?;
                        Some((name, value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Geometry::Preset {
            name: p.attribute("prst").unwrap_or("rect").to_string(),
            adjust,
        }
    } else if child(sp_pr, A, "custGeom").is_some() {
        Geometry::Custom
    } else {
        Geometry::None
    }
}

/// The fill stated directly in `node` (an `spPr`, `bgPr`, `ln`, …), or
/// `None` when it states none.
pub fn fill(ctx: &PartCtx, node: Node, palette: &Palette) -> Option<Fill> {
    xml::elements(node)
        .into_iter()
        .find_map(|f| fill_element(ctx, f, palette))
}

/// `f` read as a fill element (`a:noFill`, `a:solidFill`…); `None` when it
/// is not one.
pub fn fill_element(ctx: &PartCtx, f: Node, palette: &Palette) -> Option<Fill> {
    if f.tag_name().namespace().is_none_or(|u| !A.contains(&u)) {
        return None;
    }
    Some(match f.tag_name().name() {
        "noFill" => Fill::None,
        "solidFill" => match color::color_child(f).and_then(|c| color::resolve(c, palette)) {
            Some(color) => Fill::Solid { color },
            None => {
                ctx.warn("a solid fill with an unresolvable colour");
                Fill::None
            }
        },
        "gradFill" => gradient(f, palette),
        "blipFill" => Fill::Picture {
            image: child(f, A, "blip")
                .and_then(|b| xml::rel_attr(b, "embed"))
                .and_then(|id| ctx.part_of(id)),
        },
        "pattFill" | "grpFill" => Fill::Other {
            kind: f.tag_name().name().to_string(),
        },
        _ => return None,
    })
}

fn gradient(g: Node, palette: &Palette) -> Fill {
    let stops = child(g, A, "gsLst")
        .map(|l| {
            children(l, A, "gs")
                .into_iter()
                .filter_map(|gs| {
                    Some(GradientStop {
                        position: xml::fraction(gs, "pos")?,
                        color: color::color_child(gs).and_then(|c| color::resolve(c, palette))?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let angle = child(g, A, "lin").map(|l| xml::int(l, "ang").unwrap_or(0) as f64 / 60_000.0);
    Fill::Gradient { stops, angle }
}

fn line(ctx: &PartCtx, ln: Node, palette: &Palette) -> Line {
    Line {
        width: xml::int(ln, "w").unwrap_or(0),
        fill: fill(ctx, ln, palette).unwrap_or(Fill::None),
        dash: child(ln, A, "prstDash")
            .and_then(|d| d.attribute("val"))
            .map(str::to_string),
    }
}

/// `p:style/a:fillRef idx="N"` → the theme's Nth fill style, coloured with
/// the reference's colour. 0 means no fill; 1001+ are background styles.
fn style_fill(ctx: &PartCtx, style: Node) -> Option<Fill> {
    let r = child(style, A, "fillRef")?;
    let idx = xml::uint(r, "idx")? as usize;
    let list = if idx >= 1001 {
        &ctx.theme.background_styles
    } else {
        &ctx.theme.fill_styles
    };
    let pos = if idx >= 1001 {
        idx - 1001
    } else {
        idx.checked_sub(1)?
    };
    let styled = list.get(pos)?;
    // The style list entry is itself the fill element.
    fill_element(ctx, *styled, &ref_palette(ctx, r))
}

fn style_line(ctx: &PartCtx, style: Node) -> Option<Line> {
    let r = child(style, A, "lnRef")?;
    let idx = xml::uint(r, "idx")? as usize;
    let styled = ctx.theme.line_styles.get(idx.checked_sub(1)?)?;
    Some(line(ctx, *styled, &ref_palette(ctx, r)))
}

fn ref_palette(ctx: &PartCtx, reference: Node) -> Palette {
    let c = color::color_child(reference).and_then(|c| color::resolve(c, &ctx.palette));
    ctx.palette.with_placeholder(c)
}

fn text_body(ctx: &PartCtx, body: Node) -> TextBody {
    let pr = child(body, A, "bodyPr");
    let insets = pr.and_then(|p| {
        let get = |n| xml::int(p, n);
        let any = ["lIns", "tIns", "rIns", "bIns"]
            .iter()
            .any(|n| p.attribute(*n).is_some());
        // Defaults when only some are stated: 0.1" left/right, 0.05" top/bottom.
        any.then(|| {
            [
                get("lIns").unwrap_or(91_440),
                get("tIns").unwrap_or(45_720),
                get("rIns").unwrap_or(91_440),
                get("bIns").unwrap_or(45_720),
            ]
        })
    });
    let autofit = pr
        .and_then(|p| {
            if let Some(n) = child(p, A, "normAutofit") {
                Some(Autofit::Normal {
                    font_scale: xml::fraction(n, "fontScale").unwrap_or(1.0),
                    line_spacing_reduction: xml::fraction(n, "lnSpcReduction").unwrap_or(0.0),
                })
            } else {
                child(p, A, "spAutoFit").map(|_| Autofit::Shape)
            }
        })
        .unwrap_or_default();
    TextBody {
        paragraphs: children(body, A, "p")
            .into_iter()
            .map(|p| paragraph(ctx, p))
            .collect(),
        anchor: pr.and_then(|p| p.attribute("anchor")).map(str::to_string),
        insets,
        wrap: pr.and_then(|p| p.attribute("wrap")) != Some("none"),
        autofit,
    }
}

fn paragraph(ctx: &PartCtx, p: Node) -> Paragraph {
    let ppr = child(p, A, "pPr");
    let mut runs = Vec::new();
    for r in xml::elements(p) {
        if r.tag_name().namespace().is_none_or(|u| !A.contains(&u)) {
            continue;
        }
        match r.tag_name().name() {
            "r" => runs.push(run(ctx, r, None)),
            "fld" => runs.push(run(ctx, r, r.attribute("type"))),
            "br" => runs.push(Run {
                text: "\n".into(),
                ..run_props(ctx, child(r, A, "rPr"))
            }),
            _ => {}
        }
    }
    Paragraph {
        level: ppr.and_then(|p| xml::uint(p, "lvl")).unwrap_or(0).min(8) as u8,
        align: ppr.and_then(|p| p.attribute("algn")).map(str::to_string),
        runs,
    }
}

fn run(ctx: &PartCtx, r: Node, field: Option<&str>) -> Run {
    let text = child(r, A, "t")
        .map(|t| t.text().unwrap_or("").to_string())
        .unwrap_or_default();
    Run {
        text,
        field: field.map(str::to_string),
        ..run_props(ctx, child(r, A, "rPr"))
    }
}

fn run_props(ctx: &PartCtx, rpr: Option<Node>) -> Run {
    let Some(rpr) = rpr else {
        return Run::default();
    };
    Run {
        text: String::new(),
        size: xml::uint(rpr, "sz").map(|s| s as f64 / 100.0),
        bold: xml::boolean(rpr, "b"),
        italic: xml::boolean(rpr, "i"),
        font: child(rpr, A, "latin")
            .and_then(|l| l.attribute("typeface"))
            .map(|f| theme_font(ctx, f)),
        color: child(rpr, A, "solidFill")
            .and_then(color::color_child)
            .and_then(|c| color::resolve(c, &ctx.palette)),
        field: None,
    }
}

/// `+mj-lt` / `+mn-lt` (and the East Asian / complex-script variants,
/// which fall back to the Latin face here) → the theme's font name.
fn theme_font(ctx: &PartCtx, face: &str) -> String {
    match face {
        f if f.starts_with("+mj") => ctx.theme.major_font.clone(),
        f if f.starts_with("+mn") => ctx.theme.minor_font.clone(),
        f => f.to_string(),
    }
}
