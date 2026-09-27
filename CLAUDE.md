# podimo-rs

An unofficial podcast and audiobook proxy for Podimo, written in Rust. It serves
two things:

1. **RSS feeds** (`/feed/<podcast-id>.xml`, `/audiobook/<audiobook-id>.xml`):
   standard feeds for podcatchers and Audiobookshelf.
2. **Audiobook sync to disk** (`/library`): saves subscribed audiobooks to
   `LIBRARY_DIR/<Author>/<Title>/` so Audiobookshelf can serve them.

## Development commands

```bash
cargo build                      # build
cargo test                       # all tests (needs ffmpeg on PATH)
cargo clippy --all-targets       # linter (-D warnings)
cargo fmt --all -- --check       # style check
cargo run                        # run locally (reads .env)
```

Running a single test:

```bash
cargo test -p podimo-rs test_name
cargo test -p podimo-rs --test integration_feed
```

The transcode tests in `crates/podimo-rs/src/podimo/hls.rs` invoke the real
`ffmpeg` binary (via lavfi sine tone generation and ffprobe). CI runs with
Debian's `ffmpeg` installed.

## Architecture

A cargo workspace with one binary crate:

```text
crates/podimo-rs/src/
  main.rs             entry point, router assembly, tracing init
  config.rs           env-var parsing with serde / envy
  state.rs            AppState: caches, clients, config
  handlers/           axum request handlers
    feed.rs           /feed, /audiobook, /
    stream.rs         /stream/<token>/<id>.mp3|.m4a, /stream/<id>.aac
    library.rs        /library (web UI and JSON API)
    setup.rs          /setup form
  podimo/             upstream Podimo GraphQL and auth
    client.rs         GraphQL queries and token refresh
    auth.rs           token parsing and cookies
    hls.rs            HLS playlist resolution, MPEG-TS demux, ffmpeg transcode
    head.rs           HEAD probe for the size of non-HLS enclosures
  library/            audiobook downloads and on-disk layout
  cache.rs            TtlCache: moka in memory, JSON files on disk
  episode_files.rs    finished M4A episodes in a temp dir, one preparation per episode
  util.rs             helpers: auth parsing, request_base_url, amp_arg, …
crates/podimo-rs/templates/   HTML, embedded with include_str!
crates/podimo-rs/tests/       integration tests; Podimo is mocked with wiremock
```

## Podimo auth flow

- **Web login**: `POST https://auth.podimo.com/tokens` returns access and
  refresh tokens.
- **App login**: `POST https://auth.podimo.com/apple/tokens` or `/google/tokens`.
- Stored as `PODIMO_ACCESS_TOKEN` / `PODIMO_REFRESH_TOKEN` in `.env` or passed
  via HTTP Basic auth per request.
- The GraphQL endpoint is `https://api.podimo.com/graphql`. Queries are plain
  strings in `crates/podimo-rs/src/podimo/client.rs`.

## Feed generation

1. `handlers::feed::get_podcast_feed` parses the podcast ID from the route.
2. It fetches episodes via `client::episodes` with pagination.
   The memory cache key includes `?limit=N` when the caller asked for fewer
   episodes than the default 100, so a small request doesn't evict the entry
   in that hundred, or `all` once the show ran out, which serves any limit.
   The audiobook caches are per account too, since payloads hold signed URLs.
3. `rss::podcasts_to_rss` renders the feed. HLS episodes link to `/stream`
   (below); other enclosures are HEAD-probed for their size.

## HLS episodes

Podimo's `streamMedia.url` is a signed master playlist: 192k and 320k variants
of ~10 s MPEG-TS segments with AAC-LC. There is no progressive file upstream.

- The enclosure is `<base>/stream/<token>/<id>.<ext>`. `token` is the playlist
  URL in unpadded base64url, so the link is a plain file path; Audiobookshelf
  picks the file type from the extension. `<base>` comes from the feed request
  (`util::request_base_url`: `X-Forwarded-Proto`/`X-Forwarded-Host`, else
  `Host` over `http`, or over `PODIMO_PROTOCOL` when `Host` is
  `PODIMO_HOSTNAME`), falling back to `PODIMO_PROTOCOL://PODIMO_HOSTNAME`.
  In-cluster clients thus get `http://podimo/…` and proxied ones the public
  host. `STREAM_LINKS_FROM_REQUEST=false` always uses the configured address,
  for proxies that rewrite `Host`.
- `/stream` has no auth, since the signed URL is the credential, but only
  accepts https `.m3u8` URLs on `*.podimo.com`. The variant and segment URLs
  in the playlists, and any redirect target, must be https on `*.podimo.com`
  too. It takes the highest-bandwidth variant, fetches segments with retries,
  demuxes TS to ADTS and pipes that into ffmpeg's stdin.
- At most 16 `/stream` bodies or file preparations run at once, of any format,
  and at most 4 of them are MP3 encodes. A request that doesn't get a slot
  within 10 s gets 503 with `Retry-After: 30`. HEAD takes no slot.
- `STREAM_FORMAT` (`hls::StreamFormat`) sets what feeds link to; both
  extensions are always served.
  - `m4a` (default): stream copy into a regular MP4 with the `moov` up front
    (`hls::remux_m4a`). Almost no CPU, original quality. The whole episode is
    fetched (8 segments at a time, ~2 s for 45 min) before the response
    starts; `episode_files.rs` keeps the file in a temp dir for an hour after
    its last request (2 GB at most, LRU) and serves it with `ServeFile`, so
    there's a `Content-Length` and `Range` works. Concurrent requests for an
    episode share one preparation, which also survives the client leaving.
    Keyed by `hls::source_key` (playlist host and path), not the episode ID
    in the URL.
  - `mp3`: LAME, 128 kbps CBR at `-q 7`, streamed from ffmpeg's stdout as
    it's produced (4 segments ahead). About one core per download, at most 4
    at once. CBR keeps the duration right without a Xing header. A failure
    mid-stream ends the chunked body with an error rather than a short file
    that looks complete; a client disconnect kills ffmpeg; `Range` is
    ignored.
- Why not a fragmented MP4 streamed straight from ffmpeg: ExoPlayer
  (AntennaPod, most Android podcast apps) can't seek in one without a `sidx`,
  which also needs the whole episode. And with `empty_moov` ffmpeg writes the
  `moov` before the ADTS→ASC filter has seen a frame, leaving the `esds`
  without an AudioSpecificConfig, which ExoPlayer fails on.
- HEAD never fetches a whole episode: it only checks the playlist, unless the
  M4A is ready already.

## Audiobooks and library

- `/audiobook/<id>.xml` uses `audiobookById` for metadata and
  `audiobookAudioById` for the signed audio URL (cached for
  `AUDIOBOOK_AUDIO_CACHE_TIME`, 10 min by default).
- The library (`ENABLE_LIBRARY=true`, requires `LOCAL_CREDENTIALS=true`)\
  downloads books to `LIBRARY_DIR/<Author>/<Title>/` in Audiobookshelf's
  layout: `<Title>.mp3`, `cover.jpg`, `metadata.json`, plus our
  `podimo-state.json`. Downloads interrupted by a restart come back as failed.
  It never overwrites or deletes files it didn't create.
- `/setup` shows library diagnostics on every instance, but its path probe
  (`POST /setup/test-path`, and the `LIBRARY_DIR` check on the page) writes a
  file and has no auth, so it only runs with `LOCAL_CREDENTIALS=true`.

## Caches

`tokens` (5 days), `podcasts` (6 h), `audiobook_meta` (6 h), `audiobook_audio`
(10 min) and `head` (7 days). Entries live in moka and are mirrored to
`<CACHE_DIR>/<name>/<key>.json`, loaded lazily on first read. Expired entries
stay on disk: `get` skips them, `get_stale` returns them too. `url_head_info`
caches only 2xx answers. It retries a 5xx like a timeout or connection error,
and falls back to an expired size when every attempt fails.

## Code style

- Formatting: `cargo fmt` with default settings (2024 edition style).
- Error handling: `thiserror` for library-internal errors, `AppError` in
  `error.rs` for axum responses. Never panic on user input.
- Async runtime: `tokio` with `axum 0.8`.
- HTTP client: `reqwest 0.13` with `rustls`.
