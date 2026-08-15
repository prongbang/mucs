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
#
# demucs 4.1 depends on sphn, which publishes linux wheels for x86_64 only — on
# arm64 pip falls back to the sdist, and that's a pyo3 crate. Its build script
# fetches its own Rust toolchain but not a C linker, so `cc` has to be here.
# libgomp1 is pulled in by name because torch needs it at runtime and it would
# otherwise leave with build-essential on the autoremove.
#
# sphn vendors libopus and builds it through the cmake crate. pip's build
# isolation supplies CMake 4, which dropped support for the pre-3.5 policies
# that libopus' CMakeLists still declares — this is the escape hatch CMake's
# own error message names. Harmless on x86_64, where the wheel is used instead.
ENV CMAKE_POLICY_VERSION_MINIMUM=3.5
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential libgomp1 && \
    pip install --no-cache-dir \
        torch --index-url https://download.pytorch.org/whl/cpu && \
    pip install --no-cache-dir "demucs==4.1.0" lameenc numpy && \
    apt-get purge -y build-essential && \
    apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/* /root/.cache/puccinialin /root/.cargo

COPY --from=builder /build/target/release/demucs-service /usr/local/bin/demucs-service

# Warm the model into the image so the first request isn't a 300MB download.
ENV TORCH_HOME=/opt/torch
RUN python -c "from demucs.pretrained import get_model; get_model('htdemucs')"

RUN mkdir -p /data
VOLUME ["/data"]
EXPOSE 8080

CMD ["demucs-service"]
