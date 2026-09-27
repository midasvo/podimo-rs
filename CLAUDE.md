# CLAUDE.md

Guidance for Claude Code in this repository.

## What this is

Self-hosted proxy that serves Podimo shows and audiobooks as RSS feeds. Rust:
axum on tokio, reqwest (rustls), moka caches, minijinja templates, the
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
| `/library/*`, `/setup` (opt-in audiobook library; `/setup/test-path` needs `LOCAL_CREDENTIALS=true`) | `handlers/library.rs`, `handlers/setup.rs` |
| `GET /healthz` | `handlers/healthz.rs` |

`middleware.rs` adds CORS and `Cache-Control: max-age=900` to 2xx GET/HEAD
responses on `/feed`, `/audiobook` and `/stream`; everything else, the HTML
pages included, is `no-store`.

## Layout

```
crates/podimo-rs/src/
  main.rs, lib.rs     entrypoint; `app()` builds the router
  config.rs           env + .env loading
  state.rs            AppState: config, caches, block list, HTTP client, templates
  handlers/           one file per route group; auth.rs is the shared credential gate
  podimo/client.rs    GraphQL login and queries
  podimo/rss.rs       feed rendering
  podimo/hls.rs       playlist parsing, TS → ADTS demux, ffmpeg transcode and remux
  podimo/head.rs      HEAD probe for the size of non-HLS enclosures
  episode_files.rs    finished M4A episodes in a temp dir, one preparation per episode
  library/            audiobook downloads and on-disk layout
  cache.rs            TtlCache: moka in memory, JSON files on disk
  util.rs             helpers: auth parsing, request_base_url, amp_arg, …
crates/podimo-rs/templates/   HTML, embedded with include_str!
crates/podimo-rs/tests/       integration tests; Podimo is mocked with wiremock
```

## Feed flow

1. `auth::authorize_request` gets credentials: HTTP Basic with username
   `email,region,locale` by default (region and locale default to `nl` and
   `nl-NL`), or `PODIMO_EMAIL`/`PODIMO_PASSWORD` when
   `LOCAL_CREDENTIALS=true`, with `?region=&locale=` or else `PODIMO_REGION`
   and `PODIMO_LOCALE` (an unknown value fails startup). It also validates the
   ID and checks the block list (a listed token anywhere in the
   percent-decoded URL, in any case, gives `410`). `/stream` checks it too,
   against the episode id and playlist URL.
2. `PodimoClient::login` does three GraphQL calls; the token is cached under
   `sha256(user~pass)`. A GraphQL error on the login query itself is a wrong
   email or password: 401. When a query with a cached token fails with
   anything but not-found, the token is dropped so the next request logs in
   again. `get_podcasts` pages episodes 100 at a time and `?limit` stops after
   the pages it needs. The listing is cached per account (that hash) and
   show: `p<n>` for the first `n` pages, shared by every limit in that
   hundred, or `all` once the show ran out, which serves any limit. The
   audiobook caches are per account too, since payloads hold signed URLs.
3. `rss::podcasts_to_rss` renders the feed. HLS episodes link to `/stream`
   (below); other enclosures are HEAD-probed for their size. Dates are
   RFC 822, and characters XML 1.0 forbids are stripped from the document.

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
    it's produced (4 segments ahead). About one core per download. CBR keeps
    the duration right without a Xing header. A failure mid-stream ends the
    chunked body with an error rather than a short file that looks complete;
    a client disconnect kills ffmpeg; `Range` is ignored.
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
- The library (`ENABLE_LIBRARY=true`, requires `LOCAL_CREDENTIALS=true`)
  downloads books to `LIBRARY_DIR/<Author>/<Title>/` in Audiobookshelf's
  layout: `<Title>.mp3`, `cover.jpg`, `metadata.json`, plus our
  `podimo-state.json`. Downloads interrupted by a restart come back as failed
  and can be retried from `/library`. It never overwrites or deletes files it
  didn't create. It logs in with `PODIMO_REGION` / `PODIMO_LOCALE`, takes a
  bare UUID on its form as an audiobook, and cuts directory and file names to
  200 bytes (not characters) on a character boundary.
- `/setup` shows library diagnostics on every instance, but its path probe
  (`POST /setup/test-path`, and the `LIBRARY_DIR` check on the page) writes a
  file and has no auth, so it only runs with `LOCAL_CREDENTIALS=true`.

## Caches

`tokens` (5 days), `podcasts` (6 h), `audiobook_meta` (6 h), `audiobook_audio`
(10 min) and `head` (7 days). Entries live in moka and are mirrored to
`<CACHE_DIR>/<name>/<key>.json`, loaded lazily on first read. Expired entries
stay on disk: `get` skips them, `get_stale` returns them too. `url_head_info`
caches only 2xx answers. It retries a 5xx like a timeout or connection error,
but not another error status. When the probe fails it returns the expired
`head` entry, since a stale size beats a failed probe. Without one, an error
status gives length `0` (not cached) and no answer at all an error.
`STORE_TOKENS_ON_DISK=false` keeps tokens in memory only.
Startup deletes the bincode files that 1.2.0 and earlier left in
`<CACHE_DIR>/*_cache/`.

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
- The disk mirror must stay JSON or another self-describing format:
  `serde_json::Value` (in `podcasts` and `audiobook_meta`) can't be read back
  from bincode or postcard, and a failed read looks like a cache miss.

## Known gaps

- Audiobook chapters: `audiobookAudioById` returns one file without chapters.
- The HTTP client shares one cookie jar across all users.
