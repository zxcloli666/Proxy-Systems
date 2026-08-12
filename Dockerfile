# syntax=docker/dockerfile:1.7
#
# Unified multi-target Dockerfile. Pick which binary to build via:
#   docker build --target simple-proxy -t simple-proxy .
#   docker build --target simple-ipv6-proxy -t simple-ipv6-proxy .
#   docker build --target intermediate-proxy -t intermediate-proxy .
#   docker build --target tor-proxy -t tor-proxy .
#
# Two builders on purpose:
#   `builder`        — alpine/musl, builds intermediate-proxy and tor-proxy.
#   `builder-simple` — debian/glibc, builds the two proxies that carry wreq
#                      impersonation. wreq pulls BoringSSL, whose bindgen step
#                      needs to dlopen libclang — impossible in a static musl
#                      build.

FROM rust:1-alpine AS builder
# gcc: mlua `vendored` compiles Lua 5.4 C sources via the cc crate.
RUN apk add --no-cache musl-dev cmake make perl gcc
WORKDIR /build

# === Layer 1: dependency cache ===
# Copy only manifests, build with stub sources. This layer's cache key is
# (Cargo.toml + Cargo.lock + member manifests) — invalidates only when
# dependencies change, not on every code edit.
COPY Cargo.toml Cargo.lock ./
COPY proxy-common/Cargo.toml proxy-common/Cargo.toml
COPY simple-proxy/Cargo.toml simple-proxy/Cargo.toml
COPY simple-ipv6-proxy/Cargo.toml simple-ipv6-proxy/Cargo.toml
COPY intermediate-proxy/Cargo.toml intermediate-proxy/Cargo.toml
COPY tor-proxy/Cargo.toml tor-proxy/Cargo.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    mkdir -p proxy-common/src simple-proxy/src simple-ipv6-proxy/src intermediate-proxy/src tor-proxy/src \
    && echo "fn main(){}" > simple-proxy/src/main.rs \
    && echo "fn main(){}" > simple-ipv6-proxy/src/main.rs \
    && echo "fn main(){}" > intermediate-proxy/src/main.rs \
    && echo "fn main(){}" > tor-proxy/src/main.rs \
    && touch proxy-common/src/lib.rs \
    && cargo build --release --bins --workspace --exclude simple-proxy --exclude simple-ipv6-proxy || true

# === Layer 2: real source build ===
# Copy actual source. `target/` from Layer 1 is preserved in the image
# filesystem (not in cache mount) so cargo only rebuilds workspace crates,
# not their dependencies.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    touch proxy-common/src/lib.rs \
          intermediate-proxy/src/main.rs \
          tor-proxy/src/main.rs \
    && cargo build --release --bins --workspace --exclude simple-proxy --exclude simple-ipv6-proxy \
    && mkdir -p /out \
    && cp target/release/intermediate-proxy \
          target/release/tor-proxy \
          /out/

FROM rust:1-bookworm AS builder-simple
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        cmake make perl git golang-go clang libclang-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY proxy-common/Cargo.toml proxy-common/Cargo.toml
COPY simple-proxy/Cargo.toml simple-proxy/Cargo.toml
COPY simple-ipv6-proxy/Cargo.toml simple-ipv6-proxy/Cargo.toml
COPY intermediate-proxy/Cargo.toml intermediate-proxy/Cargo.toml
COPY tor-proxy/Cargo.toml tor-proxy/Cargo.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    mkdir -p proxy-common/src simple-proxy/src simple-ipv6-proxy/src intermediate-proxy/src tor-proxy/src \
    && echo "fn main(){}" > simple-proxy/src/main.rs \
    && echo "fn main(){}" > simple-ipv6-proxy/src/main.rs \
    && echo "fn main(){}" > intermediate-proxy/src/main.rs \
    && echo "fn main(){}" > tor-proxy/src/main.rs \
    && touch proxy-common/src/lib.rs \
    && cargo build --release -p simple-proxy -p simple-ipv6-proxy || true

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    touch proxy-common/src/lib.rs simple-proxy/src/main.rs simple-ipv6-proxy/src/main.rs \
    && cargo build --release -p simple-proxy -p simple-ipv6-proxy \
    && mkdir -p /out \
    && cp target/release/simple-proxy target/release/simple-ipv6-proxy /out/

# === Runtime images ===

FROM debian:12-slim AS simple-proxy
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder-simple /out/simple-proxy /usr/local/bin/
CMD ["simple-proxy"]

FROM debian:12-slim AS simple-ipv6-proxy
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder-simple /out/simple-ipv6-proxy /usr/local/bin/
CMD ["simple-ipv6-proxy"]

FROM alpine:3.21 AS intermediate-proxy
RUN apk add --no-cache ca-certificates && mkdir -p /var/cache/acme
COPY --from=builder /out/intermediate-proxy /usr/local/bin/
VOLUME ["/var/cache/acme"]
CMD ["intermediate-proxy"]

FROM alpine:3.21 AS tor-proxy
COPY --from=builder /out/tor-proxy /usr/local/bin/
CMD ["tor-proxy"]
