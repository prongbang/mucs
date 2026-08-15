# ---------- build the console ----------
FROM oven/bun:1 AS web
WORKDIR /web
COPY web/package.json web/bun.lockb ./
RUN bun install --frozen-lockfile
COPY web ./
RUN bun run build

# ---------- build the Rust binary ----------
FROM rust:1.94-bookworm AS builder
WORKDIR /build

# Cache dependencies separately from the source.
COPY Cargo.toml Cargo.lock* build.rs ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && rm -rf src

# The console goes in before the compile — rust-embed reads it at build time.
COPY --from=web /web/build ./web/build
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---------- runtime: python + demucs + the binary ----------
FROM python:3.11-slim-bookworm
WORKDIR /app

RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

# CPU-only torch keeps the image around 1GB instead of 6GB+.
RUN pip install --no-cache-dir \
        torch --index-url https://download.pytorch.org/whl/cpu && \
    pip install --no-cache-dir "demucs>=4.0.1" lameenc

COPY --from=builder /build/target/release/demucs-service /usr/local/bin/demucs-service

# Warm the model into the image so the first request isn't a 300MB download.
ENV TORCH_HOME=/opt/torch
RUN python -c "from demucs.pretrained import get_model; get_model('htdemucs')"

RUN mkdir -p /data
VOLUME ["/data"]
EXPOSE 8080

CMD ["demucs-service"]
