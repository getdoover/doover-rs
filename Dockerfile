# syntax=docker/dockerfile:1
#
# doover-cli -- multi-arch static-musl binary build.
# Mirrors ../doover-device-agent/Dockerfile: ONE native rustc on $BUILDPLATFORM
# cross-compiles every arch via cargo-zigbuild (zig = cross C compiler/linker).
# No QEMU, no per-arch base image -- only the Rust target triple changes.
#
# This produces a BINARY, not an image: the final stage is `FROM scratch` and is
# meant to be exported with --output. debian/rules packages the result.
#
#   docker buildx build --platform linux/arm64 --target bin \
#     --output type=local,dest=debian/prebuilt/arm64 .
#
# A static musl binary shares only the host KERNEL, so one arm64 build runs on
# the CM4 Doovits (Debian 12) and on far older userlands alike.

ARG ZIG_VERSION=0.13.0

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG ZIG_VERSION
ARG TARGETPLATFORM
ARG TARGETARCH
ARG TARGETVARIANT

RUN apt-get update && apt-get install -y --no-install-recommends \
        xz-utils curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# NB: no system protoc -- doover-proto/build.rs falls back to its vendored
# protoc when PROTOC is unset (a glibc binary, runs natively on this builder).
RUN set -eux; \
    case "$(uname -m)" in \
        aarch64) ZARCH=aarch64 ;; \
        x86_64)  ZARCH=x86_64  ;; \
        *) echo "unsupported build arch $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${ZARCH}-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz; \
    mkdir -p /opt/zig; tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    zig version
RUN cargo install cargo-zigbuild --locked

RUN set -eux; \
    case "$TARGETPLATFORM" in \
        linux/amd64)  TRIPLE=x86_64-unknown-linux-musl      ;; \
        linux/arm64)  TRIPLE=aarch64-unknown-linux-musl     ;; \
        linux/arm/v7) TRIPLE=armv7-unknown-linux-musleabihf ;; \
        linux/arm/v6) TRIPLE=arm-unknown-linux-musleabihf   ;; \
        linux/386)    TRIPLE=i686-unknown-linux-musl        ;; \
        *) echo "unsupported target platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    echo "$TRIPLE" > /tmp/triple; \
    rustup target add "$TRIPLE"

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY doover-proto/ ./doover-proto/
COPY doover/ ./doover/
COPY doover-macros/ ./doover-macros/
COPY doover-cli/ ./doover-cli/

# Per-arch cache ids so concurrent multi-platform builds don't race on the
# crate registry (see the device-agent Dockerfile for the .cargo-ok rationale).
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=dcli-registry-${TARGETARCH}${TARGETVARIANT} \
    --mount=type=cache,target=/build/target,id=dcli-target-${TARGETARCH}${TARGETVARIANT} \
    set -eux; \
    TRIPLE="$(cat /tmp/triple)"; \
    cargo zigbuild --release --target "$TRIPLE" -p doover-cli; \
    cp "target/${TRIPLE}/release/doover" /doover

# Binary-only export stage: `--target bin --output type=local` drops the
# binary on the host with no image built. The name here is what
# debian/rules looks for under debian/prebuilt/$DEB_HOST_ARCH/.
FROM scratch AS bin
COPY --from=builder /doover /doover
