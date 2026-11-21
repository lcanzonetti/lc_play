//! Virtual displays: areas of the canvas rendered into a picture that cues
//! then show, instead of going to a screen (WATCHOUT's "virtual display").
//!
//! ```text
//!   canvas ──(virtual V: its rect, at its resolution)──▶ V's picture
//!                                                          │
//!   layer with source `virtual V` ◀── samples ─────────────┘
//!     └─▶ drawn like any layer, on outputs or in another virtual
//! ```
//!
//! Each Runner renders the virtual displays it needs itself, on each card
//! that needs them, every frame, before its outputs. Nothing travels over
//! the network: every Runner computes the same frame from the same show
//! and transport, so the picture of a virtual display is the same on all of
//! them, and on the same frame.
//!
//! **What is rendered.** Only what a picture on this card's outputs depends
//! on ([`plan`]): the virtuals shown by a layer on air over one of the
//! outputs, then the virtuals shown by a layer on air inside *those*, and so
//! on. A Runner whose outputs show no virtual display renders none.
//!
//! **Order and loops.** A virtual is rendered after every virtual that its
//! area shows, so a chain (V2 shows V1, the outputs show V2) is all current
//! on the same frame. A loop (V shows itself, or V1 shows V2 which shows V1)
//! has no first element: the plan is a depth-first walk, and a layer whose
//! virtual has not been rendered yet when its turn comes is left out of
//! that picture. So in a loop one link is dropped — there is never
//! feedback, and never a picture from a previous frame.
//!
//! **What a virtual captures:** every layer on air in its area, of every
//! timeline, over a *transparent* background (not the stage colour), so
//! the captured picture keeps its alpha. Display test patterns are not
//! captured: they belong to their display.

use std::collections::HashMap;

use crate::show::{Rect, Show, Source, Transform};

/// Whether a layer may cover any part of `r`. Conservative: the layer is
/// taken as a circle around its anchor point as wide as its diagonal, which
/// covers any rotation and anchor.
pub fn overlaps(t: &Transform, r: &Rect) -> bool {
    let reach = (t.size[0] * t.scale[0]).hypot(t.size[1] * t.scale[1]);
    let [x, y] = t.position;
    x + reach >= r.x && x - reach <= r.x + r.w && y + reach >= r.y && y - reach <= r.y + r.h
}

/// The virtual displays (indices into `show.virtuals`) that the pictures of
/// `areas` (the canvas areas of the outputs being rendered) need, in the
/// order to render them: each after every virtual it shows. `live(i)` says
/// whether layer `i` counts — on air now, for rendering; on air or about to
/// be, for deciding what to decode.
pub fn plan(show: &Show, live: impl Fn(usize) -> bool, areas: &[Rect]) -> Vec<usize> {
    let index: HashMap<&str, usize> = show
        .virtuals
        .iter()
        .enumerate()
        .map(|(i, v)| (v.id.as_str(), i))
        .collect();
    // Every live layer that shows a virtual display: which, and where.
    let shows: Vec<(usize, &Transform)> = show
        .layers
        .iter()
        .enumerate()
        .filter(|&(i, l)| l.only_output.is_none() && live(i))
        .filter_map(|(_, l)| match &l.source {
            Source::Virtual { id } => index.get(id.as_str()).map(|&v| (v, &l.transform)),
            _ => None,
        })
        .collect();

    #[derive(Clone, Copy, PartialEq)]
    enum Seen {
        No,
        /// Being planned: what it shows is being visited (a loop leads here).
        Visiting,
        Planned,
    }
    fn visit(
        v: usize,
        show: &Show,
        shows: &[(usize, &Transform)],
        seen: &mut [Seen],
        order: &mut Vec<usize>,
    ) {
        seen[v] = Seen::Visiting;
        let area = show.virtuals[v].canvas_rect;
        for &(w, t) in shows {
            if seen[w] == Seen::No && overlaps(t, &area) {
                visit(w, show, shows, seen, order);
            }
        }
        seen[v] = Seen::Planned;
        order.push(v);
    }

    let mut seen = vec![Seen::No; show.virtuals.len()];
    let mut order = Vec::new();
    for &(v, t) in &shows {
        if seen[v] == Seen::No && areas.iter().any(|a| overlaps(t, a)) {
            visit(v, show, &shows, &mut seen, &mut order);
        }
    }
    order
}

/// The virtual displays the pictures of `areas` need (as [`plan`]), each
/// with its canvas area and the outputs (indices into `areas`) whose picture
/// shows it, directly or through other virtual displays. A layer inside one
/// of those areas is on those outputs as much as a layer over them: a card
/// counts its late or split frames there.
pub fn shown_on(
    show: &Show,
    live: impl Fn(usize) -> bool,
    areas: &[Rect],
) -> Vec<(Rect, Vec<usize>)> {
    let mut outputs: Vec<Vec<usize>> = vec![Vec::new(); show.virtuals.len()];
    for (w, area) in areas.iter().enumerate() {
        for v in plan(show, &live, std::slice::from_ref(area)) {
            outputs[v].push(w);
        }
    }
    outputs
        .into_iter()
        .enumerate()
        .filter(|(_, ws)| !ws.is_empty())
        .map(|(v, ws)| (show.virtuals[v].canvas_rect, ws))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Virtual displays `a` (x 0..100), `b` (x 200..300), `c` (x 400..500);
    /// the output covers x 1000..1100. `layers`: (shown virtual, x centre).
    fn show(layers: &[(&str, f32)]) -> Show {
        let layers: Vec<_> = layers
            .iter()
            .enumerate()
            .map(|(i, (v, x))| {
                serde_json::json!({ "id": format!("l{i}"),
                    "source": { "type": "virtual", "id": v },
                    "transform": { "position": [x, 50], "size": [10, 10] } })
            })
            .collect();
        let rect = |x: i32| serde_json::json!({ "x": x, "y": 0, "w": 100, "h": 100 });
        serde_json::from_value(serde_json::json!({
            "canvas": { "width": 2000, "height": 100 },
            "outputs": [],
            "virtuals": [
                { "id": "a", "canvas_rect": rect(0), "width": 64, "height": 64 },
                { "id": "b", "canvas_rect": rect(200), "width": 64, "height": 64 },
                { "id": "c", "canvas_rect": rect(400), "width": 64, "height": 64 }
            ],
            "layers": layers
        }))
        .unwrap()
    }
    const OUT: Rect = Rect {
        x: 1000.0,
        y: 0.0,
        w: 100.0,
        h: 100.0,
    };

    #[test]
    fn only_what_the_outputs_show_is_rendered_and_dependencies_first() {
        // The output shows c; c's area shows b; b's area shows a. And a
        // layer showing c sits in a, unseen by anybody: no loop through it
        // matters, because a is rendered before anything that shows it.
        let s = show(&[("c", 1050.0), ("b", 450.0), ("a", 250.0)]);
        assert_eq!(plan(&s, |_| true, &[OUT]), [0, 1, 2], "a, then b, then c");
        // Nothing on air: nothing to render.
        assert!(plan(&s, |_| false, &[OUT]).is_empty());
        // Only the layer on the output on air: c alone (what it would show
        // is not on air).
        assert_eq!(plan(&s, |i| i == 0, &[OUT]), [2]);
        // An output that shows no virtual display needs none.
        let elsewhere = Rect { x: 5000.0, ..OUT };
        assert!(plan(&s, |_| true, &[elsewhere]).is_empty());
    }

    #[test]
    fn a_loop_is_planned_once_and_ends() {
        // The output shows a; a shows b; b shows a. And c shows itself.
        let s = show(&[
            ("a", 1050.0),
            ("b", 50.0),
            ("a", 250.0),
            ("c", 1060.0),
            ("c", 450.0),
        ]);
        // b first (without a: a is not rendered yet), then a (with b), then c
        // (without itself).
        assert_eq!(plan(&s, |_| true, &[OUT]), [1, 0, 2]);
    }

    #[test]
    fn each_virtual_display_knows_the_outputs_it_reaches() {
        // Output 0 shows c, whose area shows b; output 1 shows a. Nothing
        // shows anything on output 2.
        let s = show(&[("c", 1050.0), ("b", 450.0), ("a", 1250.0)]);
        let second = Rect { x: 1200.0, ..OUT };
        let third = Rect { x: 5000.0, ..OUT };
        let reach = shown_on(&s, |_| true, &[OUT, second, third]);
        let rect = |x: f32| Rect {
            x,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(
            reach,
            vec![
                (rect(0.0), vec![1]),
                (rect(200.0), vec![0]),
                (rect(400.0), vec![0])
            ],
            "a on output 1; b (through c) and c on output 0"
        );
        // The same virtual display shown on two outputs reaches both.
        let s = show(&[("a", 1050.0), ("a", 1250.0)]);
        assert_eq!(
            shown_on(&s, |_| true, &[OUT, second]),
            vec![(rect(0.0), vec![0, 1])]
        );
        // Off air: nothing.
        assert!(shown_on(&s, |_| false, &[OUT, second]).is_empty());
    }
}
