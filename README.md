<div align="center">

# podimo-rs

Self-hosted RSS proxy for [Podimo](https://podimo.com). Logs in to your account,
fetches the episode list for a given show, and serves it as an RSS 2.0 feed your
podcast player can subscribe to.

Rust rewrite of the original Python service (which was a fork of
[ThijsRay/podimo](https://github.com/ThijsRay/podimo)). The HTTP contract, config
surface, and `.block-list` format are preserved.

</div>

## What it does

You point your podcast player at a generated feed URL like

```
https://you%40example.com,nl,nl-NL:your-password@your-host/feed/<podcast-id>.xml
```

and it returns RSS 2.0 (with the iTunes namespace) that any normal podcast app
can consume. The web form at `/` builds those URLs for you.

There are two modes, selected with the `LOCAL_CREDENTIALS` env var:

- **Multi-user (default)** — credentials travel in HTTP Basic on the feed URL,
  so different users on the same instance see their own subscriptions.
- **Single-user (`LOCAL_CREDENTIALS=true`)** — credentials come from the host
  via `PODIMO_EMAIL` / `PODIMO_PASSWORD`. The form omits the email/password
  inputs. Recommended for self-hosted instances with one user.

## Run with Docker

```sh
docker run --rm \
    -e PODIMO_BIND_HOST=0.0.0.0:12104 \
    -p 12104:12104 \
    -v $(pwd)/cache:/app/cache \
    -e CACHE_DIR=/app/cache \
    ghcr.io/midasvo/podimo-rs:latest
```

Then visit <http://localhost:12104>.

Multi-arch images (linux/amd64, linux/arm64) are published from `main` and from
version tags. See <https://github.com/midasvo/podimo-ce/pkgs/container/podimo-rs>
for the tag list.

### `docker-compose.yml`

```yaml
services:
  podimo:
    image: ghcr.io/midasvo/podimo-rs:latest
    restart: unless-stopped
    ports:
      - "12104:12104"
    environment:
      PODIMO_BIND_HOST: 0.0.0.0:12104
      PODIMO_HOSTNAME: podimo.example.com
      PODIMO_PROTOCOL: https
      # LOCAL_CREDENTIALS: "true"
      # PODIMO_EMAIL: you@example.com
      # PODIMO_PASSWORD: hunter2
    volumes:
      - ./cache:/app/cache
```

## Run from source

Requires a Rust toolchain (1.80+). [rustup](https://rustup.rs) is the usual
install path. Podimo now serves episodes as HLS, which the proxy converts to
MP3 with [ffmpeg](https://ffmpeg.org), so `ffmpeg` must be on your `PATH`
too (the Docker image includes it).

```sh
git clone https://github.com/midasvo/podimo-ce        # repo name is `podimo-ce`; binary is `podimo-rs`
cd podimo-ce
cp .env.example .env                                   # edit as needed
cargo run --release --bin podimo-rs
```

The service binds `PODIMO_BIND_HOST` (default `127.0.0.1:12104`).

## Configuration

All knobs are env vars; see [`.env.example`](.env.example) for the full list
with defaults. A `.env` file at the working directory is picked up
automatically.

The high-impact ones:

| Var | Default | Purpose |
| --- | --- | --- |
| `PODIMO_BIND_HOST` | `127.0.0.1:12104` | Listen address. Set `0.0.0.0:12104` inside containers. |
| `PODIMO_HOSTNAME` | `localhost:12104` | Hostname shown in generated feed URLs (use your reverse-proxy hostname). |
| `PODIMO_PROTOCOL` | `http` | Scheme shown in generated URLs (`https` once behind a TLS terminator). |
| `LOCAL_CREDENTIALS` | `false` | If `true`, read creds from `PODIMO_EMAIL` / `PODIMO_PASSWORD` instead of HTTP Basic. |
| `CACHE_DIR` | `./cache` | Root for the three on-disk caches (tokens, podcasts, head probes). |
| `BLOCK_LIST_FILE` | `./.block-list` | One token per line; if any token is a substring of the request URL the feed returns `410`. See `.block-list.example`. |

### Bot-protection bypass

Podimo sits behind Cloudflare. Requests from datacenter IPs (most VPSes) are
unreliable without one of:

- `SCRAPER_API` — your ScraperAPI key. Requests get URL-rewritten through
  `api.scraperapi.com`. Free tier at <https://dashboard.scraperapi.com/signup>.
- `ZENROWS_API` — your ZenRows key. Requests get routed via `api.zenrows.com`.
  Free trial at <https://app.zenrows.com/register>.
- `HTTP_PROXY` — a residential HTTPS proxy URL.

Only one of the three needs to be set; they're checked in that order. A
residential IP (e.g. self-hosting from home) usually doesn't need any of them.

## Endpoints

| Path | Method | Body |
| --- | --- | --- |
| `/` | GET, POST | HTML form for building feed URLs. POST validates input and renders the resulting URL. |
| `/feed/<podcast_id>.xml` | GET | RSS 2.0 feed. HTTP Basic auth in multi-user mode; env-var creds in single-user mode. |
| `/healthz` | GET | `200 {"status":"ok"}` with `Cache-Control: no-store`. Container HEALTHCHECK target. |

Caching policy:

- `/healthz` is never cached.
- 2xx responses get `Cache-Control: max-age=900`.
- Everything else (401, 4xx, 5xx) gets `Cache-Control: no-store` — prevents
  upstream blips from stickying through CDNs.

CORS is scoped to `GET`/`HEAD` of `/feed/*` so podcast clients can fetch
cross-origin without exposing the form's POST endpoint to other origins.

## Block list

Drop a `.block-list` file next to the service (or point `BLOCK_LIST_FILE` at
one elsewhere). One token per line; `#` starts a comment; only the first
whitespace-separated token of each line is used. Any line whose token appears
anywhere in the request URL returns `410 GONE`. Tokens can be podcast IDs or
the random 10-character cache-buster the form generates. See
[`.block-list.example`](.block-list.example).

## Privacy

What lives in memory (or on disk under `CACHE_DIR` when `STORE_TOKENS_ON_DISK`
is true, which is the default):

- Your email + password — only used to obtain a Podimo access token; never
  written to logs.
- `sha256("<email>~<password>")` — used as the cache key for your access token.
- The Podimo access token itself.

The token cache defaults to a 5-day TTL. Wipe `CACHE_DIR` to forget everything.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked

docker build -t podimo-rs:test .
docker run --rm -p 12104:12104 podimo-rs:test
curl http://127.0.0.1:12104/healthz   # → 200 {"status":"ok"}
```

CI runs the same three commands on every push and PR. The Docker image is
published from `main` and version tags.

## License

EUPL-1.2.

```
Copyright 2022-2023 Thijs Raymakers
Copyright 2025-2026 Midas van Oene

Licensed under the EUPL, Version 1.2 or – as soon they will be approved by
the European Commission - subsequent versions of the EUPL (the "Licence");
You may not use this work except in compliance with the Licence.
You may obtain a copy of the Licence at:

https://joinup.ec.europa.eu/software/page/eupl

Unless required by applicable law or agreed to in writing, software
distributed under the Licence is distributed on an "AS IS" basis,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the Licence for the specific language governing permissions and
limitations under the Licence.
```
