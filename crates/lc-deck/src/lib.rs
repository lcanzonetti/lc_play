//! # lc-deck — a presentation, independent of the file it was read from
//!
//! The model every presentation reader produces and every presentation
//! consumer reads. `lc-ooxml` fills it from a PowerPoint `.pptx`; a later
//! `lc-keyx` reader will fill it from Keynote's `.keyx`; the renderer, the
//! Asset Manager's import and the Producer's presenter view only ever see
//! this.
//!
//! ```text
//! Deck
//!  ├─ size              slide size in EMU (English Metric Units, below)
//!  ├─ theme             font names the theme names "major" and "minor"
//!  ├─ slides[]          in show order
//!  │    ├─ shapes[]     back to front: master decoration, layout decoration,
//!  │    │               then the slide's own shapes (layer says which)
//!  │    ├─ background
//!  │    ├─ transition   how this slide arrives
//!  │    ├─ timing       the animation tree, as the file states it
//!  │    └─ notes        speaker notes, plain text
//!  └─ warnings[]        what the reader skipped or could not resolve
//! ```
//!
//! ## Design choices
//!
//! * **Resolved, not referenced.** What the file expresses by reference —
//!   a theme colour (`accent1`), a theme font (`+mj-lt`), a placeholder that
//!   takes its position from the layout — is resolved by the reader, so a
//!   consumer never needs the master, the layout or the theme. What is *not*
//!   resolved yet (the cascade of text styles, which belongs to the text
//!   engine) is left as `None` rather than guessed.
//! * **Units are the file's.** Positions and sizes are integer EMU
//!   (914 400 per inch, 12 700 per point), exactly as stored, so nothing is
//!   rounded on the way in; the renderer converts once, to canvas pixels.
//!   Angles are degrees, times milliseconds.
//! * **Nothing silently dropped.** Anything a reader does not understand is
//!   either kept in a generic form (`Fill::Other`, `FrameContent::Other`,
//!   `Geometry::Custom`) or listed in [`Deck::warnings`], so an import can
//!   report it to the operator.
//! * **The timing tree is kept as a tree.** PowerPoint's animations are a
//!   tree of parallel and sequential time containers; evaluating it belongs
//!   to the animation engine (a later phase). What is needed now — how many
//!   clicks a slide takes and which shapes each click animates — is derived
//!   from the tree by [`Timing::main_sequence`].
//! * Every type is `serde`, so a deck can be dumped as JSON for inspection
//!   and for corpus statistics.

use serde::{Deserialize, Serialize};

/// EMU per inch.
pub const EMU_PER_INCH: i64 = 914_400;
/// EMU per typographic point (1/72 inch).
pub const EMU_PER_POINT: i64 = 12_700;

/// A whole presentation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Deck {
    /// Slide size, EMU.
    pub size: Size,
    pub theme: Theme,
    /// In show order (hidden slides included, marked).
    pub slides: Vec<Slide>,
    /// Things the reader skipped, could not find or could not resolve,
    /// one line each, for the operator.
    pub warnings: Vec<String>,
}

impl Deck {
    /// Slides that play in a slideshow (hidden ones left out).
    pub fn shown_slides(&self) -> impl Iterator<Item = &Slide> {
        self.slides.iter().filter(|s| !s.hidden)
    }

    /// Total number of steps an operator presses through: per shown slide,
    /// one step to arrive on it plus one per click its animations wait for.
    pub fn step_count(&self) -> usize {
        self.shown_slides()
            .map(|s| 1 + s.timing.as_ref().map_or(0, Timing::click_count))
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Size {
    pub cx: i64,
    pub cy: i64,
}

/// What a deck keeps from its theme once everything is resolved: the two
/// font families text refers to as "major" (headings) and "minor" (body).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub major_font: String,
    pub minor_font: String,
}

/// A colour in sRGB with straight (not premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Slide {
    /// 1-based position in the file's slide list.
    pub number: u32,
    /// Excluded from the slideshow ("Hide Slide").
    pub hidden: bool,
    /// The name of the layout the slide uses, for people.
    pub layout: String,
    /// Resolved through slide → layout → master; `None` if none of them
    /// sets one (the renderer then uses white, as PowerPoint does).
    pub background: Option<Fill>,
    /// Back to front.
    pub shapes: Vec<Shape>,
    pub transition: Option<Transition>,
    pub timing: Option<Timing>,
    /// Speaker notes as plain text, paragraphs separated by `\n`.
    pub notes: String,
    /// Where the slide came from in its file (e.g. `ppt/slides/slide3.xml`),
    /// for messages.
    pub source: String,
}

/// Which part of the template a shape on a slide comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// Decoration on the slide master (a logo, a footer bar), shown on every
    /// slide that does not turn master shapes off.
    Master,
    /// Decoration on the slide's layout.
    Layout,
    /// The slide's own shapes.
    #[default]
    Slide,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Shape {
    /// The id the file gives the shape, unique per slide part; animations
    /// refer to shapes by it.
    pub id: u32,
    pub name: String,
    pub hidden: bool,
    pub layer: Layer,
    /// Position, size, rotation and flips. For a placeholder that does not
    /// set its own, inherited from the layout or master placeholder.
    pub transform: Transform,
    /// Set when the shape is a placeholder (title, body, picture…).
    pub placeholder: Option<Placeholder>,
    pub kind: ShapeKind,
}

/// Placement of a shape: its unrotated rectangle (EMU), then a rotation
/// about the rectangle's centre and flips.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Transform {
    pub x: i64,
    pub y: i64,
    pub cx: i64,
    pub cy: i64,
    /// Degrees clockwise.
    pub rotation: f64,
    pub flip_h: bool,
    pub flip_v: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Placeholder {
    /// `title`, `ctrTitle`, `subTitle`, `body`, `obj`, `pic`, `dt`, `ftr`,
    /// `sldNum`… as in the file (`obj` when the file names none).
    pub kind: String,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ShapeKind {
    /// A drawn shape, possibly holding text (text boxes are shapes too).
    Shape {
        geometry: Geometry,
        fill: Fill,
        line: Option<Line>,
        text: Option<TextBody>,
    },
    /// A picture, or a video or audio clip shown through its poster frame.
    Picture {
        /// Package path of the image (e.g. `ppt/media/image3.png`).
        image: Option<String>,
        /// Crop of the source image, fractions of its size from each edge.
        crop: Crop,
        media: Option<Media>,
    },
    /// Shapes positioned in the group's own child coordinate space, which
    /// `child_space` maps onto the group's `transform`.
    Group {
        child_space: Rect,
        children: Vec<Shape>,
    },
    /// A table, chart, diagram or embedded object.
    Frame { content: FrameContent },
    /// A line or connector between shapes.
    Connector {
        geometry: Geometry,
        line: Option<Line>,
    },
}

impl Default for ShapeKind {
    fn default() -> Self {
        ShapeKind::Shape {
            geometry: Geometry::None,
            fill: Fill::None,
            line: None,
            text: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub cx: i64,
    pub cy: i64,
}

/// Fractions (0 = no crop) cut from each edge of a picture. Negative values
/// pad instead.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Crop {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Media {
    pub kind: MediaKind,
    /// Package path of the clip, or `None` when it is linked outside the
    /// file (then `link` holds the target as written).
    pub part: Option<String>,
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Audio,
}

/// The outline of a shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Geometry {
    /// No geometry stated (pictures, frames).
    #[default]
    None,
    /// One of the standard's preset shapes (`rect`, `roundRect`,
    /// `rightArrow`…) with its adjustment values, as written (`adj`,
    /// `adj1`… → integer, usually in 1/100000).
    Preset {
        name: String,
        adjust: Vec<(String, i64)>,
    },
    /// A shape drawn with its own path list (`custGeom`). Kept as a marker
    /// for now: the path geometry is read by the shapes phase.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Fill {
    /// Explicitly unfilled, or no fill stated anywhere.
    #[default]
    None,
    Solid {
        color: Rgba,
    },
    /// Gradient stops at positions 0..=1; `angle` in degrees for a linear
    /// gradient, `None` for a path (radial, rectangular…) gradient.
    Gradient {
        stops: Vec<GradientStop>,
        angle: Option<f64>,
    },
    /// An image fill (`blipFill` in a shape or a background).
    Picture {
        image: Option<String>,
    },
    /// A fill kind not read yet (pattern, group fill), by its element name.
    Other {
        kind: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct GradientStop {
    pub position: f64,
    pub color: Rgba,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Line {
    /// Width in EMU (0 = the thinnest line the device can draw).
    pub width: i64,
    pub fill: Fill,
    /// Preset dash name as written (`solid`, `dash`, `sysDot`…).
    pub dash: Option<String>,
}

/// Text inside a shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TextBody {
    pub paragraphs: Vec<Paragraph>,
    /// Vertical anchoring as written: `t`, `ctr`, `b`, `just`, `dist`.
    pub anchor: Option<String>,
    /// Left, top, right, bottom insets in EMU, when stated.
    pub insets: Option<[i64; 4]>,
    /// `false` when the box does not wrap (`wrap="none"`).
    pub wrap: bool,
    pub autofit: Autofit,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Autofit {
    #[default]
    None,
    /// "Shrink text on overflow", with the scale PowerPoint last computed
    /// and stored (fractions; 1.0 = unscaled).
    Normal {
        font_scale: f64,
        line_spacing_reduction: f64,
    },
    /// "Resize shape to fit text".
    Shape,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Paragraph {
    /// Outline level, 0 = top.
    pub level: u8,
    /// Alignment as written: `l`, `ctr`, `r`, `just`, `dist`…
    pub align: Option<String>,
    pub runs: Vec<Run>,
}

impl Paragraph {
    /// The paragraph's text, breaks as `\n`.
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// A stretch of text with one set of character properties. `None` means
/// "not stated here": the value comes from the style cascade (list style,
/// layout and master placeholder, master text styles), which the text
/// engine resolves.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Run {
    /// The text; a line break inside a paragraph is a run of `"\n"`.
    pub text: String,
    /// Points.
    pub size: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    /// Latin typeface, theme references (`+mj-lt`, `+mn-lt`) resolved.
    pub font: Option<String>,
    pub color: Option<Rgba>,
    /// Set for a field (slide number, date…): its type as written. `text`
    /// then holds the value PowerPoint last stored.
    pub field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FrameContent {
    Table {
        rows: u32,
        columns: u32,
    },
    Chart {
        part: Option<String>,
    },
    /// SmartArt: `drawing` is the ready-made drawing PowerPoint stores
    /// beside the data, when present.
    Diagram {
        drawing: Option<String>,
    },
    /// Anything else, by its graphic data URI.
    Other {
        uri: String,
    },
}

/// How a slide arrives.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Transition {
    /// The transition's element name: `fade`, `push`, `wipe`, `morph`,
    /// `vortex`…, or `none` when the slide only sets advance options.
    pub kind: String,
    /// Its attributes as written (`dir`, `thruBlk`, `option`…), for the
    /// transition renderer to interpret.
    pub options: Vec<(String, String)>,
    /// Duration, ms: the file's own (`p14:dur`) when present, otherwise
    /// derived from the speed (`spd`).
    pub duration_ms: u32,
    /// Advance to the next slide on a click (default true).
    pub advance_on_click: bool,
    /// Advance automatically after this many ms.
    pub advance_after_ms: Option<u32>,
}

/// A slide's animation tree as the file states it (`p:timing`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Timing {
    /// The root time node (`tmRoot`).
    pub root: TimeNode,
}

/// A time container or an animation behaviour.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TimeNode {
    pub id: u32,
    pub kind: TimeNodeKind,
    /// `tmRoot`, `mainSeq`, `interactiveSeq`, `clickPar`, `withGroup`,
    /// `afterGroup`, `clickEffect`, `withEffect`, `afterEffect`… as written.
    pub node_type: Option<String>,
    /// Set on the node of one effect, as PowerPoint names it.
    pub preset: Option<EffectPreset>,
    /// When the node starts (`stCondLst`).
    pub begin: Vec<Condition>,
    /// When it ends early (`endCondLst`).
    pub end: Vec<Condition>,
    pub duration: Option<Time>,
    /// `repeatCount` in 1/1000, or `Indefinite`.
    pub repeat: Option<Time>,
    /// What a behaviour acts on.
    pub target: Option<Target>,
    pub children: Vec<TimeNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeNodeKind {
    /// Children run together.
    #[default]
    Par,
    /// Children run one after another.
    Seq,
    /// Only one child at a time.
    Excl,
    /// A behaviour, by element name: `set`, `anim`, `animEffect`,
    /// `animMotion`, `animScale`, `animRot`, `animClr`, `cmd`, `audio`,
    /// `video`.
    Behavior(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EffectPreset {
    /// `entr`, `exit`, `emph`, `path`, `verb`, `mediacall`.
    pub class: String,
    /// PowerPoint's effect number within the class.
    pub id: u32,
    pub subtype: u32,
}

/// A time in ms, or "indefinite" (waits for a click or another event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Time {
    Ms(u32),
    Indefinite,
}

/// A start or end condition: after `delay`, optionally relative to an
/// event (`onBegin`, `onEnd`, `onClick`, `onNext`, `begin`, `end`…) on a
/// shape or another time node.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Condition {
    pub delay: Option<Time>,
    pub event: Option<String>,
    pub shape: Option<u32>,
    pub time_node: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Target {
    pub shape: u32,
    /// Only these paragraphs (inclusive, 0-based), when the effect builds
    /// text paragraph by paragraph.
    pub paragraphs: Option<(u32, u32)>,
}

/// One group of effects in a slide's main sequence: what one click (or,
/// with `on_click` false, one automatic start after the previous group)
/// sets off.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StepGroup {
    /// Waits for a click. `false` for a group that starts on its own after
    /// the previous one (including one that starts as the slide appears).
    pub on_click: bool,
    pub effects: Vec<EffectSummary>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EffectSummary {
    pub preset: Option<EffectPreset>,
    /// `clickEffect`, `withEffect` or `afterEffect`.
    pub node_type: Option<String>,
    /// Shapes the effect's behaviours act on, sorted, without repeats.
    pub shapes: Vec<u32>,
}

impl Timing {
    /// The main sequence's groups in order. Each child of the `mainSeq`
    /// container is one group; it waits for a click when its start
    /// condition has an indefinite delay (how PowerPoint writes "On
    /// Click"). Shape-triggered animations (`interactiveSeq`) are not part
    /// of it; see [`Timing::interactive_sequences`].
    pub fn main_sequence(&self) -> Vec<StepGroup> {
        let Some(main) = find(&self.root, &|n| n.node_type.as_deref() == Some("mainSeq")) else {
            return Vec::new();
        };
        main.children
            .iter()
            .map(|group| StepGroup {
                on_click: waits_for_click(group),
                effects: effects_in(group),
            })
            .collect()
    }

    /// How many clicks the slide's animations wait for before the next
    /// click advances to the next slide.
    pub fn click_count(&self) -> usize {
        self.main_sequence().iter().filter(|g| g.on_click).count()
    }

    /// Number of animation sequences started by clicking a shape
    /// (triggers). Not played by LC Play; counted so an import can say so.
    pub fn interactive_sequences(&self) -> usize {
        let mut n = 0;
        visit(&self.root, &mut |node| {
            if node.node_type.as_deref() == Some("interactiveSeq") {
                n += 1;
            }
        });
        n
    }
}

fn waits_for_click(node: &TimeNode) -> bool {
    node.begin
        .iter()
        .any(|c| c.delay == Some(Time::Indefinite) && c.event.is_none())
}

fn effects_in(group: &TimeNode) -> Vec<EffectSummary> {
    let mut out = Vec::new();
    visit(group, &mut |node| {
        let is_effect = node.preset.is_some()
            || matches!(
                node.node_type.as_deref(),
                Some("clickEffect" | "withEffect" | "afterEffect")
            );
        if is_effect {
            let mut shapes = Vec::new();
            visit(node, &mut |n| {
                if let Some(t) = &n.target {
                    shapes.push(t.shape);
                }
            });
            shapes.sort_unstable();
            shapes.dedup();
            out.push(EffectSummary {
                preset: node.preset.clone(),
                node_type: node.node_type.clone(),
                shapes,
            });
        }
    });
    out
}

fn find<'a>(node: &'a TimeNode, pred: &dyn Fn(&TimeNode) -> bool) -> Option<&'a TimeNode> {
    if pred(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find(c, pred))
}

/// Depth-first, parents before children. An effect node's own subtree is
/// visited too, but effects are not nested in practice.
fn visit(node: &TimeNode, f: &mut dyn FnMut(&TimeNode)) {
    f(node);
    for c in &node.children {
        visit(c, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(node_type: &str, shape: u32) -> TimeNode {
        TimeNode {
            node_type: Some(node_type.into()),
            preset: Some(EffectPreset {
                class: "entr".into(),
                id: 10,
                subtype: 0,
            }),
            children: vec![TimeNode {
                kind: TimeNodeKind::Behavior("set".into()),
                target: Some(Target {
                    shape,
                    paragraphs: None,
                }),
                ..TimeNode::default()
            }],
            ..TimeNode::default()
        }
    }

    fn group(delay: Time, effects: Vec<TimeNode>) -> TimeNode {
        TimeNode {
            begin: vec![Condition {
                delay: Some(delay),
                ..Condition::default()
            }],
            // PowerPoint nests effects one level deeper (click group →
            // with/after group → effect); the summary must look through it.
            children: vec![TimeNode {
                children: effects,
                ..TimeNode::default()
            }],
            ..TimeNode::default()
        }
    }

    fn timing(groups: Vec<TimeNode>) -> Timing {
        Timing {
            root: TimeNode {
                node_type: Some("tmRoot".into()),
                children: vec![TimeNode {
                    kind: TimeNodeKind::Seq,
                    node_type: Some("mainSeq".into()),
                    children: groups,
                    ..TimeNode::default()
                }],
                ..TimeNode::default()
            },
        }
    }

    #[test]
    fn counts_click_groups_and_not_automatic_ones() {
        let t = timing(vec![
            // Starts by itself as the slide appears.
            group(Time::Ms(0), vec![effect("afterEffect", 2)]),
            group(
                Time::Indefinite,
                vec![effect("clickEffect", 3), effect("withEffect", 4)],
            ),
            group(Time::Indefinite, vec![effect("clickEffect", 5)]),
        ]);
        let steps = t.main_sequence();
        assert_eq!(steps.len(), 3);
        assert!(!steps[0].on_click);
        assert_eq!(t.click_count(), 2);
        assert_eq!(steps[1].effects.len(), 2);
        assert_eq!(steps[1].effects[0].shapes, vec![3]);
        assert_eq!(steps[1].effects[1].shapes, vec![4]);
    }

    #[test]
    fn no_main_sequence_means_no_clicks() {
        let t = Timing::default();
        assert!(t.main_sequence().is_empty());
        assert_eq!(t.click_count(), 0);
    }

    #[test]
    fn step_count_skips_hidden_slides() {
        let animated = Slide {
            timing: Some(timing(vec![group(
                Time::Indefinite,
                vec![effect("clickEffect", 2)],
            )])),
            ..Slide::default()
        };
        let deck = Deck {
            slides: vec![
                animated.clone(),
                Slide {
                    hidden: true,
                    ..animated
                },
                Slide::default(),
            ],
            ..Deck::default()
        };
        // Slide 1: arrive + 1 click; slide 2 hidden; slide 3: arrive.
        assert_eq!(deck.step_count(), 3);
    }

    #[test]
    fn json_round_trip() {
        let deck = Deck {
            size: Size {
                cx: 12_192_000,
                cy: 6_858_000,
            },
            slides: vec![Slide {
                number: 1,
                shapes: vec![Shape {
                    id: 2,
                    kind: ShapeKind::Shape {
                        geometry: Geometry::Preset {
                            name: "rect".into(),
                            adjust: vec![],
                        },
                        fill: Fill::Solid {
                            color: Rgba::rgb(255, 0, 0),
                        },
                        line: None,
                        text: None,
                    },
                    ..Shape::default()
                }],
                ..Slide::default()
            }],
            ..Deck::default()
        };
        let json = serde_json::to_string(&deck).unwrap();
        let back: Deck = serde_json::from_str(&json).unwrap();
        assert_eq!(back, deck);
    }
}
