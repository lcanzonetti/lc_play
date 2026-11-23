//! Reads .pptx files and prints what LC Play understood of them.
//!
//! ```text
//! cargo run -p lc-ooxml --example pptx-dump -- deck.pptx          # summary
//! cargo run -p lc-ooxml --example pptx-dump -- --json deck.pptx   # whole model
//! cargo run -p lc-ooxml --example pptx-dump -- corpus/*.pptx      # one line each
//! ```
//!
//! The one-line-per-file form is meant for the phase 0 corpus: it shows at
//! a glance which decks fail to read and which produce warnings.

use std::fs::File;
use std::io::BufReader;

fn main() {
    let mut json = false;
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" => json = true,
            _ => files.push(arg),
        }
    }
    if files.is_empty() {
        eprintln!("usage: pptx-dump [--json] FILE.pptx…");
        std::process::exit(2);
    }
    let mut failed = 0;
    for path in &files {
        let deck = File::open(path)
            .map_err(|e| e.to_string())
            .and_then(|f| lc_ooxml::read(BufReader::new(f)).map_err(|e| e.to_string()));
        let deck = match deck {
            Ok(d) => d,
            Err(e) => {
                failed += 1;
                println!("{path}: ERROR {e}");
                continue;
            }
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&deck).expect("serialises")
            );
            continue;
        }
        let shapes: usize = deck.slides.iter().map(|s| s.shapes.len()).sum();
        let morphs = deck
            .slides
            .iter()
            .filter(|s| s.transition.as_ref().is_some_and(|t| t.kind == "morph"))
            .count();
        println!(
            "{path}: {} slides ({} shown), {} steps, {shapes} shapes, {morphs} morph, {} warnings",
            deck.slides.len(),
            deck.shown_slides().count(),
            deck.step_count(),
            deck.warnings.len()
        );
        if files.len() == 1 {
            for s in &deck.slides {
                let clicks = s.timing.as_ref().map_or(0, |t| t.click_count());
                let transition = s.transition.as_ref().map_or("-", |t| t.kind.as_str());
                let notes = s.notes.lines().next().unwrap_or("");
                println!(
                    "  {:>3}{} {:<28} {:>3} shapes {:>2} clicks  {:<10} {notes}",
                    s.number,
                    if s.hidden { "h" } else { " " },
                    s.layout,
                    s.shapes.len(),
                    clicks,
                    transition
                );
            }
            for w in &deck.warnings {
                println!("  warning: {w}");
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}
