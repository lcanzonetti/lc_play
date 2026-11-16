# Working on LC Play

## Git

- **Always push to `main`** as well as to the working branch, unless told
  otherwise for a specific change. Push to `main` only as a fast-forward:
  if `main` has commits the working branch does not, stop and ask instead
  of overwriting them.

## Before pushing

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace` (the GPU tests need a Vulkan driver, e.g.
  `mesa-vulkan-drivers`; the video tests need `ffmpeg` with a HAP encoder)

## Orientation

- Architecture: `docs/ARCHITECTURE.md`; wire format: `docs/PROTOCOL.md`.
- The owner runs it on Windows 11 (laptop with an RTX 3050 Ti): keep
  `cargo check --workspace --target x86_64-pc-windows-msvc` passing.
