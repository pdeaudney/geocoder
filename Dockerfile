# Stage 1: Build C++ indexer
FROM debian:bookworm-slim AS builder-cpp

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake \
    libosmium2-dev libprotozero-dev \
    libs2-dev \
    zlib1g-dev libbz2-dev libexpat1-dev liblz4-dev \
    libdeflate-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY builder/ builder/
RUN mkdir build && cd build && cmake ../builder && make -j$(nproc)

# Stage 2: Build Rust server
FROM rust:bookworm AS builder-rust

# libicu-dev is required by the `translit` feature (default-on) for
# building build-forward-index / build-autocomplete-fst with
# Cyrillic / Han / Arabic / Greek / Hebrew / Thai / Devanagari Latin
# transliteration. The runtime query-server doesn't link libicu —
# but the build binaries do, and they're shipped into the runtime
# image so the `auto` entrypoint can rebuild the index.
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake protobuf-compiler \
    libicu-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY server/ server/
# This builder copies only `server/`, so cargo runs without the
# workspace Cargo.toml and the workspace-level `[profile.release]`
# (lto = "thin") wouldn't otherwise apply. Set it on the command
# line to keep parity with local + Packer builds. See
# docs/performance/lto-config-2026-04-26.md for why thin over fat.
RUN cargo build --release --manifest-path server/Cargo.toml \
    --config 'profile.release.lto="thin"' \
    --bins

# Stage 3: Runtime
FROM debian:bookworm-slim

# libicu72 is the runtime shared library for libicu (matches the
# libicu-dev version on Bookworm). Required because the build
# binaries (build-forward-index, build-autocomplete-fst) shipped
# into this image link libicu for translit support. The
# query-server itself doesn't depend on libicu — operators
# building a serve-only image can fork this Dockerfile to drop
# the build binaries and the libicu runtime.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libs2-0 \
    zlib1g libbz2-1.0 libexpat1 liblz4-1 \
    libdeflate0 \
    libicu72 \
    curl ca-certificates \
    lbzip2 \
    unzip \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder-cpp /src/build/build-index /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/query-server /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/build-forward-index /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/build-postcode-lookup /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/build-gnaf-index /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/build-openaddresses-index /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/build-autocomplete-fst /usr/local/bin/
COPY --from=builder-rust /src/server/target/release/fetch-data /usr/local/bin/
COPY entrypoint.sh /usr/local/bin/

RUN chmod +x /usr/local/bin/entrypoint.sh

ENTRYPOINT ["entrypoint.sh"]
CMD ["auto"]
