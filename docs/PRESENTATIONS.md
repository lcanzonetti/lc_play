# Native presentations

Status: **phase 1 of 11** (reading .pptx files). Nothing in the Director,
the Runners or the Producer uses it yet. Work happens on the branch
`feature/native-pptx-playback`, not on `main`.

The goal is for LC Play to play PowerPoint decks itself: every Runner
draws the slides, steps through builds on cue and stays frame-synced, with
no PowerPoint in the playback path. The full plan (phases, gates, fidelity
testing, risks) is kept outside the repository; this file records what is
in the code.

## Layers

```
.pptx ──lc-ooxml──▶ lc_deck::Deck ──(later) lc-text, lc-deck-render──▶ Runner layers
.keyx ──(later) lc-keyx──┘
```

* **`lc-deck`** is the model every reader produces: slides in show order,
  shapes back to front (master and layout decoration first, marked with
  their layer), resolved colours and theme fonts, text runs with their
  directly stated formatting, transitions, the animation tree as written,
  speaker notes. `Timing::main_sequence` derives the click steps;
  `Deck::step_count` is what an operator presses through.
* **`lc-ooxml`** reads the package (ZIP + relationships), follows
  presentation → slide → layout → master → theme, and fills the model.
  `cargo run -p lc-ooxml --example pptx-dump -- deck.pptx` shows what it
  understood of a file.

## What phase 1 reads

| Area | Read | Left for later phases |
| --- | --- | --- |
| Package | ZIP, relationships (Transitional and Strict), `mc:AlternateContent` (newer features taken when understood, otherwise the fallback) | — |
| Inheritance | Placeholder positions slide → layout → master; backgrounds; master/layout decoration with *Hide background graphics*; colour-map overrides | Text style cascade (phase 3) |
| Colours | sRGB, scheme (through the colour map), system, preset, scRGB, HSL; alpha, lumMod/lumOff, satMod, tint, shade | Remaining transforms; fidelity of tint/shade to be checked against PowerPoint (phase 2) |
| Shapes | Preset geometry with adjustments, fills (solid, gradient, picture), outlines, theme style references (`p:style`) | Custom geometry paths, pattern fills, effects (phases 2 and 5) |
| Pictures and media | Image part, crop, embedded or linked video/audio | — |
| Frames | Tables (size), charts (part), SmartArt (its stored drawing part) | Drawing them (phase 5) |
| Text | Paragraphs, runs, breaks, fields, size, bold, italic, Latin font (theme fonts resolved), colour, insets, anchor, wrap, autofit | Layout (phase 3) |
| Transitions | Kind and options (Morph and the 2010+ set included), duration, advance on click / after time | Rendering (phases 6 and 8) |
| Animations | The whole time-node tree: containers, behaviours, presets, start/end conditions, durations, repeats, shape and paragraph targets; click steps derived | Evaluation (phase 7); shape-click triggers are reported, not played |
| Notes | Plain text | — |

## Safety

Files come from clients, so the reader treats them as hostile: entries are
inflated with a hard cap (64 MiB per XML part, 512 MiB in total) counted on
the bytes produced rather than the header; document type declarations are
refused (no entity expansion); the animation tree is cut at 64 levels; a
part that does not parse skips that slide with a warning instead of failing
the deck. A test corrupts every part of a deck at hundreds of positions and
checks that nothing panics.
