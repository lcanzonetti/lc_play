//! The **show document**: everything an operator authors, in one JSON value.
//!
//! ```text
//! ShowDocument
//!  ├─ stage      the canvas (one big pixel space) and the displays cut from it
//!  ├─ assets[]   media known to the show, with where to fetch each
//!  └─ tracks[]   the timeline: each track holds cues placed in time and space
//! ```
//!
//! The Producer edits it, the Director stores and distributes it, each Runner
//! renders the part that concerns it. It is deliberately one self-contained
//! document rather than a set of deltas: small enough to send whole on every
//! edit, and a Runner holding it needs nothing else to play the show.
//!
//! Conventions:
//!
//! * **Space** is canvas pixels, origin top-left, y down. A cue's `x, y, w,
//!   h` is its rectangle on the canvas; a display's `canvas_rect` is the part
//!   of the canvas that display shows. Content that crosses a display
//!   boundary simply appears on both — spanning needs no special case.
//! * **Time** is seconds on the show timeline (`f64`). The *transport* that
//!   says which show time it is now is exact integer nanoseconds (see
//!   [`crate::transport`]); authored positions do not need that.
//! * **Stacking**: the first track is in front, like the top layer of a
//!   timeline editor.
//! * Every field has a default, so a document can be written by hand with
//!   only what matters, and older documents stay loadable as fields are added.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowDocument {
    pub name: String,
    pub stage: Stage,
    pub assets: Vec<Asset>,
    /// Length of the timeline in seconds (what the Producer shows and what
    /// "loop the show" wraps at).
    pub duration_s: f64,
    pub tracks: Vec<Track>,
    /// Control cues of the main timeline.
    pub control_cues: Vec<ControlCue>,
    /// Auxiliary timelines ("tasks"): independent timelines with their own
    /// transport, started and stopped separately from the main one — by the
    /// operator, by show control, or by a control cue. They render in front
    /// of the main timeline.
    pub tasks: Vec<Timeline>,
}

/// An auxiliary timeline. (The main timeline is the show's own `tracks`,
/// `duration_s` and `control_cues`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeline {
    pub id: String,
    pub name: String,
    pub duration_s: f64,
    pub tracks: Vec<Track>,
    pub control_cues: Vec<ControlCue>,
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline {
            id: String::new(),
            name: String::new(),
            duration_s: 30.0,
            tracks: Vec::new(),
            control_cues: Vec::new(),
        }
    }
}

/// What a control cue does when its timeline's playhead reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlAction {
    #[default]
    Pause,
    Play,
    Stop,
    /// Move the target timeline to `to_s`.
    Jump,
}

/// A point on a timeline that acts on a transport: "pause here", "start
/// that task", "jump back to 10 s".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlCue {
    pub id: String,
    pub name: String,
    /// Timeline time at which it fires.
    pub at_s: f64,
    pub action: ControlAction,
    /// The timeline acted on: empty = the timeline this cue is on, `"main"`
    /// = the main timeline, otherwise an auxiliary timeline's id.
    pub target: String,
    /// `Jump`: where to.
    pub to_s: f64,
}

/// Name by which a control cue refers to the main timeline.
pub const MAIN_TIMELINE: &str = "main";

impl Default for ShowDocument {
    fn default() -> Self {
        ShowDocument {
            name: "Untitled".into(),
            stage: Stage::default(),
            assets: Vec::new(),
            duration_s: 60.0,
            tracks: Vec::new(),
            control_cues: Vec::new(),
            tasks: Vec::new(),
        }
    }
}

/// The canvas and the displays that show parts of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage {
    pub width: f32,
    pub height: f32,
    /// Colour behind all cues, linear RGBA 0..1.
    pub background: [f32; 4],
    pub displays: Vec<Display>,
}

impl Default for Stage {
    fn default() -> Self {
        Stage {
            width: 1920.0,
            height: 1080.0,
            background: [0.0, 0.0, 0.0, 1.0],
            displays: Vec::new(),
        }
    }
}

/// One physical output: a rectangle of the canvas, driven by one Runner.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    pub id: String,
    pub name: String,
    /// Name of the Runner that drives this display (its `--name`). Empty
    /// means "any Runner" — convenient on a single machine.
    pub runner: String,
    pub canvas_rect: Rect,
    pub window: WindowSpec,
    /// Physical rotation of the display: 0, 90, 180 or 270.
    pub rotation_degrees: u16,
    /// Corner-pin warp: TL, TR, BR, BL in normalised output space (0..1).
    /// `None` = no warp.
    pub warp_corners: Option<[[f32; 2]; 4]>,
    pub blend: EdgeBlend,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Where a display's window goes on the Runner's desktop.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSpec {
    /// Borderless fullscreen on `monitor` — the setting for a real output.
    pub fullscreen: bool,
    /// Index into the Runner's monitor list (logged at Runner start-up).
    pub monitor: Option<usize>,
    /// Windowed mode: top-left corner in desktop pixels.
    pub position: Option<[i32; 2]>,
    /// Windowed mode: size in pixels. `None` = the canvas rect's size.
    pub size: Option<[u32; 2]>,
}

/// Soft-edge blend widths as a fraction of the output (0..1) per edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeBlend {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    /// Projector gamma; 2.2 means no correction.
    pub gamma: f32,
}

impl Default for EdgeBlend {
    fn default() -> Self {
        EdgeBlend {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            gamma: 2.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    #[default]
    Image,
    Video,
}

/// A media file the show uses, and where a Runner can fetch it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Asset {
    /// The Asset Manager's id for the file.
    pub id: String,
    /// File name, for people (and for the file extension).
    pub name: String,
    pub kind: AssetKind,
    /// HTTP URL of the original file on the Asset Manager.
    pub url: String,
    /// Size in bytes. Part of a Runner's cache key, so a file replaced under
    /// the same name is fetched again.
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Zero for stills.
    pub duration_s: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub cues: Vec<Cue>,
    /// Free-running: video on this track loops on the cluster clock instead
    /// of following the timeline's position. It keeps moving while the
    /// timeline is paused and is unaffected by seeking — for backgrounds and
    /// ambient loops. The cues still decide *when* it is visible.
    ///
    /// The loop's phase is tied to cluster time, not to the moment the cue
    /// appeared: every Runner shows the same frame, but the clip does not
    /// necessarily start from its first frame.
    pub free_run: bool,
}

/// What a cue shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CueSource {
    /// An entry of [`ShowDocument::assets`], by id.
    Asset { asset: String },
    /// A solid colour, linear RGBA 0..1. Useful with no media at all.
    Color { rgba: [f32; 4] },
}

impl Default for CueSource {
    fn default() -> Self {
        CueSource::Color {
            rgba: [0.5, 0.5, 0.5, 1.0],
        }
    }
}

/// One piece of content, placed in time (`start_s`, `duration_s`) and on the
/// canvas (`x, y, w, h`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cue {
    pub id: String,
    pub name: String,
    pub source: CueSource,
    /// Show time at which the cue appears.
    pub start_s: f64,
    pub duration_s: f64,
    /// Video: where in the file playback starts.
    pub in_s: f64,
    /// Video: restart from `in_s` when the file ends (otherwise the last
    /// frame is held).
    #[serde(rename = "loop")]
    pub looping: bool,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Degrees clockwise about the cue's centre.
    pub rotation: f32,
    pub opacity: f32,
    pub fade_in_s: f64,
    pub fade_out_s: f64,
}

impl Default for Cue {
    fn default() -> Self {
        Cue {
            id: String::new(),
            name: String::new(),
            source: CueSource::default(),
            start_s: 0.0,
            duration_s: 10.0,
            in_s: 0.0,
            looping: false,
            x: 0.0,
            y: 0.0,
            w: 640.0,
            h: 360.0,
            rotation: 0.0,
            opacity: 1.0,
            fade_in_s: 0.0,
            fade_out_s: 0.0,
        }
    }
}

/// Largest display edge accepted, in pixels (the texture limit of current
/// GPUs). A larger value is a typo — and one that would crash a renderer.
pub const MAX_OUTPUT_PIXELS: u32 = 16_384;

/// True if the four points form a strictly convex quad (either winding).
/// Anything else has no valid perspective mapping.
fn is_convex(c: &[[f32; 2]; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b, d) = (c[i], c[(i + 1) % 4], c[(i + 2) % 4]);
        let cross = (b[0] - a[0]) * (d[1] - b[1]) - (b[1] - a[1]) * (d[0] - b[0]);
        if cross.abs() < 1e-9 || (sign != 0.0 && cross.signum() != sign) {
            return false;
        }
        sign = cross.signum();
    }
    true
}

impl ShowDocument {
    pub fn asset(&self, id: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.id == id)
    }

    /// Check the document for values that cannot be rendered. Returns the
    /// first problem as a message an operator can act on.
    ///
    /// The Director runs this before accepting a document, so a malformed
    /// one is refused at the door instead of reaching every Runner.
    pub fn validate(&self) -> Result<(), String> {
        let finite = |what: &str, values: &[f64]| {
            if values.iter().all(|v| v.is_finite()) {
                Ok(())
            } else {
                Err(format!("{what}: contains a value that is not a number"))
            }
        };

        let s = &self.stage;
        finite("stage", &[s.width as f64, s.height as f64])?;
        finite("stage background", &s.background.map(f64::from))?;
        if !(s.width > 0.0 && s.height > 0.0) {
            return Err("stage: width and height must be positive".into());
        }
        if !(self.duration_s.is_finite() && self.duration_s > 0.0) {
            return Err("show: duration must be positive".into());
        }

        for d in &s.displays {
            let name = format!(
                "display '{}'",
                if d.name.is_empty() { &d.id } else { &d.name }
            );
            let r = &d.canvas_rect;
            finite(&name, &[r.x as f64, r.y as f64, r.w as f64, r.h as f64])?;
            if !(r.w > 0.0 && r.h > 0.0) {
                return Err(format!(
                    "{name}: canvas rect must have positive width and height"
                ));
            }
            if r.w > MAX_OUTPUT_PIXELS as f32 || r.h > MAX_OUTPUT_PIXELS as f32 {
                return Err(format!(
                    "{name}: canvas rect must not exceed {MAX_OUTPUT_PIXELS} pixels per side"
                ));
            }
            if let Some(corners) = d.warp_corners {
                let flat: Vec<f64> = corners.iter().flatten().map(|&v| v as f64).collect();
                finite(&name, &flat)?;
                if !is_convex(&corners) {
                    return Err(format!(
                        "{name}: warp corners must form a convex quad in the order TL, TR, BR, BL"
                    ));
                }
            }
            if !matches!(d.rotation_degrees, 0 | 90 | 180 | 270) {
                return Err(format!("{name}: rotation must be 0, 90, 180 or 270"));
            }
            let b = &d.blend;
            let edges = [b.left, b.right, b.top, b.bottom];
            if edges.iter().any(|e| !(0.0..=1.0).contains(e))
                || b.left + b.right > 1.0
                || b.top + b.bottom > 1.0
            {
                return Err(format!(
                    "{name}: blend edges must be 0..1 and opposite edges must not exceed 1 together"
                ));
            }
            if !(b.gamma.is_finite() && b.gamma > 0.0) {
                return Err(format!("{name}: blend gamma must be positive"));
            }
            if let Some(size) = d.window.size {
                if size.iter().any(|&d| d == 0 || d > MAX_OUTPUT_PIXELS) {
                    return Err(format!(
                        "{name}: window size must be 1..{MAX_OUTPUT_PIXELS} pixels per side"
                    ));
                }
            }
        }

        // Every timeline: the main one and each task.
        let main = (
            MAIN_TIMELINE,
            self.duration_s,
            &self.tracks,
            &self.control_cues,
        );
        let tasks = self
            .tasks
            .iter()
            .map(|t| (t.id.as_str(), t.duration_s, &t.tracks, &t.control_cues));
        let mut seen = std::collections::HashSet::new();
        for task in &self.tasks {
            if task.id.is_empty() || task.id == MAIN_TIMELINE {
                return Err(format!(
                    "task '{}': needs an id other than '' or 'main'",
                    task.name
                ));
            }
            if !seen.insert(task.id.as_str()) {
                return Err(format!("task '{}': duplicate id '{}'", task.name, task.id));
            }
        }
        for (timeline_id, duration_s, tracks, control_cues) in std::iter::once(main).chain(tasks) {
            if !(duration_s.is_finite() && duration_s > 0.0) {
                return Err(format!(
                    "timeline '{timeline_id}': duration must be positive"
                ));
            }
            for cc in control_cues {
                let name = format!("control cue at {} s on '{timeline_id}'", cc.at_s);
                finite(&name, &[cc.at_s, cc.to_s])?;
                if cc.at_s < 0.0 || cc.to_s < 0.0 {
                    return Err(format!("{name}: times must not be negative"));
                }
                let known = cc.target.is_empty()
                    || cc.target == MAIN_TIMELINE
                    || self.tasks.iter().any(|t| t.id == cc.target);
                if !known {
                    return Err(format!("{name}: targets unknown timeline '{}'", cc.target));
                }
            }
            for track in tracks {
                for c in &track.cues {
                    let name = format!("cue '{}'", if c.name.is_empty() { &c.id } else { &c.name });
                    finite(
                        &name,
                        &[
                            c.start_s,
                            c.duration_s,
                            c.in_s,
                            c.fade_in_s,
                            c.fade_out_s,
                            c.x as f64,
                            c.y as f64,
                            c.w as f64,
                            c.h as f64,
                            c.rotation as f64,
                            c.opacity as f64,
                        ],
                    )?;
                    if c.duration_s <= 0.0 {
                        return Err(format!("{name}: duration must be positive"));
                    }
                    if c.start_s < 0.0 || c.in_s < 0.0 || c.fade_in_s < 0.0 || c.fade_out_s < 0.0 {
                        return Err(format!("{name}: times must not be negative"));
                    }
                    if !(c.w > 0.0 && c.h > 0.0) {
                        return Err(format!("{name}: width and height must be positive"));
                    }
                    match &c.source {
                        CueSource::Asset { asset } => {
                            if self.asset(asset).is_none() {
                                return Err(format!(
                                    "{name}: refers to asset '{asset}', which is not in the show"
                                ));
                            }
                        }
                        CueSource::Color { rgba } => finite(&name, &rgba.map(f64::from))?,
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_object_is_a_valid_default_show() {
        let doc: ShowDocument = serde_json::from_str("{}").unwrap();
        assert_eq!(doc, ShowDocument::default());
        doc.validate().unwrap();
    }

    #[test]
    fn hand_written_documents_need_only_what_matters() {
        let doc: ShowDocument = serde_json::from_str(
            r#"{
              "stage": { "width": 3840, "height": 1080,
                         "displays": [ { "id": "l", "canvas_rect": { "w": 1920, "h": 1080 } } ] },
              "tracks": [ { "cues": [
                 { "source": { "type": "color", "rgba": [1, 0, 0, 1] }, "w": 100, "h": 100, "loop": true }
              ] } ]
            }"#,
        )
        .unwrap();
        doc.validate().unwrap();
        let cue = &doc.tracks[0].cues[0];
        assert!(cue.looping);
        assert_eq!(cue.opacity, 1.0, "defaults fill the rest");
        // And it survives a round trip unchanged.
        let again: ShowDocument =
            serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(again, doc);
    }

    #[test]
    fn validation_names_the_problem() {
        let mut doc = ShowDocument::default();
        doc.stage.displays.push(Display {
            name: "Left".into(),
            ..Default::default() // zero-sized canvas rect
        });
        assert!(doc.validate().unwrap_err().contains("display 'Left'"));

        let mut doc = ShowDocument::default();
        doc.tracks.push(Track {
            cues: vec![Cue {
                name: "logo".into(),
                source: CueSource::Asset {
                    asset: "missing".into(),
                },
                ..Default::default()
            }],
            ..Default::default()
        });
        let err = doc.validate().unwrap_err();
        assert!(
            err.contains("cue 'logo'") && err.contains("missing"),
            "{err}"
        );

        let mut doc = ShowDocument::default();
        doc.stage.width = f32::NAN;
        assert!(doc.validate().is_err());

        // A typo'd size (19200 for 1920) must be refused here, not reach a
        // Runner's GPU; a bow-tie warp has no perspective mapping.
        let display = |edit: &dyn Fn(&mut Display)| {
            let mut d = Display {
                name: "Wall".into(),
                canvas_rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                },
                ..Default::default()
            };
            edit(&mut d);
            let mut doc = ShowDocument::default();
            doc.stage.displays.push(d);
            doc.validate()
        };
        let bow_tie = [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]];
        let square = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        assert!(display(&|_| {}).is_ok());
        assert!(display(&|d| d.canvas_rect.w = 40_000.0).is_err());
        assert!(display(&|d| d.window.size = Some([19_200, 1080])).is_err());
        assert!(display(&|d| d.warp_corners = Some(bow_tie)).is_err());
        assert!(display(&|d| d.warp_corners = Some(square)).is_ok());
    }
}
