//! XML helpers: namespaces, Markup Compatibility, attribute parsing.
//!
//! Office Open XML exists in two spellings: *Transitional* (what PowerPoint
//! writes, namespaces under `schemas.openxmlformats.org`) and *Strict*
//! (namespaces under `purl.oclc.org/ooxml`). Elements are matched by local
//! name within either namespace of a family, so both read the same.
//!
//! PowerPoint wraps newer features in `mc:AlternateContent`: a `Choice`
//! that needs namespaces an older reader may not know (for example Morph,
//! `p159`), and a `Fallback` an older reader shows instead. [`elements`]
//! resolves these in place: a `Choice` is taken when every namespace it
//! requires is one this reader understands ([`UNDERSTOOD`]), otherwise the
//! `Fallback`. Everything above sees the chosen branch as if it were
//! written there directly.

use roxmltree::{Document, Node, ParsingOptions};

use crate::Error;

pub const P: &[&str] = &[
    "http://schemas.openxmlformats.org/presentationml/2006/main",
    "http://purl.oclc.org/ooxml/presentationml/main",
];
pub const A: &[&str] = &[
    "http://schemas.openxmlformats.org/drawingml/2006/main",
    "http://purl.oclc.org/ooxml/drawingml/main",
];
pub const R: &[&str] = &[
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    "http://purl.oclc.org/ooxml/officeDocument/relationships",
];
pub const MC: &[&str] = &["http://schemas.openxmlformats.org/markup-compatibility/2006"];
/// PowerPoint 2010 extensions (newer transitions, media, durations).
pub const P14: &[&str] = &["http://schemas.microsoft.com/office/powerpoint/2010/main"];
/// SmartArt drawing extension (`dsp:dataModelExt`).
pub const DSP: &[&str] = &["http://schemas.microsoft.com/office/drawing/2008/diagram"];

/// Namespaces whose `mc:Choice` this reader takes. Only PresentationML
/// extensions whose content is read here; a choice needing anything else
/// (for example `a14` for equations) falls back to the picture PowerPoint
/// stores for older readers, which is what LC Play can show.
pub const UNDERSTOOD: &[&str] = &[
    "http://schemas.openxmlformats.org/presentationml/2006/main",
    "http://schemas.openxmlformats.org/drawingml/2006/main",
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    "http://schemas.microsoft.com/office/powerpoint/2010/main",
    "http://schemas.microsoft.com/office/powerpoint/2012/main",
    "http://schemas.microsoft.com/office/powerpoint/2015/09/main",
];

/// Parses a part. Document type declarations are refused (roxmltree's
/// default), which rules out entity-expansion attacks; OOXML never uses
/// them.
pub fn parse<'i>(text: &'i str, part: &str) -> Result<Document<'i>, Error> {
    Document::parse_with_options(text, ParsingOptions::default()).map_err(|e| Error::Xml {
        part: part.to_string(),
        message: e.to_string(),
    })
}

/// Whether `node` is the element `name` in one of the namespaces `ns`.
pub fn is(node: Node, ns: &[&str], name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node.tag_name().namespace().is_some_and(|u| ns.contains(&u))
}

/// Element children, with `mc:AlternateContent` replaced by the branch
/// chosen (recursively).
pub fn elements<'a, 'i>(node: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    let mut out = Vec::new();
    push_elements(node, &mut out, 0);
    out
}

fn push_elements<'a, 'i>(node: Node<'a, 'i>, out: &mut Vec<Node<'a, 'i>>, depth: u32) {
    for child in node.children().filter(|n| n.is_element()) {
        if is(child, MC, "AlternateContent") && depth < 8 {
            if let Some(branch) = choose(child) {
                push_elements(branch, out, depth + 1);
            }
        } else {
            out.push(child);
        }
    }
}

fn choose<'a, 'i>(alternate: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    let branches: Vec<_> = alternate.children().filter(|n| n.is_element()).collect();
    branches
        .iter()
        .find(|b| is(**b, MC, "Choice") && requirements_met(**b))
        .or_else(|| branches.iter().find(|b| is(**b, MC, "Fallback")))
        .copied()
}

fn requirements_met(choice: Node) -> bool {
    let Some(requires) = choice.attribute("Requires") else {
        return false;
    };
    requires.split_whitespace().all(|prefix| {
        choice
            .lookup_namespace_uri(Some(prefix))
            .is_some_and(|uri| UNDERSTOOD.contains(&uri))
    })
}

/// The first child element `name` in `ns` (Markup Compatibility resolved).
pub fn child<'a, 'i>(node: Node<'a, 'i>, ns: &[&str], name: &str) -> Option<Node<'a, 'i>> {
    elements(node).into_iter().find(|n| is(*n, ns, name))
}

/// All child elements `name` in `ns`.
pub fn children<'a, 'i>(node: Node<'a, 'i>, ns: &[&str], name: &str) -> Vec<Node<'a, 'i>> {
    elements(node)
        .into_iter()
        .filter(|n| is(*n, ns, name))
        .collect()
}

/// Follows a path of child elements, all in `ns`.
pub fn path<'a, 'i>(node: Node<'a, 'i>, ns: &[&str], names: &[&str]) -> Option<Node<'a, 'i>> {
    names.iter().try_fold(node, |n, name| child(n, ns, name))
}

/// A relationship-id attribute (`r:id`, `r:embed`, `r:link`…).
pub fn rel_attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    R.iter().find_map(|ns| node.attribute((*ns, name)))
}

/// An attribute in one of the namespaces `ns` (for prefixed attributes
/// such as `p14:dur`).
pub fn ns_attr<'a>(node: Node<'a, '_>, ns: &[&str], name: &str) -> Option<&'a str> {
    ns.iter().find_map(|u| node.attribute((*u, name)))
}

pub fn int(node: Node, name: &str) -> Option<i64> {
    node.attribute(name)?.trim().parse().ok()
}

pub fn uint(node: Node, name: &str) -> Option<u32> {
    node.attribute(name)?.trim().parse().ok()
}

/// An XML Schema boolean (`true`, `false`, `1`, `0`; Office also writes
/// `on` and `off`).
pub fn boolean(node: Node, name: &str) -> Option<bool> {
    match node.attribute(name)?.trim() {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    }
}

/// An `ST_Percentage`-style value: `50000` (1/1000 percent) or, in Strict,
/// `50%`. Returned as a fraction (0.5).
pub fn fraction(node: Node, name: &str) -> Option<f64> {
    let v = node.attribute(name)?.trim();
    match v.strip_suffix('%') {
        Some(p) => p.parse::<f64>().ok().map(|p| p / 100.0),
        None => v.parse::<f64>().ok().map(|n| n / 100_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"<p:root xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
        xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"
        xmlns:p159="http://schemas.microsoft.com/office/powerpoint/2015/09/main"
        xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main">
      <mc:AlternateContent>
        <mc:Choice Requires="p159"><p:new/></mc:Choice>
        <mc:Fallback><p:old/></mc:Fallback>
      </mc:AlternateContent>
      <mc:AlternateContent>
        <mc:Choice Requires="a14"><p:equation/></mc:Choice>
        <mc:Fallback><p:picture/></mc:Fallback>
      </mc:AlternateContent>
      <p:plain/>
    </p:root>"#;

    #[test]
    fn markup_compatibility_takes_understood_choices_only() {
        let doc = parse(DOC, "t").unwrap();
        let names: Vec<_> = elements(doc.root_element())
            .iter()
            .map(|n| n.tag_name().name())
            .collect();
        assert_eq!(names, ["new", "picture", "plain"]);
    }

    #[test]
    fn refuses_doctype() {
        let evil = r#"<!DOCTYPE x [<!ENTITY a "aaaa">]><x>&a;</x>"#;
        assert!(parse(evil, "t").is_err());
    }

    #[test]
    fn percentages() {
        let doc = parse(r#"<x a="50000" b="25%" c="nope"/>"#, "t").unwrap();
        let n = doc.root_element();
        assert_eq!(fraction(n, "a"), Some(0.5));
        assert_eq!(fraction(n, "b"), Some(0.25));
        assert_eq!(fraction(n, "c"), None);
    }
}
