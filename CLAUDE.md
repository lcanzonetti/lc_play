# Notes for AI coding sessions

This repository is a **show-control media server** in the WATCHOUT / Pixera /
disguise category: multi-output playback for displays, LED walls and
projectors at live events and installations. It is **not** an HTTP
streaming / Plex-style media server.

- Read the "Project goal" section of `README.md` before planning or reviewing
  work, and judge findings by it. Browser compatibility, HTTP caching, CDN or
  load-balancer concerns are out of scope here.
- The old HTTP streaming server that used to live at this repository's root
  was moved to its own repository (`lcanzonetti/lcserver`) on 2026-11-13. Older commits
  that mention `/stream`, `/transcode`, `LCMS_*`, Docker or a web player are
  about that server.
- Priorities: show reliability (nothing a bad file or an operator does may
  take outputs down mid-show), timing accuracy (one master clock), then
  features in the README roadmap order. Video playback (Phase 2) is next.
- Checks: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --locked`. The offscreen GPU tests need a Vulkan driver
  (`apt install mesa-vulkan-drivers` on headless Linux); with `CI` or
  `LCME_REQUIRE_GPU` set they fail instead of skipping.
