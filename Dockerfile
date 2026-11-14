# Image for the Asset Manager (lc-assets). The other services (Director,
# Runner) are meant to run natively on show hardware.

# ---- build stage ----
FROM rust:1-slim AS build
WORKDIR /app

# The repo is a Cargo workspace: cargo needs every member's manifest to load
# it, so the whole tree is copied. `-p lc-assets` then compiles only the asset
# manager and its dependencies (none of the GPU stack).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p lc-assets

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime

# ffmpeg powers the on-the-fly transcoding endpoint.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/lc-assets /usr/local/bin/lc-assets

# Sensible container defaults; override at runtime as needed.
ENV LCMS_BIND=0.0.0.0:8080 \
    LCMS_MEDIA_DIR=/media \
    LCMS_FFMPEG=ffmpeg

VOLUME ["/media"]
EXPOSE 8080

# Run as an unprivileged user.
RUN useradd --system --uid 10001 lcms
USER lcms

ENTRYPOINT ["lc-assets"]
