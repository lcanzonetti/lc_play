//! Reading decks built here, part by part, so every test states exactly the
//! XML it relies on. The XML follows what PowerPoint writes (namespaces,
//! relationship types, Markup Compatibility), trimmed to what each test
//! needs.

use std::io::{Cursor, Write};

use lc_deck::{
    Autofit, Fill, FrameContent, Geometry, Layer, MediaKind, Rgba, ShapeKind, Time, TimeNodeKind,
};
use zip::write::SimpleFileOptions;

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main" xmlns:p159="http://schemas.microsoft.com/office/powerpoint/2015/09/main""#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn rels(list: &[(&str, &str, &str)]) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, kind, target) in list {
        let (kind, mode) = match kind.strip_prefix("ext:") {
            Some(k) => (k, r#" TargetMode="External""#),
            None => (*kind, ""),
        };
        let full = if kind.starts_with("http") {
            kind.to_string()
        } else {
            format!("{REL}/{kind}")
        };
        s.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{full}" Target="{target}"{mode}/>"#
        ));
    }
    s.push_str("</Relationships>");
    s
}

fn theme() -> String {
    format!(
        r#"<a:theme {NS} name="Office Theme"><a:themeElements>
<a:clrScheme name="Office">
  <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
  <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
  <a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
  <a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
  <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
  <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
  <a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:fontScheme name="Office">
  <a:majorFont><a:latin typeface="Calibri Light"/></a:majorFont>
  <a:minorFont><a:latin typeface="Calibri"/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="Office">
  <a:fillStyleLst>
    <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
    <a:gradFill><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"/></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:shade val="50000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="5400000"/></a:gradFill>
    <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
  </a:fillStyleLst>
  <a:lnStyleLst>
    <a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln>
    <a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
    <a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
  </a:lnStyleLst>
  <a:effectStyleLst/>
  <a:bgFillStyleLst>
    <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
    <a:solidFill><a:schemeClr val="phClr"><a:tint val="95000"/></a:schemeClr></a:solidFill>
    <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
  </a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements></a:theme>"#
    )
}

const MASTER_CLRMAP: &str = r#"<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>"#;

fn xfrm(x: i64, y: i64, cx: i64, cy: i64) -> String {
    format!(r#"<a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>"#)
}

fn sp(id: u32, name: &str, ph: &str, sp_pr: &str, extra: &str) -> String {
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr/><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr>{sp_pr}</p:spPr>{extra}</p:sp>"#
    )
}

fn tree(shapes: &str) -> String {
    format!(
        r#"<p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree>"#
    )
}

fn master() -> String {
    let shapes = [
        sp(2, "Title Placeholder", r#"<p:ph type="title"/>"#, &xfrm(100, 200, 3000, 400), ""),
        sp(3, "Text Placeholder", r#"<p:ph type="body" idx="1"/>"#, &xfrm(100, 700, 3000, 2000), ""),
        // Decoration: a logo bar on every slide.
        sp(
            7,
            "Brand bar",
            "",
            &format!(
                r#"{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:schemeClr val="accent1"/></a:solidFill>"#,
                xfrm(0, 0, 9_144_000, 100_000)
            ),
            "",
        ),
    ]
    .concat();
    format!(
        r#"<p:sldMaster {NS}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>{}</p:cSld>{MASTER_CLRMAP}<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#,
        tree(&shapes)
    )
}

fn layout() -> String {
    // The title moves on this layout; the body is not restated (inherits
    // the master's position).
    let shapes = [
        sp(
            2,
            "Title 1",
            r#"<p:ph type="title"/>"#,
            &xfrm(500, 600, 2000, 300),
            "",
        ),
        sp(3, "Content Placeholder 2", r#"<p:ph idx="1"/>"#, "", ""),
    ]
    .concat();
    format!(
        r#"<p:sldLayout {NS} type="obj" preserve="1"><p:cSld name="Title and Content">{}</p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#,
        tree(&shapes)
    )
}

/// A relationship: id, type, target.
type Rel = (String, String, String);

/// A deck: `slides` are (spTree contents, extra elements after cSld,
/// attributes on p:sld, extra slide relationships).
struct Deck {
    slides: Vec<(String, String, String, Vec<Rel>)>,
    notes: Vec<Option<String>>,
    extra_parts: Vec<(String, Vec<u8>)>,
}

impl Deck {
    fn new() -> Deck {
        Deck {
            slides: Vec::new(),
            notes: Vec::new(),
            extra_parts: Vec::new(),
        }
    }

    fn slide(mut self, shapes: &str, after: &str, attrs: &str) -> Deck {
        self.slides.push((
            shapes.to_string(),
            after.to_string(),
            attrs.to_string(),
            Vec::new(),
        ));
        self.notes.push(None);
        self
    }

    fn slide_rel(mut self, id: &str, kind: &str, target: &str) -> Deck {
        self.slides
            .last_mut()
            .unwrap()
            .3
            .push((id.into(), kind.into(), target.into()));
        self
    }

    fn notes(mut self, text: &str) -> Deck {
        *self.notes.last_mut().unwrap() = Some(text.to_string());
        self
    }

    fn part(mut self, name: &str, bytes: &[u8]) -> Deck {
        self.extra_parts.push((name.to_string(), bytes.to_vec()));
        self
    }

    /// Builds the package. Slides are listed in the presentation in
    /// *reverse* file order, so tests also check that order comes from
    /// `sldIdLst`, not from file names.
    fn bytes(&self) -> Vec<u8> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut add = |name: &str, text: String| files.push((name.to_string(), text.into_bytes()));
        add("[Content_Types].xml", r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#.into());
        add(
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
        );
        let n = self.slides.len();
        let mut pres_rels = vec![(
            "rIdM".to_string(),
            "slideMaster".to_string(),
            "slideMasters/slideMaster1.xml".to_string(),
        )];
        let mut ids = String::new();
        for i in (0..n).rev() {
            let rid = format!("rId{}", i + 10);
            ids.push_str(&format!(r#"<p:sldId id="{}" r:id="{rid}"/>"#, 256 + i));
            pres_rels.push((rid, "slide".into(), format!("slides/slide{}.xml", i + 1)));
        }
        let pr: Vec<(&str, &str, &str)> = pres_rels
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
            .collect();
        add("ppt/_rels/presentation.xml.rels", rels(&pr));
        add(
            "ppt/presentation.xml",
            format!(
                r#"<p:presentation {NS}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rIdM"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
            ),
        );
        add("ppt/slideMasters/slideMaster1.xml", master());
        add(
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            rels(&[
                ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
                ("rId2", "theme", "../theme/theme1.xml"),
            ]),
        );
        add("ppt/slideLayouts/slideLayout1.xml", layout());
        add(
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            rels(&[("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]),
        );
        add("ppt/theme/theme1.xml", theme());
        for (i, (shapes, after, attrs, extra)) in self.slides.iter().enumerate() {
            let k = i + 1;
            add(
                &format!("ppt/slides/slide{k}.xml"),
                format!(
                    r#"<p:sld {NS} {attrs}><p:cSld>{}</p:cSld>{after}</p:sld>"#,
                    tree(shapes)
                ),
            );
            let mut r = vec![(
                "rId1".to_string(),
                "slideLayout".to_string(),
                "../slideLayouts/slideLayout1.xml".to_string(),
            )];
            if let Some(text) = &self.notes[i] {
                r.push((
                    "rIdN".into(),
                    "notesSlide".into(),
                    format!("../notesSlides/notesSlide{k}.xml"),
                ));
                let paragraphs: String = text
                    .split('\n')
                    .map(|p| format!("<a:p><a:r><a:t>{p}</a:t></a:r></a:p>"))
                    .collect();
                add(
                    &format!("ppt/notesSlides/notesSlide{k}.xml"),
                    format!(
                        r#"<p:notes {NS}><p:cSld>{}</p:cSld></p:notes>"#,
                        tree(&format!(
                            r#"{}<p:sp><p:nvSpPr><p:cNvPr id="3" name="Notes"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/>{paragraphs}<a:p/></p:txBody></p:sp>"#,
                            sp(2, "Slide Image", r#"<p:ph type="sldImg"/>"#, "", "")
                        ))
                    ),
                );
            }
            r.extend(extra.iter().cloned());
            let rr: Vec<(&str, &str, &str)> = r
                .iter()
                .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
                .collect();
            add(&format!("ppt/slides/_rels/slide{k}.xml.rels"), rels(&rr));
        }
        for (name, bytes) in &self.extra_parts {
            files.push((name.clone(), bytes.clone()));
        }
        zip_of(&files)
    }

    fn read(&self) -> lc_deck::Deck {
        lc_ooxml::read(Cursor::new(self.bytes())).expect("deck reads")
    }
}

fn zip_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in files {
        w.start_file(name.as_str(), SimpleFileOptions::default())
            .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn text_box(id: u32, runs: &str) -> String {
    sp(
        id,
        &format!("TextBox {id}"),
        "",
        &format!(
            r#"{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/>"#,
            xfrm(0, 0, 1000, 1000)
        ),
        &format!(
            r#"<p:txBody><a:bodyPr wrap="square" lIns="0" anchor="ctr"><a:normAutofit fontScale="92500" lnSpcReduction="10000"/></a:bodyPr><a:lstStyle/>{runs}</p:txBody>"#
        ),
    )
}

#[test]
fn slide_order_size_theme_and_hidden() {
    let deck = Deck::new()
        .slide(
            &text_box(2, "<a:p><a:r><a:t>first</a:t></a:r></a:p>"),
            "",
            "",
        )
        .slide(
            &text_box(2, "<a:p><a:r><a:t>second</a:t></a:r></a:p>"),
            "",
            r#"show="0""#,
        )
        .read();
    assert_eq!((deck.size.cx, deck.size.cy), (12_192_000, 6_858_000));
    assert_eq!(deck.theme.major_font, "Calibri Light");
    assert_eq!(deck.theme.minor_font, "Calibri");
    // The presentation lists slide2.xml first.
    assert_eq!(deck.slides.len(), 2);
    assert_eq!(deck.slides[0].source, "ppt/slides/slide2.xml");
    assert!(deck.slides[0].hidden);
    assert_eq!(deck.slides[1].number, 2);
    assert_eq!(deck.slides[1].layout, "Title and Content");
    assert_eq!(deck.shown_slides().count(), 1);
    assert!(deck.warnings.is_empty(), "{:?}", deck.warnings);
}

#[test]
fn placeholders_inherit_position_from_layout_then_master() {
    let shapes = [
        sp(2, "Title", r#"<p:ph type="ctrTitle"/>"#, "", ""),
        sp(3, "Content", r#"<p:ph idx="1"/>"#, "", ""),
        sp(4, "Moved body", r#"<p:ph idx="1"/>"#, &xfrm(1, 2, 3, 4), ""),
    ]
    .concat();
    let deck = Deck::new().slide(&shapes, "", "").read();
    let s = &deck.slides[0];
    let find = |id| {
        s.shapes
            .iter()
            .find(|x| x.id == id && x.layer == Layer::Slide)
            .unwrap()
    };
    // Title: from the layout (which moved it).
    assert_eq!((find(2).transform.x, find(2).transform.y), (500, 600));
    // Body: the layout does not restate it, so the master's position.
    assert_eq!(
        (
            find(3).transform.x,
            find(3).transform.y,
            find(3).transform.cx
        ),
        (100, 700, 3000)
    );
    // Its own position wins.
    assert_eq!(find(4).transform.cx, 3);
    assert_eq!(find(3).placeholder.as_ref().unwrap().kind, "obj");
}

#[test]
fn master_decoration_is_drawn_unless_hidden() {
    let deck = Deck::new()
        .slide("", "", "")
        .slide("", "", r#"showMasterSp="0""#)
        .read();
    // Slide order is reversed: slides[1] is slide1.xml.
    let with = &deck.slides[1];
    assert_eq!(with.shapes.len(), 1);
    let bar = &with.shapes[0];
    assert_eq!((bar.layer, bar.name.as_str()), (Layer::Master, "Brand bar"));
    match &bar.kind {
        ShapeKind::Shape { fill, .. } => {
            assert_eq!(
                fill,
                &Fill::Solid {
                    color: Rgba::rgb(0x44, 0x72, 0xC4)
                }
            )
        }
        k => panic!("{k:?}"),
    }
    assert!(deck.slides[0].shapes.is_empty());
    // Master placeholders are templates, never drawn.
    assert!(with.shapes.iter().all(|s| s.placeholder.is_none()));
    // Background: master's bgRef 1001 = first background style with bg1 (white).
    assert_eq!(with.background, Some(Fill::Solid { color: Rgba::WHITE }));
}

#[test]
fn colours_styles_and_geometry() {
    let styled = format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="5" name="Styled"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="roundRect"><a:avLst><a:gd name="adj" fmla="val 25000"/></a:avLst></a:prstGeom></p:spPr><p:style><a:lnRef idx="2"><a:schemeClr val="accent1"><a:shade val="50000"/></a:schemeClr></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent2"/></a:fillRef><a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style></p:sp>"#,
        xfrm(0, 0, 10, 10)
    );
    let own = sp(
        6,
        "Own",
        "",
        &format!(
            r#"{}<a:custGeom/><a:gradFill><a:gsLst><a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="100000"><a:schemeClr val="tx1"/></a:gs></a:gsLst><a:lin ang="2700000"/></a:gradFill><a:ln w="9525"><a:noFill/></a:ln>"#,
            xfrm(0, 0, 10, 10)
        ),
        "",
    );
    let deck = Deck::new().slide(&format!("{styled}{own}"), "", "").read();
    let shapes = &deck.slides[0].shapes;
    match &shapes[1].kind {
        ShapeKind::Shape {
            geometry,
            fill,
            line,
            ..
        } => {
            assert_eq!(
                geometry,
                &Geometry::Preset {
                    name: "roundRect".into(),
                    adjust: vec![("adj".into(), 25000)]
                }
            );
            // fillRef 1 = solid phClr, coloured accent2.
            assert_eq!(
                fill,
                &Fill::Solid {
                    color: Rgba::rgb(0xED, 0x7D, 0x31)
                }
            );
            let line = line.as_ref().unwrap();
            assert_eq!(line.width, 12_700);
            match line.fill {
                Fill::Solid { color } => assert!(color.b < 0xC4 && color.b > 0x30, "{color:?}"),
                ref f => panic!("{f:?}"),
            }
        }
        k => panic!("{k:?}"),
    }
    match &shapes[2].kind {
        ShapeKind::Shape {
            geometry,
            fill,
            line,
            ..
        } => {
            assert_eq!(geometry, &Geometry::Custom);
            match fill {
                Fill::Gradient { stops, angle } => {
                    assert_eq!(stops.len(), 2);
                    assert_eq!(stops[1].color, Rgba::BLACK);
                    assert_eq!(*angle, Some(45.0));
                }
                f => panic!("{f:?}"),
            }
            assert_eq!(line.as_ref().unwrap().fill, Fill::None);
        }
        k => panic!("{k:?}"),
    }
}

#[test]
fn text_runs_fonts_fields_and_autofit() {
    let runs = r#"<a:p><a:pPr lvl="1" algn="ctr"/><a:r><a:rPr lang="it-IT" sz="2400" b="1"><a:solidFill><a:schemeClr val="accent6"/></a:solidFill><a:latin typeface="+mj-lt"/></a:rPr><a:t>Ciao</a:t></a:r><a:br/><a:r><a:rPr i="1"/><a:t>mondo</a:t></a:r><a:fld id="{X}" type="slidenum"><a:t>3</a:t></a:fld></a:p>"#;
    let deck = Deck::new().slide(&text_box(2, runs), "", "").read();
    let ShapeKind::Shape {
        text: Some(body), ..
    } = &deck.slides[0].shapes[1].kind
    else {
        panic!("no text");
    };
    assert_eq!(body.anchor.as_deref(), Some("ctr"));
    assert_eq!(body.insets, Some([0, 45_720, 91_440, 45_720]));
    assert!(body.wrap);
    assert_eq!(
        body.autofit,
        Autofit::Normal {
            font_scale: 0.925,
            line_spacing_reduction: 0.1
        }
    );
    let p = &body.paragraphs[0];
    assert_eq!((p.level, p.align.as_deref()), (1, Some("ctr")));
    assert_eq!(p.text(), "Ciao\nmondo3");
    let r = &p.runs[0];
    assert_eq!(r.size, Some(24.0));
    assert_eq!(r.bold, Some(true));
    assert_eq!(r.font.as_deref(), Some("Calibri Light"));
    assert_eq!(r.color, Some(Rgba::rgb(0x70, 0xAD, 0x47)));
    assert_eq!(p.runs[2].italic, Some(true));
    assert_eq!(p.runs[3].field.as_deref(), Some("slidenum"));
}

#[test]
fn pictures_media_groups_and_frames() {
    let pic = format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="4" name="Video 3"/><p:cNvPicPr/><p:nvPr><a:videoFile r:link="rIdV"/><p:extLst><p:ext uri="{{DAA4B4D4-6D71-4841-9C94-3DE7FCFB9230}}"><p14:media r:embed="rIdM"/></p:ext></p:extLst></p:nvPr></p:nvPicPr><p:blipFill><a:blip r:embed="rIdI"/><a:srcRect l="10000" b="25000"/><a:stretch/></p:blipFill><p:spPr>{}</p:spPr></p:pic>"#,
        xfrm(1, 1, 2, 2)
    );
    let group = format!(
        r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="8" name="Group 7"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm rot="5400000" flipH="1"><a:off x="10" y="20"/><a:ext cx="100" cy="200"/><a:chOff x="0" y="0"/><a:chExt cx="50" cy="100"/></a:xfrm></p:grpSpPr>{}{}</p:grpSp>"#,
        text_box(9, "<a:p/>"),
        text_box(10, "<a:p/>")
    );
    let table = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="11" name="Table 10"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="5" y="6"/><a:ext cx="7" cy="8"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblGrid><a:gridCol w="1"/><a:gridCol w="1"/><a:gridCol w="1"/></a:tblGrid><a:tr h="1"/><a:tr h="1"/></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#;
    let chart = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="12" name="Chart 11"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="0"/><a:ext cx="1" cy="1"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rIdC"/></a:graphicData></a:graphic></p:graphicFrame>"#;
    let deck = Deck::new()
        .slide(&format!("{pic}{group}{table}{chart}"), "", "")
        .slide_rel("rIdI", "image", "../media/image1.png")
        .slide_rel("rIdV", "video", "../media/media1.mp4")
        .slide_rel(
            "rIdM",
            "http://schemas.microsoft.com/office/2007/relationships/media",
            "../media/media1.mp4",
        )
        .slide_rel("rIdC", "chart", "../charts/chart1.xml")
        .read();
    let shapes = &deck.slides[0].shapes;
    match &shapes[1].kind {
        ShapeKind::Picture { image, crop, media } => {
            assert_eq!(image.as_deref(), Some("ppt/media/image1.png"));
            assert_eq!((crop.left, crop.bottom, crop.top), (0.1, 0.25, 0.0));
            let m = media.as_ref().unwrap();
            assert_eq!(m.kind, MediaKind::Video);
            assert_eq!(m.part.as_deref(), Some("ppt/media/media1.mp4"));
        }
        k => panic!("{k:?}"),
    }
    let g = &shapes[2];
    assert_eq!((g.transform.rotation, g.transform.flip_h), (90.0, true));
    match &g.kind {
        ShapeKind::Group {
            child_space,
            children,
        } => {
            assert_eq!((child_space.cx, child_space.cy), (50, 100));
            assert_eq!(children.iter().map(|c| c.id).collect::<Vec<_>>(), [9, 10]);
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(shapes[3].transform.cx, 7);
    assert_eq!(
        shapes[3].kind,
        ShapeKind::Frame {
            content: FrameContent::Table {
                rows: 2,
                columns: 3
            }
        }
    );
    assert_eq!(
        shapes[4].kind,
        ShapeKind::Frame {
            content: FrameContent::Chart {
                part: Some("ppt/charts/chart1.xml".into())
            }
        }
    );
}

#[test]
fn smartart_points_at_its_stored_drawing() {
    let frame = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="4" name="Diagram 3"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="0"/><a:ext cx="1" cy="1"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram"><dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rIdD" r:lo="rIdL" r:qs="rIdQ" r:cs="rIdS"/></a:graphicData></a:graphic></p:graphicFrame>"#;
    let data = r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><dgm:ptLst/><dgm:extLst><a:ext uri="http://schemas.microsoft.com/office/drawing/2008/diagram"><dsp:dataModelExt xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" relId="rIdW" minVer="http://schemas.openxmlformats.org/drawingml/2006/diagram"/></a:ext></dgm:extLst></dgm:dataModel>"#;
    let deck = Deck::new()
        .slide(frame, "", "")
        .slide_rel("rIdD", "diagramData", "../diagrams/data1.xml")
        .slide_rel(
            "rIdW",
            "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing",
            "../diagrams/drawing1.xml",
        )
        .part("ppt/diagrams/data1.xml", data.as_bytes())
        .read();
    assert_eq!(
        deck.slides[0].shapes[1].kind,
        ShapeKind::Frame {
            content: FrameContent::Diagram {
                drawing: Some("ppt/diagrams/drawing1.xml".into())
            }
        }
    );
}

/// What PowerPoint writes for "Morph, 2 s" with a fade for older readers,
/// and a slide with an automatic effect then two click steps.
#[test]
fn morph_transition_and_click_steps() {
    let transition = r#"<mc:AlternateContent><mc:Choice Requires="p159"><p:transition spd="slow" p14:dur="2000"><p159:morph option="byObject"/></p:transition></mc:Choice><mc:Fallback><p:transition spd="slow"><p:fade/></p:transition></mc:Fallback></mc:AlternateContent>"#;
    let effect = |id: u32, node_type: &str, delay: &str, spid: u32| {
        format!(
            r#"<p:par><p:cTn id="{id}" fill="hold"><p:stCondLst><p:cond delay="{delay}"/></p:stCondLst><p:childTnLst><p:par><p:cTn id="{}" fill="hold"><p:stCondLst><p:cond delay="0"/></p:stCondLst><p:childTnLst><p:par><p:cTn id="{}" presetID="10" presetClass="entr" presetSubtype="0" fill="hold" nodeType="{node_type}"><p:stCondLst><p:cond delay="0"/></p:stCondLst><p:childTnLst><p:set><p:cBhvr><p:cTn id="{}" dur="1" fill="hold"><p:stCondLst><p:cond delay="0"/></p:stCondLst></p:cTn><p:tgtEl><p:spTgt spid="{spid}"><p:txEl><p:pRg st="0" end="0"/></p:txEl></p:spTgt></p:tgtEl><p:attrNameLst><p:attrName>style.visibility</p:attrName></p:attrNameLst></p:cBhvr><p:to><p:strVal val="visible"/></p:to></p:set><p:animEffect transition="in" filter="fade"><p:cBhvr><p:cTn id="{}" dur="500"/><p:tgtEl><p:spTgt spid="{spid}"/></p:tgtEl></p:cBhvr></p:animEffect></p:childTnLst></p:cTn></p:par></p:childTnLst></p:cTn></p:par></p:childTnLst></p:cTn></p:par>"#,
            id + 1,
            id + 2,
            id + 3,
            id + 4
        )
    };
    let timing = format!(
        r#"<p:timing><p:tnLst><p:par><p:cTn id="1" dur="indefinite" restart="never" nodeType="tmRoot"><p:childTnLst><p:seq concurrent="1" nextAc="seek"><p:cTn id="2" dur="indefinite" nodeType="mainSeq"><p:childTnLst>{}{}{}</p:childTnLst></p:cTn><p:prevCondLst><p:cond evt="onPrev" delay="0"><p:tgtEl><p:sldTgt/></p:tgtEl></p:cond></p:prevCondLst><p:nextCondLst><p:cond evt="onNext" delay="0"><p:tgtEl><p:sldTgt/></p:tgtEl></p:cond></p:nextCondLst></p:seq></p:childTnLst></p:cTn></p:par></p:tnLst></p:timing>"#,
        effect(10, "afterEffect", "0", 2),
        effect(20, "clickEffect", "indefinite", 3),
        effect(30, "clickEffect", "indefinite", 2),
    );
    let deck = Deck::new()
        .slide(
            &format!("{}{}", text_box(2, "<a:p/>"), text_box(3, "<a:p/>")),
            &format!("{transition}{timing}"),
            "",
        )
        .read();
    let s = &deck.slides[0];
    let t = s.transition.as_ref().unwrap();
    assert_eq!(t.kind, "morph");
    assert_eq!(t.duration_ms, 2000);
    assert_eq!(
        t.options,
        vec![("option".to_string(), "byObject".to_string())]
    );

    let timing = s.timing.as_ref().unwrap();
    assert_eq!(timing.root.node_type.as_deref(), Some("tmRoot"));
    assert_eq!(timing.root.duration, Some(Time::Indefinite));
    let groups = timing.main_sequence();
    assert_eq!(groups.len(), 3);
    assert_eq!(
        groups.iter().map(|g| g.on_click).collect::<Vec<_>>(),
        [false, true, true]
    );
    assert_eq!(groups[1].effects[0].shapes, vec![3]);
    assert_eq!(groups[1].effects[0].preset.as_ref().unwrap().class, "entr");
    assert_eq!(timing.click_count(), 2);
    assert_eq!(deck.step_count(), 3);
    // The behaviour nodes keep their kind and paragraph targets.
    let effect_node = &timing.root.children[0].children[1].children[0].children[0];
    assert_eq!(
        effect_node.children[0].kind,
        TimeNodeKind::Behavior("set".into())
    );
    assert_eq!(
        effect_node.children[0].target.as_ref().unwrap().paragraphs,
        Some((0, 0))
    );
    assert_eq!(effect_node.children[1].duration, Some(Time::Ms(500)));
}

#[test]
fn speaker_notes() {
    let deck = Deck::new()
        .slide("", "", "")
        .notes("Welcome everyone\nNext: the numbers")
        .read();
    assert_eq!(deck.slides[0].notes, "Welcome everyone\nNext: the numbers");
}

#[test]
fn unsupported_content_is_reported_not_fatal() {
    let ink = r#"<p:contentPart r:id="rIdX"/>"#;
    let deck = Deck::new().slide(ink, "", "").read();
    assert!(
        deck.warnings.iter().any(|w| w.contains("contentPart")),
        "{:?}",
        deck.warnings
    );
}

#[test]
fn a_broken_slide_is_skipped_with_a_warning() {
    let mut files = vec![];
    let good = Deck::new().slide("", "", "").slide("", "", "");
    let bytes = good.bytes();
    // Rebuild the archive with slide1.xml truncated.
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    for i in 0..archive.len() {
        let mut e = archive.by_index(i).unwrap();
        let name = e.name().unwrap().to_string();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut data).unwrap();
        if name == "ppt/slides/slide1.xml" {
            data.truncate(data.len() / 2);
        }
        files.push((name, data));
    }
    let deck = lc_ooxml::read(Cursor::new(zip_of(&files))).unwrap();
    assert_eq!(deck.slides.len(), 1);
    assert!(
        deck.warnings.iter().any(|w| w.contains("slide1.xml")),
        "{:?}",
        deck.warnings
    );
}

#[test]
fn not_a_presentation() {
    assert!(matches!(
        lc_ooxml::read(Cursor::new(b"not a zip at all".to_vec())),
        Err(lc_ooxml::Error::Zip(_))
    ));
    let word = zip_of(&[
        ("_rels/.rels".into(), rels(&[("rId1", "officeDocument", "word/document.xml")]).into_bytes()),
        ("word/document.xml".into(), br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#.to_vec()),
    ]);
    assert!(matches!(
        lc_ooxml::read(Cursor::new(word)),
        Err(lc_ooxml::Error::NotPresentation(_))
    ));
}

#[test]
fn oversized_xml_part_is_refused() {
    // Inflates to just over the limit; compresses to almost nothing.
    let mut big = Vec::with_capacity(lc_ooxml::package::MAX_XML_PART as usize + 2);
    big.extend_from_slice(b"<x>");
    big.resize(lc_ooxml::package::MAX_XML_PART as usize + 1, b' ');
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflated =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    w.start_file("ppt/slides/slide1.xml", deflated).unwrap();
    w.write_all(&big).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert!(bytes.len() < 1 << 20);
    assert!(matches!(
        lc_ooxml::read(Cursor::new(bytes)),
        Err(lc_ooxml::Error::TooLarge { .. })
    ));
}

/// Corrupting the XML of every part in turn, at many positions, must give
/// an error or a deck with warnings, never a panic.
#[test]
fn corrupted_xml_never_panics() {
    let base = Deck::new()
        .slide(&text_box(2, "<a:p><a:r><a:t>x</a:t></a:r></a:p>"), "", "")
        .notes("n");
    let bytes = base.bytes();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut files = Vec::new();
    for i in 0..archive.len() {
        let mut e = archive.by_index(i).unwrap();
        let name = e.name().unwrap().to_string();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut data).unwrap();
        files.push((name, data));
    }
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for part in 0..files.len() {
        for _ in 0..40 {
            let mut copy = files.clone();
            let data = &mut copy[part].1;
            if data.is_empty() {
                continue;
            }
            match next() % 3 {
                0 => {
                    let at = (next() as usize) % data.len();
                    data.truncate(at);
                }
                1 => {
                    let at = (next() as usize) % data.len();
                    data[at] = b"<>\"=/ a0"[(next() % 8) as usize];
                }
                _ => {
                    let at = (next() as usize) % data.len();
                    let len = ((next() as usize) % 40).min(data.len() - at);
                    data.drain(at..at + len);
                }
            }
            let _ = lc_ooxml::read(Cursor::new(zip_of(&copy)));
        }
    }
}
