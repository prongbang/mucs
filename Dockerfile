# ---------- build the console ----------
# Pinned: the moving `1` tag lets a host reuse a node_modules layer that an
# older bun installed, and the stale vite shim in it breaks the build.
FROM oven/bun:1.3.14 AS web
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

# libgomp1 is torch's, not a build dependency — it lives here so the toolchain
# teardown below can't take it along.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg ca-certificates curl libgomp1 && \
    rm -rf /var/lib/apt/lists/*

# CPU-only torch keeps the image around 1GB instead of 6GB+.
#
# demucs 4.1 depends on sphn, which publishes linux wheels for x86_64 only. On
# every other arch pip falls back to the sdist, and that's a pyo3 crate: its
# build script fetches its own Rust toolchain but not a C linker, and it
# vendors libopus through cmake, where pip's build-isolated CMake 4 rejects the
# pre-3.5 policies libopus still declares. So arm64 needs a compiler for one
# package, and gets it only for as long as that package takes to build.
#
# TARGETARCH comes from BuildKit. If some older builder leaves it empty the
# test falls to the toolchain side, which is the harmless direction — a slower
# build rather than a broken one.
ARG TARGETARCH
RUN set -eux; \
    if [ "$TARGETARCH" != "amd64" ]; then \
        apt-get update; \
        apt-get install -y --no-install-recommends build-essential; \
    fi; \
    pip install --no-cache-dir torch --index-url https://download.pytorch.org/whl/cpu; \
    CMAKE_POLICY_VERSION_MINIMUM=3.5 \
        pip install --no-cache-dir "demucs==4.1.0" lameenc numpy; \
    if [ "$TARGETARCH" != "amd64" ]; then \
        apt-get purge -y build-essential; \
        apt-get autoremove -y; \
    fi; \
    rm -rf /var/lib/apt/lists/* /root/.cache/puccinialin /root/.cargo

COPY --from=builder /build/target/release/demucs-service /usr/local/bin/demucs-service

# Warm the model into the image so the first request isn't a 300MB download.
ENV TORCH_HOME=/opt/torch
RUN python -c "from demucs.pretrained import get_model; get_model('htdemucs')"

RUN mkdir -p /data
VOLUME ["/data"]
EXPOSE 8080

CMD ["demucs-service"]
