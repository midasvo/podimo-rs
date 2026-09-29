# Rust with rustfmt, clippy and rust-analyzer, for the non-root user "vscode".
# Bookworm like the runtime image in ../Dockerfile, so the tests run against
# the ffmpeg that production has.
FROM mcr.microsoft.com/devcontainers/rust:2-1-bookworm

# ffmpeg and ffprobe, for the transcode tests in podimo/hls.rs.
RUN apt-get update \
    && export DEBIAN_FRONTEND=noninteractive \
    && apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*
