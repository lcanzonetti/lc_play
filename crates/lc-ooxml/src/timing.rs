//! Slide transitions (`p:transition`) and animation trees (`p:timing`).
//!
//! Both are read as the file states them; playing them is the job of the
//! transition renderer and the animation engine (later phases).

use lc_deck::{Condition, EffectPreset, Target, Time, TimeNode, TimeNodeKind, Timing, Transition};
use roxmltree::Node;

use crate::xml::{self, child, children, P, P14};

/// Transition durations by speed, ms, used when the file gives no explicit
/// duration (`p14:dur`). [Uncertain: the standard names the speeds without
/// durations; these are PowerPoint's usual values and are to be checked
/// against its renders.]
const SPEED_MS: [(&str, u32); 3] = [("fast", 500), ("med", 750), ("slow", 1000)];

/// Reads `p:transition`. Markup Compatibility is already resolved by the
/// caller's element walk, so a Morph transition (`p159:morph`, only inside
/// an `mc:Choice`) arrives here as written, and an older reader's fallback
/// is ignored.
pub fn transition(node: Node) -> Transition {
    let spd = node.attribute("spd").unwrap_or("fast");
    let by_speed = SPEED_MS
        .iter()
        .find(|(name, _)| *name == spd)
        .map_or(500, |(_, ms)| *ms);
    let duration_ms = xml::ns_attr(node, P14, "dur")
        .and_then(|d| d.trim().parse().ok())
        .unwrap_or(by_speed);
    // The effect is the first child that is not sound or an extension list.
    let effect = xml::elements(node)
        .into_iter()
        .find(|n| !matches!(n.tag_name().name(), "sndAc" | "extLst"));
    Transition {
        kind: effect.map_or("none", |e| e.tag_name().name()).to_string(),
        options: effect
            .map(|e| {
                e.attributes()
                    .map(|a| (a.name().to_string(), a.value().to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        duration_ms,
        advance_on_click: xml::boolean(node, "advClick").unwrap_or(true),
        advance_after_ms: xml::uint(node, "advTm"),
    }
}

/// Reads `p:timing`; `None` when it holds no time node list.
pub fn timing(node: Node) -> Option<Timing> {
    let list = child(node, P, "tnLst")?;
    let root = xml::elements(list)
        .into_iter()
        .find_map(|n| time_node(n, 0))?;
    Some(Timing { root })
}

/// Nesting deeper than this is not PowerPoint's and is cut, so a hostile
/// file cannot exhaust the stack.
const MAX_DEPTH: u32 = 64;

fn time_node(node: Node, depth: u32) -> Option<TimeNode> {
    if depth > MAX_DEPTH || !xml::is(node, P, node.tag_name().name()) {
        return None;
    }
    let name = node.tag_name().name();
    let (kind, ctn, behavior) = match name {
        "par" => (TimeNodeKind::Par, child(node, P, "cTn")?, None),
        "seq" => (TimeNodeKind::Seq, child(node, P, "cTn")?, None),
        "excl" => (TimeNodeKind::Excl, child(node, P, "cTn")?, None),
        "set" | "anim" | "animEffect" | "animMotion" | "animScale" | "animRot" | "animClr"
        | "cmd" => {
            let bhvr = child(node, P, "cBhvr")?;
            (
                TimeNodeKind::Behavior(name.into()),
                child(bhvr, P, "cTn")?,
                Some(bhvr),
            )
        }
        "audio" | "video" => {
            let media = child(node, P, "cMediaNode")?;
            (
                TimeNodeKind::Behavior(name.into()),
                child(media, P, "cTn")?,
                Some(media),
            )
        }
        _ => return None,
    };
    let preset = ctn.attribute("presetClass").map(|class| EffectPreset {
        class: class.to_string(),
        id: xml::uint(ctn, "presetID").unwrap_or(0),
        subtype: xml::uint(ctn, "presetSubtype").unwrap_or(0),
    });
    let children = child(ctn, P, "childTnLst")
        .map(|l| {
            xml::elements(l)
                .into_iter()
                .filter_map(|c| time_node(c, depth + 1))
                .collect()
        })
        .unwrap_or_default();
    Some(TimeNode {
        id: xml::uint(ctn, "id").unwrap_or(0),
        kind,
        node_type: ctn.attribute("nodeType").map(str::to_string),
        preset,
        begin: conditions(child(ctn, P, "stCondLst")),
        end: conditions(child(ctn, P, "endCondLst")),
        duration: ctn.attribute("dur").and_then(time),
        repeat: ctn.attribute("repeatCount").and_then(time),
        target: behavior.and_then(|b| child(b, P, "tgtEl")).and_then(target),
        children,
    })
}

fn conditions(list: Option<Node>) -> Vec<Condition> {
    let Some(list) = list else {
        return Vec::new();
    };
    children(list, P, "cond")
        .into_iter()
        .map(|c| {
            let tgt = child(c, P, "tgtEl");
            Condition {
                delay: c.attribute("delay").and_then(time),
                event: c.attribute("evt").map(str::to_string),
                shape: tgt
                    .and_then(|t| child(t, P, "spTgt"))
                    .and_then(|s| xml::uint(s, "spid")),
                time_node: child(c, P, "tn").and_then(|t| xml::uint(t, "val")),
            }
        })
        .collect()
}

fn target(tgt_el: Node) -> Option<Target> {
    let sp = child(tgt_el, P, "spTgt")?;
    let paragraphs = child(sp, P, "txEl")
        .and_then(|t| child(t, P, "pRg"))
        .and_then(|r| Some((xml::uint(r, "st")?, xml::uint(r, "end")?)));
    Some(Target {
        shape: xml::uint(sp, "spid")?,
        paragraphs,
    })
}

/// `"indefinite"` or a whole number of ms.
fn time(s: &str) -> Option<Time> {
    match s.trim() {
        "indefinite" => Some(Time::Indefinite),
        n => n.parse().ok().map(Time::Ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
        xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main""#;

    fn parse_transition(inner: &str) -> Transition {
        let text = format!("<p:transition {NS} {inner}");
        let doc = xml::parse(&text, "t").unwrap();
        transition(doc.root_element())
    }

    #[test]
    fn transition_speed_and_explicit_duration() {
        let t = parse_transition(r#"spd="slow" advTm="3000"><p:push dir="u"/></p:transition>"#);
        assert_eq!(t.kind, "push");
        assert_eq!(t.options, vec![("dir".to_string(), "u".to_string())]);
        assert_eq!(t.duration_ms, 1000);
        assert_eq!(t.advance_after_ms, Some(3000));
        assert!(t.advance_on_click);

        let t =
            parse_transition(r#"p14:dur="1600" advClick="0"><p14:vortex dir="r"/></p:transition>"#);
        assert_eq!(
            (t.kind.as_str(), t.duration_ms, t.advance_on_click),
            ("vortex", 1600, false)
        );

        let t = parse_transition("/>");
        assert_eq!(t.kind, "none");
    }

    #[test]
    fn times() {
        assert_eq!(time("indefinite"), Some(Time::Indefinite));
        assert_eq!(time("500"), Some(Time::Ms(500)));
        assert_eq!(time("-1"), None);
    }
}
