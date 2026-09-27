# CLAUDE.md

Guidance for Claude Code in this repository.

## What this is

Self-hosted proxy that serves Podimo shows and audiobooks as RSS feeds. Rust:
axum on tokio, reqwest (rustls), moka + bincode caches, minijinja templates, the
`rss` crate. One binary, `podimo-rs`, on port 12104. Podimo delivers episode
audio as HLS; ffmpeg (must be on `PATH`) turns it into one file per episode.

## Commands

```sh
cp .env.example .env
cargo run --bin podimo-rs
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked    # the transcode tests need ffmpeg and ffprobe
```

CI runs these on PRs and `main`. Pushes to `main` publish
`ghcr.io/midasvo/podimo-rs:latest`; `vX.Y.Z` tags publish versioned images
(bump `crates/podimo-rs/Cargo.toml` to match first).

Renovate (`.github/renovate.json5`, checked by `renovate-config.yml`) runs
`cargo update` early on Mondays and merges that PR itself once CI passes, so
`:latest` picks up compatible crate releases weekly. Versions outside a
`Cargo.toml` range are breaking and get their own PR, merged by hand, as do
Docker and GitHub Actions updates.

## Routes

| Route | Handler |
| --- | --- |
| `GET /feed/<podcast_id>.xml[?limit=N]` | `handlers/feed.rs` |
| `GET /audiobook/<audiobook_id>.xml` | `handlers/audiobook.rs` |
| `GET /stream/<token>/<episode_id>.m4a` or `.mp3` | `handlers/stream.rs` |
| `GET /stream/<episode_id>.aac?src=…` (legacy, kept for stored URLs) | `handlers/stream.rs` |
| `GET, POST /` (form that builds feed URLs) | `handlers/index.rs` |
| `/library/*`, `/setup` (opt-in audiobook library) | `handlers/library.rs`, `handlers/setup.rs` |
| `GET /healthz` | `handlers/healthz.rs` |

`middleware.rs` adds CORS to GET/HEAD on `/feed`, `/audiobook` and `/stream`,
and `Cache-Control` (`max-age=900` on 2xx, `no-store` otherwise and on
`/healthz`).

## Layout

```
crates/podimo-rs/src/
  main.rs, lib.rs     entrypoint; `app()` builds the router
  config.rs           env + .env loading
  state.rs            AppState: config, caches, block list, HTTP client, templates
  handlers/           one file per route group; auth.rs is the shared credential gate
  podimo/client.rs    GraphQL login and queries
  podimo/rss.rs       feed rendering
  podimo/hls.rs       playlist parsing, TS → ADTS demux, ffmpeg transcode
  podimo/head.rs      HEAD probe for the size of non-HLS enclosures
  library/            audiobook downloads and on-disk layout
  cache.rs            TtlCache: moka in memory, bincode files on disk
  util.rs             helpers: auth parsing, request_base_url, amp_arg, …
crates/podimo-rs/templates/   HTML, embedded with include_str!
crates/podimo-rs/tests/       integration tests; Podimo is mocked with wiremock
```

## Feed flow

1. `auth::authorize_request` gets credentials: HTTP Basic with username
   `email,region,locale` by default, or `PODIMO_EMAIL`/`PODIMO_PASSWORD` plus
   `?region=&locale=` when `LOCAL_CREDENTIALS=true`. Region and locale default
   to `nl` and `nl-NL`. It also validates the ID and checks the block list
   (a listed token anywhere in the URL gives `410`).
2. `PodimoClient::login` does three GraphQL calls; the token is cached under
   `sha256(user~pass)`. `get_podcasts` pages episodes 100 at a time; `?limit`
   stops early and is part of the cache key.
3. `rss::podcasts_to_rss` renders the feed. HLS episodes link to `/stream`
   (below); other enclosures are HEAD-probed for their size.

## HLS episodes

Podimo's `streamMedia.url` is a signed master playlist: 192k and 320k variants
of ~10 s MPEG-TS segments with AAC-LC. There is no progressive file upstream.

- The enclosure is `<base>/stream/<token>/<id>.<ext>`. `token` is the playlist
  URL in unpadded base64url, so the link is a plain file path; Audiobookshelf
  picks the file type from the extension. `<base>` comes from the feed request
  (`util::request_base_url`: `X-Forwarded-Proto`/`X-Forwarded-Host`, else
  `http://` + `Host`), falling back to `PODIMO_PROTOCOL://PODIMO_HOSTNAME`.
  In-cluster clients thus get `http://podimo/…` and proxied ones the public
  host.
- `/stream` has no auth, since the signed URL is the credential, but only
  accepts https `.m3u8` URLs on `*.podimo.com`. It takes the highest-bandwidth
  variant, fetches segments 4 ahead with retries, demuxes TS to ADTS and pipes
  that through ffmpeg over stdin/stdout.
- `STREAM_FORMAT` (`hls::StreamFormat`) sets what feeds link to; both
  extensions are always served.
  - `m4a` (default): stream copy into fragmented MP4. Almost no CPU, original
    quality, and ffprobe reads the exact duration.
  - `mp3`: LAME, 128 kbps CBR at `-q 7`. About one core per download, at most
    4 at once. CBR keeps the duration right without a Xing header.
- A failure mid-stream ends the chunked body with an error rather than a short
  file that looks complete. A client disconnect kills ffmpeg. HEAD skips
  ffmpeg. `Range` is ignored.

## Audiobooks and library

- `/audiobook/<id>.xml` uses `audiobookById` for metadata and
  `audiobookAudioById` for the signed audio URL (cached for
  `AUDIOBOOK_AUDIO_CACHE_TIME`, 10 min by default).
- The library (`ENABLE_LIBRARY=true`, requires `LOCAL_CREDENTIALS=true`)
  downloads books to `LIBRARY_DIR/<Author>/<Title>/` in Audiobookshelf's
  layout: `<Title>.mp3`, `cover.jpg`, `metadata.json`, plus our
  `podimo-state.json`. Downloads interrupted by a restart come back as failed.
  It never overwrites or deletes files it didn't create.

## Caches

`tokens` (5 days), `podcasts` (6 h), `audiobook_meta` (6 h), `audiobook_audio`
(10 min) and `head` (7 days, read with `get_no_expire` so a stale size beats a
failed probe). Entries live in moka and are mirrored to
`<CACHE_DIR>/<name>/<key>.bin`, loaded lazily on first read.
`STORE_TOKENS_ON_DISK=false` keeps tokens in memory only.

## Gotchas

- `util::amp_arg` also accepts `amp;region`/`amp;locale`: some clients,
  Audiobookshelf among them, don't decode `&amp;` in feed URLs.
- Image URLs get a `#.jpg` fragment because Podimo's signed image URLs have no
  extension and some podcatchers require one.
- Podimo's API is behind Cloudflare, which blocks most data center IPs.
  `SCRAPER_API`, `ZENROWS_API` and `HTTP_PROXY` are checked in that order.
  Errors from `post_graphql` name only the host, never the proxy URL with its
  key.
- Logs are `LEVEL | timestamp | message`. `RUST_LOG` sets the level and
  `PODIMO_LOG_JSON=true` switches to JSON.
- Newer clippy flags `Result<_, Response>` as `result_large_err`. Handlers
  allow it locally because the error goes straight back to axum.
- `bincode` is unmaintained: 3.0.0 contains only a `compile_error!`. Stay on
  1.x; Renovate skips it.

## Known gaps

- Audiobook chapters: `audiobookAudioById` returns one file without chapters.
- The HTTP client shares one cookie jar across all users.
- Caches load lazily, so the first request after a restart logs in again.
