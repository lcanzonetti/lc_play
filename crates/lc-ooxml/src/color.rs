//! DrawingML colours: resolving theme references and colour transforms to
//! plain sRGB.
//!
//! A colour in a slide is rarely an RGB value. Usually it is a *scheme*
//! colour (`accent1`, `tx1`) that goes through the master's colour map
//! (`tx1` → `dk1`) to the theme's colour scheme, followed by transforms
//! (`lumMod 75000` = "accent 1, 25 % darker"). Resolving it here means the
//! renderer never needs the theme.
//!
//! Fidelity notes: `lumMod`/`lumOff`/`satMod` work in HSL, which is how
//! PowerPoint's own colour picker describes them and matches its output on
//! the usual theme tints. `tint`/`shade` are applied in linear light.
//! Both are to be checked against PowerPoint renders in the shapes phase
//! [not yet verified].

use std::collections::HashMap;

use lc_deck::Rgba;
use roxmltree::Node;

use crate::xml::{self, A};

/// A theme's colour scheme: `dk1`, `lt1`, `dk2`, `lt2`, `accent1`…`6`,
/// `hlink`, `folHlink` → colour.
#[derive(Debug, Clone, Default)]
pub struct Scheme(pub HashMap<String, Rgba>);

impl Scheme {
    /// Reads `a:clrScheme`.
    pub fn read(clr_scheme: Node) -> Scheme {
        let mut map = HashMap::new();
        for slot in xml::elements(clr_scheme) {
            let empty = Palette::default();
            if let Some(c) = color_child(slot).and_then(|c| resolve(c, &empty)) {
                map.insert(slot.tag_name().name().to_string(), c);
            }
        }
        Scheme(map)
    }
}

/// A colour map (`p:clrMap`): `bg1`, `tx1`, `bg2`, `tx2`, `accent1`…,
/// `hlink`, `folHlink` → a scheme slot name.
#[derive(Debug, Clone)]
pub struct Map(pub HashMap<String, String>);

impl Default for Map {
    /// The usual mapping (dark text on a light background), used when a
    /// master states none.
    fn default() -> Self {
        let pairs = [
            ("bg1", "lt1"),
            ("tx1", "dk1"),
            ("bg2", "lt2"),
            ("tx2", "dk2"),
        ];
        let mut map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for slot in [
            "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink",
        ] {
            map.insert(slot.into(), slot.into());
        }
        Map(map)
    }
}

impl Map {
    /// Reads a `p:clrMap` (or `a:overrideClrMapping`): every attribute is
    /// one mapping.
    pub fn read(node: Node) -> Map {
        let mut map = Map::default();
        for a in node.attributes() {
            map.0.insert(a.name().to_string(), a.value().to_string());
        }
        map
    }
}

/// Everything needed to resolve a colour in one part.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub scheme: Scheme,
    pub map: Map,
    /// The "placeholder colour" (`phClr`) a theme style is being applied
    /// with, if any.
    pub placeholder: Option<Rgba>,
}

impl Palette {
    fn scheme_color(&self, name: &str) -> Option<Rgba> {
        if name == "phClr" {
            return self.placeholder;
        }
        let slot = self.map.0.get(name).map_or(name, String::as_str);
        self.scheme.0.get(slot).copied()
    }

    pub fn with_placeholder(&self, color: Option<Rgba>) -> Palette {
        Palette {
            placeholder: color,
            ..self.clone()
        }
    }
}

const COLOR_ELEMENTS: &[&str] = &[
    "srgbClr",
    "schemeClr",
    "sysClr",
    "prstClr",
    "scrgbClr",
    "hslClr",
];

/// The colour element directly inside `node` (a `solidFill`, a gradient
/// stop, a style reference), if any.
pub fn color_child<'a, 'i>(node: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    xml::elements(node)
        .into_iter()
        .find(|n| COLOR_ELEMENTS.iter().any(|name| xml::is(*n, A, name)))
}

/// Resolves one colour element, transforms included. `None` when it
/// refers to something unknown (a scheme slot the theme lacks).
pub fn resolve(color: Node, palette: &Palette) -> Option<Rgba> {
    let base = match color.tag_name().name() {
        "srgbClr" => hex(color.attribute("val")?)?,
        "schemeClr" => palette.scheme_color(color.attribute("val")?)?,
        "sysClr" => match color.attribute("lastClr").and_then(hex) {
            Some(c) => c,
            None => match color.attribute("val")? {
                "window" | "highlightText" | "btnHighlight" => Rgba::WHITE,
                _ => Rgba::BLACK,
            },
        },
        "prstClr" => preset(color.attribute("val")?)?,
        "scrgbClr" => {
            // Linear-light percentages.
            let ch = |n| xml::fraction(color, n).map(|v| to_srgb8(v.clamp(0.0, 1.0)));
            Rgba::rgb(ch("r")?, ch("g")?, ch("b")?)
        }
        "hslClr" => {
            let h = xml::int(color, "hue")? as f64 / 60_000.0;
            let s = xml::fraction(color, "sat")?;
            let l = xml::fraction(color, "lum")?;
            from_hsl(h, s, l, 255)
        }
        _ => return None,
    };
    Some(transform(base, color))
}

/// Applies the transform children of a colour element in document order.
fn transform(mut c: Rgba, color: Node) -> Rgba {
    for t in xml::elements(color) {
        let Some(v) = xml::fraction(t, "val") else {
            continue;
        };
        match t.tag_name().name() {
            "alpha" => c.a = unit8(v),
            "alphaMod" => c.a = unit8(c.a as f64 / 255.0 * v),
            "alphaOff" => c.a = unit8(c.a as f64 / 255.0 + v),
            "lumMod" | "lumOff" | "satMod" | "satOff" => {
                let (h, mut s, mut l) = to_hsl(c);
                match t.tag_name().name() {
                    "lumMod" => l *= v,
                    "lumOff" => l += v,
                    "satMod" => s *= v,
                    _ => s += v,
                }
                c = from_hsl(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0), c.a);
            }
            // tint 40 % = 40 % of the colour, the rest white; shade 40 % =
            // 40 % of the colour, the rest black. In linear light.
            "tint" => c = map_linear(c, |x| 1.0 - v * (1.0 - x)),
            "shade" => c = map_linear(c, |x| x * v),
            _ => {}
        }
    }
    c
}

fn hex(s: &str) -> Option<Rgba> {
    let s = s.trim();
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Rgba::rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// The preset colours in common use. The standard defines about 140 (the
/// CSS named colours); the rest are added when the corpus shows them.
fn preset(name: &str) -> Option<Rgba> {
    let rgb = match name {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "dkGray" | "darkGray" => (169, 169, 169),
        "ltGray" | "lightGray" => (211, 211, 211),
        "silver" => (192, 192, 192),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "navy" => (0, 0, 128),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "teal" => (0, 128, 128),
        _ => return None,
    };
    Some(Rgba::rgb(rgb.0, rgb.1, rgb.2))
}

fn unit8(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn to_linear(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn to_srgb8(l: f64) -> u8 {
    let c = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    unit8(c)
}

fn map_linear(c: Rgba, f: impl Fn(f64) -> f64) -> Rgba {
    let ch = |x: u8| to_srgb8(f(to_linear(x)).clamp(0.0, 1.0));
    Rgba {
        r: ch(c.r),
        g: ch(c.g),
        b: ch(c.b),
        a: c.a,
    }
}

/// Hue in degrees, saturation and lightness 0..=1.
fn to_hsl(c: Rgba) -> (f64, f64, f64) {
    let (r, g, b) = (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f64, s: f64, l: f64, a: u8) -> Rgba {
    if s == 0.0 {
        let v = unit8(l);
        return Rgba {
            r: v,
            g: v,
            b: v,
            a,
        };
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let h = (h / 360.0).rem_euclid(1.0);
    let ch = |t: f64| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        unit8(v)
    };
    Rgba {
        r: ch(h + 1.0 / 3.0),
        g: ch(h),
        b: ch(h - 1.0 / 3.0),
        a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        let mut scheme = HashMap::new();
        scheme.insert("dk1".to_string(), Rgba::rgb(0, 0, 0));
        scheme.insert("lt1".to_string(), Rgba::rgb(255, 255, 255));
        scheme.insert("accent1".to_string(), Rgba::rgb(0x44, 0x72, 0xC4));
        Palette {
            scheme: Scheme(scheme),
            map: Map::default(),
            placeholder: Some(Rgba::rgb(10, 20, 30)),
        }
    }

    fn color(xml_text: &str) -> Option<Rgba> {
        let text = format!(
            r#"<w xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">{xml_text}</w>"#
        );
        let doc = xml::parse(&text, "t").unwrap();
        resolve(color_child(doc.root_element()).unwrap(), &palette())
    }

    #[test]
    fn scheme_colours_go_through_the_map() {
        assert_eq!(color(r#"<a:schemeClr val="tx1"/>"#), Some(Rgba::BLACK));
        assert_eq!(color(r#"<a:schemeClr val="bg1"/>"#), Some(Rgba::WHITE));
        assert_eq!(
            color(r#"<a:schemeClr val="accent1"/>"#),
            Some(Rgba::rgb(0x44, 0x72, 0xC4))
        );
        assert_eq!(
            color(r#"<a:schemeClr val="phClr"/>"#),
            Some(Rgba::rgb(10, 20, 30))
        );
        assert_eq!(color(r#"<a:schemeClr val="accent6"/>"#), None);
    }

    #[test]
    fn transforms() {
        // White, 50 % luminance → mid grey.
        assert_eq!(
            color(r#"<a:srgbClr val="FFFFFF"><a:lumMod val="50000"/></a:srgbClr>"#),
            Some(Rgba::rgb(128, 128, 128))
        );
        // Black lightened by 25 % → 25 % grey.
        assert_eq!(
            color(
                r#"<a:srgbClr val="000000"><a:lumMod val="75000"/><a:lumOff val="25000"/></a:srgbClr>"#
            ),
            Some(Rgba::rgb(64, 64, 64))
        );
        let c = color(r#"<a:srgbClr val="FF0000"><a:alpha val="40000"/></a:srgbClr>"#).unwrap();
        assert_eq!((c.r, c.a), (255, 102));
        // A shade of 0 is black, a tint of 0 white.
        assert_eq!(
            color(r#"<a:srgbClr val="4472C4"><a:shade val="0"/></a:srgbClr>"#),
            Some(Rgba::BLACK)
        );
        assert_eq!(
            color(r#"<a:srgbClr val="4472C4"><a:tint val="0"/></a:srgbClr>"#),
            Some(Rgba::WHITE)
        );
    }

    #[test]
    fn system_and_preset() {
        assert_eq!(
            color(r#"<a:sysClr val="windowText" lastClr="112233"/>"#),
            Some(Rgba::rgb(0x11, 0x22, 0x33))
        );
        assert_eq!(color(r#"<a:sysClr val="window"/>"#), Some(Rgba::WHITE));
        assert_eq!(
            color(r#"<a:prstClr val="red"/>"#),
            Some(Rgba::rgb(255, 0, 0))
        );
    }

    #[test]
    fn hsl_round_trip() {
        for c in [
            Rgba::rgb(0x44, 0x72, 0xC4),
            Rgba::rgb(200, 30, 90),
            Rgba::rgb(7, 7, 7),
        ] {
            let (h, s, l) = to_hsl(c);
            assert_eq!(from_hsl(h, s, l, 255), c);
        }
    }
}
