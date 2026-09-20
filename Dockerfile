# ---- build stage ----
FROM rust:1-slim AS build
WORKDIR /app

# Pre-cache dependencies: copy manifests, build a stub, then the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch main so cargo rebuilds it against the cached deps.
RUN touch src/main.rs && cargo build --release

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
