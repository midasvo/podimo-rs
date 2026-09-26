# CLAUDE.md

Guidance for Claude Code working on this repository.

## What this is

Self-hosted RSS proxy for Podimo. Rust service: axum on tokio, reqwest for
outbound, moka + bincode for caches, minijinja for the form templates, the
`rss` crate for output. Single binary `podimo-rs` listens on port 12104.

Endpoints:
- `GET /feed/<podcast_id>.xml[?limit=N]` — podcast feed, optionally trimmed to
  the latest N episodes.
- `GET /audiobook/<audiobook_id>.xml` — single-item RSS feed for one Podimo
  audiobook (the book itself is the lone episode).
- `GET /stream/<token>/<episode_id>.mp3` — transcodes an HLS episode to one
  progressive MP3 (ffmpeg). Feed enclosures point here for any episode whose
  audio URL is an `.m3u8` (see "HLS episodes" below). The older
  `GET /stream/<episode_id>.aac?src=…` AAC remux is kept for stored URLs.
- `GET /library` + `/library/*` — opt-in audiobook library (downloads books
  to disk for permanent local storage). Requires `ENABLE_LIBRARY=true` +
  `LOCAL_CREDENTIALS=true`.

History: this was a Python service (Quart on Hypercorn), itself a fork of
`ThijsRay/podimo`. The Rust rewrite landed in PR #2 and replaced Python at the
root in PR #3. The Python source isn't in the tree anymore — `git log` from
before the cutover is the only reference. The HTTP contract was preserved,
including the fork-local patches (`amp;`-prefix query fallback, `#.jpg` image
fragment, `nl`/`nl-NL` region/locale defaults).

## Running locally

```sh
cp .env.example .env       # then edit; DEBUG=true gives verbose request logs
cargo run --bin podimo-rs  # binds PODIMO_BIND_HOST (default 127.0.0.1:12104)
```

Tests + lints:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
```

Docker build matches CI: `docker build -t podimo-rs:test .` then
`docker run -p 12104:12104 podimo-rs:test`.

## Architecture

```
.
├── Cargo.toml                       # workspace
├── crates/
│   └── podimo-rs/
│       ├── Cargo.toml               # package
│       ├── src/
│       │   ├── main.rs              # entrypoint, signal handling, listener
│       │   ├── lib.rs               # `app()` factory + module re-exports
│       │   ├── config.rs            # env+dotenv loading; REGIONS/LOCALES
│       │   ├── error.rs             # AppError + IntoResponse
│       │   ├── state.rs             # AppState (config, caches, blocklist, scraper, templates)
│       │   ├── handlers/
│       │   │   ├── auth.rs          # shared authorize_request gate
│       │   │   ├── healthz.rs       # GET /healthz
│       │   │   ├── index.rs         # GET/POST /
│       │   │   ├── feed.rs          # GET /feed/<id>.xml
│       │   │   ├── audiobook.rs     # GET /audiobook/<id>.xml
│       │   │   ├── library.rs       # GET /library + /library/* (opt-in)
│       │   │   ├── stream.rs        # GET /stream/<token>/<id>.mp3 (HLS → MP3)
│       │   │   └── not_found.rs     # fallback
│       │   ├── library/
│       │   │   ├── mod.rs           # Library struct + on-disk hydration
│       │   │   └── download.rs      # background download task
│       │   ├── middleware.rs        # after-request CORS + Cache-Control
│       │   ├── podimo/
│       │   │   ├── client.rs        # GraphQL login + getPodcasts + audiobook queries
│       │   │   ├── head.rs          # episode HEAD probe with retries
│       │   │   ├── hls.rs           # m3u8 parsing + MPEG-TS → ADTS demux
│       │   │   └── rss.rs           # podcasts_to_rss + audiobook_to_rss
│       │   ├── cache.rs             # TtlCache + on-disk bincode persistence
│       │   ├── blocklist.rs
│       │   ├── templates.rs         # minijinja env (templates embedded with include_str!)
│       │   └── util.rs              # token_key, randomHexId, randomFlyerId, email validation, _arg
│       ├── templates/
│       │   ├── base.html
│       │   ├── index.html
│       │   └── feed_location.html
│       └── tests/
│           ├── integration_healthz.rs
│           ├── integration_feed.rs                 # validation + middleware parity
│           ├── integration_feed_upstream.rs        # wiremock-mocked GraphQL
│           ├── integration_audiobook.rs            # audiobook validation + middleware
│           ├── integration_audiobook_upstream.rs   # wiremock-mocked audiobook flow
│           ├── integration_head.rs                 # wiremock-mocked HEAD probe
│           ├── integration_rss.rs                  # structural RSS rendering
│           └── integration_stream.rs               # /stream routing + validation
```

Feed request flow:

1. `handlers/feed.rs::serve` resolves credentials. Two modes selected by
   `LOCAL_CREDENTIALS`:
   - **off (multi-user)**: HTTP Basic. The username field is overloaded as
     `email,region,locale` (comma-separated, URL-encoded). See
     `util::split_username_region_locale`; defaults to `nl`/`nl-NL` if the
     comma fields are missing.
   - **on (single-user)**: creds from `PODIMO_EMAIL`/`PODIMO_PASSWORD`;
     region/locale come from the query string with the same defaults.
2. Validation: podcast id regex (`[0-9a-fA-F-]+`), region/locale enum
   membership, block-list substring match (`410` if hit).
3. `PodimoClient::login` does the three-step GraphQL dance:
   `tokenWithPreregisterUser` → `userOnboardingFlow` → `tokenWithCredentials`.
   The final token is cached under `sha256(username~password)` (see
   `util::token_key`).
4. `PodimoClient::get_podcasts` pages through `podcastEpisodes` 100 at a time
   and returns the raw GraphQL payload. When `?limit=N` is set on the request,
   pagination is short-circuited: only `ceil(N/100)` pages are fetched, and the
   cache key is namespaced by limit so different limits don't poison one
   another's cache.
5. `podimo::rss::podcasts_to_rss` builds RSS via the `rss` crate's iTunes
   extensions and runs `url_head_info` HEAD probes for enclosure metadata in
   chunks of 10 concurrent requests.

### HLS episodes (`podimo/hls.rs`, `handlers/stream.rs`)

Podimo no longer returns a progressive `audio.url`; `streamMedia.url`
is a signed master playlist on `media-cdn-episodes.podimo.com` (192k/320k
variants of ~10 s MPEG-TS segments carrying AAC-LC, unencrypted). No MP3
sibling is reachable on the CDN.

`podcasts_to_rss` rewrites such enclosures to
`{PODIMO_PROTOCOL}://{PODIMO_HOSTNAME}/stream/<token>/<id>.mp3` with
`type="audio/mpeg"` and `length="0"` (size unknown up front; no HEAD probe).
`token` is the signed `.m3u8` URL in unpadded base64url: carrying it in the
path keeps the URL a plain `….mp3` with no query string and no `.m3u8`
anywhere in it — Audiobookshelf derives the file type from the URL, and raw
AAC also leaves it guessing the duration (no header to read it from).

`/stream` needs no auth — the signed playlist URL is the credential — but only
accepts `https` playlists on `*.podimo.com`. It picks the highest-`BANDWIDTH`
variant, fetches segments 4 ahead with retries, and a tiny TS demuxer (PAT →
PMT → stream_type 0x0F → PES payloads) turns them into raw ADTS AAC. That is
piped into `ffmpeg -f aac -i pipe:0 -c:a libmp3lame -b:a 192k -f mp3 pipe:1`
(stdin/stdout only; ffmpeg never touches the network), at most 4 at a time.
A failed upstream fetch or ffmpeg failure ends the chunked body with an error
so clients see a broken download, not a short "complete" one; dropping the
body kills ffmpeg. `HEAD` resolves the playlists but skips the transcode.
`Range` is ignored (200 with the full stream).

The earlier `GET /stream/<id>.aac?src=<url-encoded m3u8>` (ADTS AAC, no
ffmpeg) is still served for enclosure URLs podcatchers stored before the MP3
switch. `ffmpeg` must be on `PATH`; startup logs its version or a warning.

### Library flow (opt-in)

When `ENABLE_LIBRARY=true` *and* `LOCAL_CREDENTIALS=true`, the service exposes
a persistent audiobook library at `/library`:

- `GET  /library` — HTML overview with cover thumbnails, status badges,
  per-book progress bars during download.
- `POST /library/add` — accepts a Podimo audiobook URL or bare UUID. Fetches
  metadata synchronously (so the new row appears immediately), then spawns a
  background task that re-mints the signed audio URL, streams `audio.mp3` to
  disk via `.partial` → rename, and downloads `cover.jpg`. Errors are
  surfaced on the entry (status=failed + error message).
- `POST /library/<id>/remove` — drops the entry and its on-disk directory.
- `GET  /library/<id>/audio.mp3` — streams the local file with
  `Content-Disposition: attachment; filename="<title>.mp3"`.
- `GET  /library/<id>/cover.jpg` — local cover image.

Storage layout follows the Audiobookshelf convention so the same directory
can be mounted into both this proxy and an ABS instance without
restructuring:

```
LIBRARY_DIR/
  <Author Name>/
    <Book Title>/
      <Book Title>.mp3      # audio (written via .partial → rename)
      cover.jpg             # cover image
      metadata.json         # Audiobookshelf-format metadata
      podimo-state.json     # our internal state (UUID, status, progress)
```

Hydration on startup walks `LIBRARY_DIR/**/podimo-state.json`. Entries that
crashed mid-download (status=queued/downloading) are forced to
`Failed("interrupted by restart")` since the signed URL would be expired by
the time we got here. Migration: any legacy `LIBRARY_DIR/<uuid>/meta.json`
directories from before this layout get moved to `Author/Title/`,
`audio.mp3` renamed to `<Title>.mp3`, and an ABS `metadata.json` dropped
alongside.

The library requires `LOCAL_CREDENTIALS=true` because library entries are
inherently single-user and the background download task needs unattended
access to Podimo creds — when this constraint is violated at startup a
warning is logged and the library stays `None`, which makes all `/library`
routes 404.

### Audiobook feed flow

`GET /audiobook/<audiobook_id>.xml` mirrors the podcast endpoint (same
basic-auth / region+locale / blocklist validation, same login flow) but
dispatches to two GraphQL queries from `audiobook-dl`'s reverse-engineered
schema:

1. `audiobookById` — static-ish metadata (title, authors, narrators, cover,
   duration). Cached for `PODCAST_CACHE_TIME` under `audiobook_meta_cache`.
2. `audiobookAudioById` — short-lived signed audio URL. Cached for
   `AUDIOBOOK_AUDIO_CACHE_TIME` (default 10 min) under `audiobook_audio_cache`,
   so podcatchers that fetch the feed but defer playback still get a fresh
   link on the next poll.

The RSS render produces a one-channel-one-item feed: the audiobook itself is
the lone episode. Narrators and publisher are appended to the item description
since podcatchers don't have a first-class field for either.

### Caching (`crates/podimo-rs/src/cache.rs`)

Five caches under `CACHE_DIR` (default `./cache`):

- `tokens_cache` — login tokens, TTL `TOKEN_CACHE_TIME` (5 days). In-memory
  only if `STORE_TOKENS_ON_DISK=false`; otherwise shadowed to disk.
- `podcast_cache` — full GraphQL payload per podcast, TTL `PODCAST_CACHE_TIME`
  (6 h). Determines how often new episodes appear in the feed.
- `audiobook_meta_cache` — `audiobookById` payload per audiobook, same TTL as
  `podcast_cache`.
- `audiobook_audio_cache` — signed audio URL per audiobook, TTL
  `AUDIOBOOK_AUDIO_CACHE_TIME` (10 min). Short on purpose.
- `head_cache` — `HeadInfo { content_length, content_type }` per episode id
  (or `audiobook__<id>` for audiobooks), TTL `HEAD_CACHE_TIME` (7 days). Read
  via `get_no_expire` so the on-disk record survives expiry (we always re-probe
  HEAD on expiry, but the historical record is kept for inspection).

`TtlCache<V>` wraps `moka::future::Cache` for in-memory TTL eviction and
shadows each entry as a bincode blob under `<CACHE_DIR>/<name>/<key>.bin`. On
expiry, reads return `None` and a fresh fetch repopulates. **The on-disk format
is not compatible with the old Python `diskcache`**; wipe `CACHE_DIR` on
cutover.

### Anti-bot bypass (`crates/podimo-rs/src/podimo/client.rs::post_graphql`)

Requests to `podimo.com/graphql` go through `reqwest` with no special browser
fingerprinting. Three mutually-exclusive overrides, checked in this order:

1. `SCRAPER_API` — URL is rewritten through ScraperAPI's `api.scraperapi.com`
   proxy with `keep_headers=true`.
2. `ZENROWS_API` — URL is rewritten through ZenRows' `api.zenrows.com` proxy.
3. `HTTP_PROXY` — used as the `https` proxy on the `reqwest::Client`.

The base case (no override) is plain `reqwest` and is unreliable from
datacenter IPs — Cloudflare will challenge it. Production deployments on VPSes
should set one of the three. Possible follow-up: add `rquest` (TLS+HTTP2
fingerprint impersonation) for a better default, but not urgent.

API-key safety: error messages from `post_graphql` only include the target
**host**, never the formatted proxy URL — the SCRAPER_API/ZENROWS_API key never
leaks into logs.

## Non-obvious carry-overs from the Python service

Preserved verbatim:

- **`util::amp_arg`** accepts `amp;region` / `amp;locale` as fallbacks because
  some downstream tools (e.g. Audiobookshelf) consume the rendered HTML feed
  URL without decoding `&amp;`, so the param arrives prefixed with `amp;`.
- **`#.jpg` image fragment** — Podimo image URLs are signed query strings with
  no file extension, but the `rss` crate / Apple's spec demand `.jpg`/`.png`.
  We append `#.jpg`; clients strip fragments before GET. Applied in both
  channel-level and per-item images in `podimo::rss`.
- **Region/locale defaults** — `util::split_username_region_locale` and the
  `LOCAL_CREDENTIALS` query-string path both default to `nl`/`nl-NL` when the
  fields are missing.

## Block list

`.block-list` (path overridable via `BLOCK_LIST_FILE`) is loaded by
`AppState::new`. Format: one token per line; lines starting with `#` are
comments; only the first whitespace-separated token is used. If any entry is a
substring of the request URI (path+query), the feed returns `410 GONE`. See
`.block-list.example`.

## Logging

`tracing` + `tracing-subscriber` with a custom line formatter that mirrors the
Python service's `LEVEL | YYYY-MM-DDThh:mm:ssZ | message` shape — log shippers
don't need re-tuning. `RUST_LOG` controls level; `PODIMO_LOG_JSON=true` swaps
to structured JSON output.

## Container

Multi-stage Dockerfile: `rust:1.95-slim-bookworm` builder → `debian:bookworm-slim`
runtime, static-linked-ish via rustls (no OpenSSL dependency). The runtime
stage installs Debian's `ffmpeg` for the MP3 transcode. Binary lives at
`/usr/local/bin/podimo-rs`, exposes port 12104, `HEALTHCHECK` hits `/healthz`.

CI publishes `ghcr.io/midasvo/podimo-rs` from `main` pushes, semver tags, or
manual dispatch. Multi-arch (linux/amd64, linux/arm64).

## Deferred items

(Kept short; full list in `MIGRATION_NOTES.md`.)

- **Cloudflare challenge solving without an API proxy** — see
  `client.rs::post_graphql`. Mitigated by ScraperAPI/ZenRows/HTTP_PROXY.
- **Per-user cookie jars** — `reqwest::Client` has a process-wide jar; Python
  kept one per `token_key`. Probably fine; verify only if Podimo's GraphQL
  endpoint actually relies on per-user cookies.
- **Cache hydration on startup** — `TtlCache` is lazily hydrated on first read;
  a cold start re-authenticates against Podimo on the very first request.
- **Side-by-side production test** — never validated against the live Podimo
  upstream; only against wiremock fakes. Worth a one-off comparison before
  treating any new behaviour as authoritative.
- **Audiobook chapters** — the `audiobookAudioById` query returns a single
  audio URL with no chapter metadata, and a brief search turned up no
  `audiobookChaptersById`-style query in any reverse-engineered Podimo client.
  For now the audiobook feed is one blob = one item. Future work: explore the
  app's actual network traffic to see whether chapters live behind a separate
  GraphQL field, then split into per-chapter RSS items.
