# podimo-rs

An unofficial podcast and audiobook proxy for Podimo, written in Rust.

It exposes your subscribed podcasts as standard RSS feeds (so you can listen in
AntennaPod, Apple Podcasts, Pocket Casts, etc.) and can sync your audiobooks to
disk in a layout [Audiobookshelf](https://www.audiobookshelf.org/) understands.

## Quick start with Docker

```yaml
services:
  podimo:
    image: ghcr.io/midasvo/podimo-rs:latest
    container_name: podimo-rs
    restart: unless-stopped
    ports:
      - "3000:3000"
    environment:
      - PODIMO_ACCESS_TOKEN=your_token_here
      - PODIMO_REFRESH_TOKEN=your_refresh_token_here
      # Or:
      # - PODIMO_USERNAME=you@example.com
      # - PODIMO_PASSWORD=your_password
    volumes:
      - ./data/cache:/cache
      - ./data/audiobooks:/audiobooks   # optional, for the audiobook library
```

Navigate to `http://localhost:3000/setup` to get your tokens, or open
`http://localhost:3000` to build feed URLs.

## How it works

| Route | |
| --- | --- |
| `/` | Web form that builds your feed URL. |
| `/feed/<podcast-id>.xml` | Podcast feed. `?limit=N` keeps only the newest N episodes. |
| `/audiobook/<audiobook-id>.xml` | Audiobook as a feed with one episode. |
| `/stream/…/<episode-id>.m4a` or `.mp3` | Episode audio (the feed links here). |
| `/library` | Optional audiobook library (see configuration). |
| `/healthz` | Health check. |

Podimo streams episode audio as HLS, which podcast apps can't download. The
proxy fetches the stream and serves each episode as one file with
[ffmpeg](https://ffmpeg.org). By default that's an `.m4a` with Podimo's own AAC
audio, repackaged without re-encoding. The proxy fetches the whole episode
before it responds (a few seconds), so apps can seek and resume. Finished files
stay in the system temp directory for an hour after their last use, 2 GB at
most. Set `STREAM_FORMAT=mp3` to get 128 kbps MP3 instead, which is streamed
while it's encoded and costs about one CPU core per download. At most 16 episodes
download at once, 4 of them as MP3; further requests get `503 Service
Unavailable` with `Retry-After`.

Episode links use the address the feed was fetched from, so one instance serves
both your phone (`https://podimo.example.com`) and an Audiobookshelf container
on the same network (`http://podimo`).

**Behind a reverse proxy**, pass the original `Host` header on and set
`X-Forwarded-Proto` (nginx: `proxy_set_header Host $host;` and
`proxy_set_header X-Forwarded-Proto $scheme;`). If your proxy can't, set
`STREAM_LINKS_FROM_REQUEST=false` so episode links always use
`PODIMO_PROTOCOL://PODIMO_HOSTNAME`.

## Authentication

Podimo requires an active subscription. Three ways to authenticate:

1. **Tokens in `.env` (recommended)**: set `PODIMO_ACCESS_TOKEN` and
   `PODIMO_REFRESH_TOKEN`.
2. **Credentials in `.env`**: set `PODIMO_USERNAME` and `PODIMO_PASSWORD`.
   Tokens are fetched at startup and refreshed as needed.
3. **HTTP Basic auth per request**: `http://user:pass@localhost:3000/...` or
   pass an access token as the password. Ideal for shared instances where users
   have their own accounts.

## Configuration

All configuration is done via environment variables (or `.env` file):

| Variable | Default | Description |
| --- | --- | --- |
| `PODIMO_PORT` | `3000` | Port to listen on. |
| `PODIMO_HOSTNAME` | `localhost:3000` | Fallback hostname when no `Host` header is present. |
| `PODIMO_PROTOCOL` | `http` | Fallback protocol (`http` or `https`). |
| `STREAM_LINKS_FROM_REQUEST` | `true` | Derive stream links from incoming feed request host/proto. Set `false` behind proxies that alter Host. |
| `STREAM_FORMAT` | `m4a` | Enclosure format in feeds: `m4a` (AAC, seekable, default) or `mp3` (re-encoded 128 kbps). |
| `PUBLIC_FEEDS` | `false` | If `true`, feeds are served without auth (uses `.env` credentials). |
| `ENABLE_LIBRARY` | `false` | Enables the `/library` audiobook management UI. |
| `LOCAL_CREDENTIALS` | `false` | Must be `true` to use `/setup` and `/library`. |
| `PODIMO_REGION`, `PODIMO_LOCALE` | `nl`, `nl-NL` | Region and locale of the account in `.env`: used by the library, and by feeds unless the URL has `?region=` and `?locale=`. |
| `LIBRARY_DIR` | `/audiobooks` | Where audiobooks are stored. |
| `CACHE_DIR` | `/cache` | Where metadata and tokens are cached. |
| `BLOCK_LIST_FILE` | None | Path to a plain text file of podcast IDs/slugs to block (one per line). |
| `SCRAPER_API` | None | ScraperAPI key for proxying requests. |
| `ZENROWS_API` | None | ZenRows API key for proxying requests. |
| `HTTP_PROXY` | None | Standard HTTP proxy URL. |
| `PODCAST_CACHE_TIME` | `21600` (6h) | Episode metadata cache TTL in seconds. |
| `AUDIOBOOK_META_CACHE_TIME` | `21600` (6h) | Audiobook metadata cache TTL in seconds. |
| `AUDIOBOOK_AUDIO_CACHE_TIME` | `600` (10m) | Audiobook audio URL cache TTL in seconds. |
| `URL_HEAD_CACHE_TIME` | `604800` (7d) | HEAD response file size cache TTL in seconds. |
| `DEBUG` | `false` | Logs podimo-rs's debug messages, and every setting at startup. |
| `RUST_LOG` | None | Log filter ([`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax) replacing the defaults. hyper, reqwest, h2 and rustls stay at `warn` unless it names them. |
| `PODIMO_LOG_JSON` | `false` | Logs as JSON instead of text. |

## License

MIT
