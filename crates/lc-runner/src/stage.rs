//! From the cluster's show document to what this Runner renders.
//!
//! The Director distributes one [`ShowDocument`] to everybody. A Runner draws
//! only *its* displays, and the renderer wants its own, flatter model
//! ([`crate::show::Show`]: outputs and layers). This module is the
//! translation:
//!
//! * displays assigned to this Runner (by name; an empty assignment means
//!   any Runner) become outputs;
//! * every cue of every track becomes a layer — all of them, not only those
//!   over this Runner's displays: the renderer clips, and a cue may be moved
//!   onto this Runner's part of the canvas at any moment;
//! * asset references become local file paths via the [`AssetCache`], or a
//!   placeholder while the file is still on its way.

use std::sync::Arc;

use lc_proto::show::{AssetKind, CueSource, ShowDocument};

use crate::assets::AssetCache;
use crate::show::{
    Blend, Canvas, Layer, Output, Playback, Rect, Show, Source, Timing, Transform, Warp,
    WindowPlacement,
};

pub fn build(doc: &ShowDocument, runner_name: &str, assets: &Arc<AssetCache>) -> Show {
    let outputs = doc
        .stage
        .displays
        .iter()
        .filter(|d| d.runner.is_empty() || d.runner == runner_name)
        .map(|d| Output {
            id: if d.name.is_empty() {
                d.id.clone()
            } else {
                d.name.clone()
            },
            canvas_rect: Rect {
                x: d.canvas_rect.x,
                y: d.canvas_rect.y,
                w: d.canvas_rect.w,
                h: d.canvas_rect.h,
            },
            window: WindowPlacement {
                fullscreen: d.window.fullscreen,
                monitor: d.window.monitor,
                position: d.window.position,
                size: d.window.size,
            },
            rotation_degrees: d.rotation_degrees,
            warp: Warp {
                corners: d.warp_corners,
            },
            blend: Blend {
                left: d.blend.left,
                right: d.blend.right,
                top: d.blend.top,
                bottom: d.blend.bottom,
                gamma: d.blend.gamma,
            },
        })
        // The Director validates documents, but a Runner must not trust the
        // network with something that could panic the GPU code.
        .filter(|o| match o.validate() {
            Ok(()) => true,
            Err(e) => {
                tracing::error!("ignoring display: {e:#}");
                false
            }
        })
        .collect();

    let track_count = doc.tracks.len();
    let mut layers = Vec::new();
    for (ti, track) in doc.tracks.iter().enumerate() {
        for (ci, cue) in track.cues.iter().enumerate() {
            let source = match &cue.source {
                CueSource::Color { rgba } => Source::Color { rgba: *rgba },
                CueSource::Asset { asset } => match doc.asset(asset) {
                    None => Source::Missing {
                        reason: format!("asset '{asset}' is not in the show"),
                    },
                    Some(a) => match assets.local_path(a) {
                        None => Source::Missing {
                            reason: format!("asset '{}' is still downloading", a.name),
                        },
                        Some(path) => {
                            let path = path.to_string_lossy().into_owned();
                            match a.kind {
                                AssetKind::Image => Source::Image { path },
                                AssetKind::Video => Source::Video { path },
                            }
                        }
                    },
                },
            };
            layers.push(Layer {
                id: if cue.id.is_empty() {
                    format!("t{ti}c{ci}")
                } else {
                    cue.id.clone()
                },
                source,
                transform: Transform {
                    // The renderer positions a layer by its anchor (centre).
                    position: [cue.x + cue.w / 2.0, cue.y + cue.h / 2.0],
                    size: [cue.w, cue.h],
                    anchor: [0.5, 0.5],
                    scale: [1.0, 1.0],
                    rotation: cue.rotation,
                    opacity: cue.opacity.clamp(0.0, 1.0),
                    // First track in front; within a track, later cues above
                    // earlier ones (they normally do not overlap in time).
                    z: (track_count - ti) as f32 + ci as f32 * 1e-4,
                },
                timing: Timing {
                    start: cue.start_s,
                    end: Some(cue.start_s + cue.duration_s),
                    fade_in: cue.fade_in_s,
                    fade_out: cue.fade_out_s,
                    media_in: cue.in_s,
                    media_loop: cue.looping,
                },
            });
        }
    }

    Show {
        canvas: Canvas {
            width: doc.stage.width,
            height: doc.stage.height,
            background: doc.stage.background,
        },
        outputs,
        layers,
        // Unused under a Director: the transport comes from the cluster.
        playback: Playback::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> ShowDocument {
        serde_json::from_value(serde_json::json!({
            "stage": { "width": 3840, "height": 1080, "displays": [
                { "id": "a", "name": "Left",  "runner": "left",  "canvas_rect": { "x": 0,    "w": 1920, "h": 1080 } },
                { "id": "b", "name": "Right", "runner": "right", "canvas_rect": { "x": 1920, "w": 1920, "h": 1080 } },
                { "id": "c", "name": "Any",   "runner": "",      "canvas_rect": { "w": 3840, "h": 1080 } },
                { "id": "d", "name": "Broken","runner": "left",  "canvas_rect": { "w": 0, "h": 0 } }
            ] },
            "assets": [ { "id": "gone", "name": "gone.png", "kind": "image", "url": "http://127.0.0.1:1/x", "size": 9 } ],
            "tracks": [
                { "cues": [ { "id": "front", "source": { "type": "color", "rgba": [1,0,0,1] },
                              "x": 100, "y": 200, "w": 400, "h": 300, "start_s": 5, "duration_s": 10,
                              "fade_in_s": 1, "opacity": 0.5 } ] },
                { "cues": [ { "id": "back",  "source": { "type": "asset", "asset": "gone" } },
                            { "id": "lost",  "source": { "type": "asset", "asset": "nope" } } ] }
            ]
        }))
        .unwrap()
    }

    fn cache() -> Arc<AssetCache> {
        AssetCache::new(
            std::env::temp_dir().join(format!("lcplay-stage-test-{}", std::process::id())),
        )
    }

    #[test]
    fn a_runner_gets_its_own_displays_and_the_unassigned_ones() {
        let show = build(&doc(), "left", &cache());
        let ids: Vec<&str> = show.outputs.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["Left", "Any"], "not Right's, and not the invalid one");
        assert_eq!(build(&doc(), "nobody", &cache()).outputs.len(), 1);
    }

    #[test]
    fn cues_become_layers_with_centre_anchor_timing_and_stacking() {
        let show = build(&doc(), "left", &cache());
        assert_eq!(show.layers.len(), 3);
        let front = &show.layers[0];
        assert_eq!(front.transform.position, [300.0, 350.0]);
        assert_eq!(front.transform.size, [400.0, 300.0]);
        assert_eq!((front.timing.start, front.timing.end), (5.0, Some(15.0)));
        assert_eq!(front.timing.fade_at(5.5), 0.5);
        assert!(
            front.transform.z > show.layers[1].transform.z,
            "first track is in front"
        );
    }

    #[test]
    fn unavailable_media_becomes_a_placeholder_not_an_error() {
        let show = build(&doc(), "left", &cache());
        assert!(
            matches!(&show.layers[1].source, Source::Missing { reason } if reason.contains("downloading"))
        );
        assert!(
            matches!(&show.layers[2].source, Source::Missing { reason } if reason.contains("not in the show"))
        );
    }
}
