# lcmediaserver

A **lightweight, well-optimized, scalable** media server written in Rust.

It scans a directory of media, exposes a small JSON API to browse it, streams
files directly with full HTTP Range support (seeking), and transcodes on the
fly with `ffmpeg` when a client needs a different format or bitrate — all from a
single static binary with an in-memory index and no database.

A minimal built-in web player is served at `/` for testing.

> **This repository is a Cargo workspace with two components:**
> - **`lcmediaserver`** (this crate) — the HTTP streaming/transcoding server documented below.
> - **[`engine/`](engine/README.md)** (`lcmediaserver-engine`) — a **multi-output canvas
>   playback engine** (WATCHOUT-style): arrange displays on a virtual canvas, place
>   content across them, span outputs, with per-output warp + edge-blend. See
>   [`engine/README.md`](engine/README.md).

---

## Why it's lightweight, optimized, and scalable

- **Single async binary** on `axum` + `tokio`. No database, no runtime, no
  background job queue. The library lives in memory.
- **Direct streaming is zero-copy-ish and cache-friendly.** Files are streamed
  from disk in bounded chunks; `Range` requests return `206 Partial Content`,
  which is what makes seeking work and lets a CDN or reverse proxy cache byte
  ranges.
- **Transcoding streams straight from `ffmpeg`** to the HTTP response — nothing
  is buffered to disk. Output is a fragmented container that plays while it
  downloads. A **semaphore caps concurrent transcodes** so a burst of clients
  can never fork-bomb the CPU.
- **Client disconnects are cleaned up promptly.** The `ffmpeg` child process is
  tied to the response body's lifetime and killed on drop — no zombie encoders.
- **Stateless per request.** Every request is served purely from the media
  directory + in-memory index, so you can run *N* identical replicas behind a
  load balancer over shared storage (NFS, object-store gateway, etc.). Item IDs
  are stable across restarts and replicas, so caching stays valid.
- **Release binary is small and fast** — LTO, single codegen unit, stripped,
  `panic = "abort"`.

## Quick start

```bash
# point it at your media and run
LCMS_MEDIA_DIR=/path/to/media cargo run --release

# then open the built-in player
open http://localhost:8080
```

Transcoding requires `ffmpeg` on `PATH` (or set `LCMS_FFMPEG`). Direct streaming
and browsing work without it.

## Configuration

All configuration is via environment variables; every one has a default.

| Variable                 | Default        | Description                                                        |
| ------------------------ | -------------- | ------------------------------------------------------------------ |
| `LCMS_BIND`              | `0.0.0.0:8080` | Address to bind the HTTP server to.                                |
| `LCMS_MEDIA_DIR`         | `./media`      | Root directory scanned for media.                                  |
| `LCMS_FFMPEG`            | `ffmpeg`       | Path to the `ffmpeg` binary used for transcoding.                  |
| `LCMS_SCAN_INTERVAL_SECS`| `300`          | Background rescan interval in seconds. `0` disables periodic rescans. |
| `LCMS_MAX_TRANSCODES`    | *(CPU count)*  | Max concurrent transcode jobs.                                     |
| `LCMS_CORS`              | `false`        | Enable permissive CORS (for a separate front-end).                 |
| `RUST_LOG`               | `info`         | Log level (`tracing` env filter syntax).                           |

## HTTP API

| Method & path            | Description                                              |
| ------------------------ | ------------------------------------------------------- |
| `GET /`                  | Built-in web player (HTML).                             |
| `GET /healthz`           | Liveness probe; always `200`. `{ "status": "ok", "items": N }`, or `"degraded"` plus `"error"` when the last scan couldn't read the media root. |
| `GET /readyz`            | Readiness probe; same body, but `503` while degraded.   |
| `GET /api/library`       | List every indexed item.                                |
| `GET /api/items/{id}`    | Metadata for one item.                                  |
| `GET /stream/{id}`       | Stream the original file, honouring `Range` (seeking).  |
| `GET /transcode/{id}`    | Transcode/remux and stream the result. `HEAD` returns headers without starting `ffmpeg`. |

### Transcode query parameters

`GET /transcode/{id}?format=mp4&height=720&t=90`

| Param      | Default | Description                                                          |
| ---------- | ------- | -------------------------------------------------------------------- |
| `format`   | `mp4`   | Output profile: `mp4` (H.264/AAC) or `webm` (VP9/Opus).              |
| `height`   | *(source)* | Scale to this output height, preserving aspect (e.g. `720`).     |
| `t`        | `0`     | Seek to this many seconds before encoding (fast input-side seek).    |
| `crf`      | `23`/`32` | Quality (lower = better; x264 0–51, VP9 0–63).                     |
| `abitrate` | `128`   | Audio bitrate in kbps (clamped to 32–512).                           |
| `copy`     | `false` | Remux only (`-c copy`) — no re-encode. Fastest when codecs already fit. |

Example — get any file as seekable, 720p MP4 that plays in every browser:

```
/transcode/<id>?format=mp4&height=720
```

To seek in a transcoded stream, request it again with a new `t=` offset (the
transcoded output itself is not byte-range seekable).

Errors and limits:

- A bad query (e.g. unknown `format`) is rejected with `400` immediately.
- When every slot is busy, a request waits up to 5 s for one, then gets
  `503` with `Retry-After: 5`.
- The status line is sent only once `ffmpeg` has produced output and kept
  running for a short moment (≤ 300 ms). If it fails at startup you get `422`
  for a `copy=true` remux the container can't hold, else `500`; details go to
  the server log. If it fails mid-stream the response is cut off rather than
  ending cleanly, so clients can tell it's incomplete.

## Running with Docker

The image bundles `ffmpeg`, so transcoding works out of the box.

```bash
docker build -t lcmediaserver .
docker run --rm -p 8080:8080 -v /path/to/media:/media:ro lcmediaserver
```

## Scaling out

Because the server is stateless per request, horizontal scaling is just running
more replicas that mount the same media directory:

```
                 ┌──────────────┐
   clients ──►   │ load balancer│
                 └──────┬───────┘
          ┌────────────┼────────────┐
      ┌───▼───┐    ┌───▼───┐    ┌───▼───┐
      │ node1 │    │ node2 │    │ node3 │   (identical, stateless)
      └───┬───┘    └───┬───┘    └───┬───┘
          └────────────┼────────────┘
                 ┌──────▼──────┐
                 │ shared media│  (NFS / object storage)
                 └─────────────┘
```

- Put a CDN or caching reverse proxy in front of `/stream/*` — the `Range`
  responses and stable IDs make those cacheable.
- Tune `LCMS_MAX_TRANSCODES` per node to match its CPU budget; transcoding is
  the only CPU-heavy path.
- Use `GET /readyz` as the load-balancer health check: it returns `503` while
  the media root is unreachable. The node keeps its last good index meanwhile,
  so a storage blip never empties the catalogue. Use `GET /healthz` for
  liveness (it stays `200`), so an orchestrator doesn't restart nodes over a
  storage outage a restart can't fix.

## Development

```bash
cargo test           # unit tests (range parsing, id encoding, classification)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

## License

MIT
