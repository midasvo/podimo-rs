# syntax=docker/dockerfile:1.7
# Multi-stage build using `cargo-chef` to keep dep compilation in its own
# layer. The dep layer only invalidates when `Cargo.toml` / `Cargo.lock`
# change, so source-only edits skip ~150 transitive crate compiles and the
# build finishes in seconds instead of minutes.

# ---- chef base: rust toolchain + cargo-chef ----
FROM rust:1.95-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# ---- planner: emit a dep-only recipe from Cargo.{toml,lock} ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock rustfmt.toml clippy.toml ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder: cook deps (cached layer) then build our code ----
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Compile every dependency from the recipe. This is the expensive step but
# its inputs (recipe.json) only change when Cargo.{toml,lock} change, so it
# stays cached across our usual source edits.
RUN cargo chef cook --release --recipe-path recipe.json --bin podimo-rs

# Now bring in the actual source and build our binary. With the dep
# artifacts already in `target/`, this step only recompiles podimo-rs
# itself — a few seconds.
COPY Cargo.toml Cargo.lock rustfmt.toml clippy.toml ./
COPY crates ./crates
RUN cargo build --release --locked --bin podimo-rs

# ---- runtime ----
FROM debian:bookworm-slim AS runtime

# ffmpeg turns Podimo's HLS episodes into M4A/MP3 files (see podimo/hls.rs).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget ffmpeg \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/podimo-rs /usr/local/bin/podimo-rs
COPY crates/podimo-rs/templates ./templates

ENV PODIMO_BIND_HOST=0.0.0.0:12104
EXPOSE 12104

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -q -O - http://127.0.0.1:12104/healthz | grep -q '"ok"' || exit 1

ENTRYPOINT ["/usr/local/bin/podimo-rs"]
