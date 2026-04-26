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

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake protobuf-compiler \
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

RUN apt-get update && apt-get install -y --no-install-recommends \
    libs2-0 \
    zlib1g libbz2-1.0 libexpat1 liblz4-1 \
    libdeflate0 \
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
