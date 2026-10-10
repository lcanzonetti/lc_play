# ---- build stage ----
FROM rust:1-slim AS build
WORKDIR /app

# Pre-cache dependencies: copy manifests, build a stub, then the real sources.
# The repo is a Cargo workspace, so the engine's manifest (plus a stub source)
# must exist for cargo to load it; `-p lcmediaserver` builds only the server.
COPY Cargo.toml Cargo.lock ./
COPY engine/Cargo.toml engine/Cargo.toml
RUN mkdir -p src engine/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "fn main() {}" > engine/src/main.rs \
    && cargo build --release --locked -p lcmediaserver \
    && rm -rf src

COPY src ./src
# Touch main so cargo rebuilds it against the cached deps.
RUN touch src/main.rs && cargo build --release --locked -p lcmediaserver

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime

# ffmpeg powers the on-the-fly transcoding endpoint.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/lcmediaserver /usr/local/bin/lcmediaserver

# Sensible container defaults; override at runtime as needed.
ENV LCMS_BIND=0.0.0.0:8080 \
    LCMS_MEDIA_DIR=/media \
    LCMS_FFMPEG=ffmpeg

VOLUME ["/media"]
EXPOSE 8080

# Run as an unprivileged user.
RUN useradd --system --uid 10001 lcms
USER lcms

ENTRYPOINT ["lcmediaserver"]
