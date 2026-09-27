# syntax=docker/dockerfile:1
# CapsCode gateway image .
#
# The only privileged container: holds the WireGuard keys, the nftables kill
# switch, the real provider keys (injected into tmpfs at run time), and the secret broker.
# Runs NO user/AI code. Capsule shares this container's network namespace.
#
# Multi-stage: a Rust builder compiles the `capscode-broker` binary from the
# workspace, then a slim Debian runtime carries the network stack + that binary.
#
# Build context MUST be the repository ROOT (the builder needs the workspace
# sources); point -f at this file:
#   docker build -f images/gateway.Dockerfile -t capscode/gateway:dev .

# ---- builder: compile capscode-broker ---------------------------------------
# Debian-bookworm-based Rust toolchain so the produced glibc binary runs directly
# on the debian:bookworm-slim runtime stage below (no musl cross-build needed).
FROM rust:1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder

# aws-lc-rs (the rustls default crypto provider used by the broker) builds its
# vendored libcrypto with cmake + a C toolchain. gcc ships in the rust image;
# add cmake.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# Copy the workspace manifests + Cargo.lock first, then the sources. Only
# capscode-broker and its dependency graph are compiled (crates/cli et al. are
# not dependencies of the broker, so they are never built here).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# capscode-core embeds providers/registry.yaml via include_str!, so it must be in
# the build context (it lives outside crates/).
COPY providers ./providers
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked -p capscode-broker --release --bin capscode-broker \
 && cp /build/target/release/capscode-broker /usr/local/bin/capscode-broker

# ---- runtime: the gateway image ---------------------------------------------
# debian:bookworm-slim pinned BY DIGEST (see images/toolchain.lock.toml [base]).
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

# Userspace WireGuard + nft kill switch + routing helpers. openresolv provides
# `resolvconf`, which wg-quick invokes for the config's DNS= line; procps provides
# `sysctl`, which wg-quick needs for `net.ipv4.conf.all.src_valid_mark=1` (without
# it wg-quick aborts and the tunnel never comes up). No recommends.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
 && apt-get install -y --no-install-recommends \
      wireguard-tools \
      wireguard-go \
      nftables \
      iproute2 \
      netcat-openbsd \
      curl \
      procps \
      openresolv \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# wg-quick sets net.ipv4.conf.all.src_valid_mark=1, but that key is not writable in
# a non-init network namespace on some hosts (e.g. OrbStack), where wg-quick treats
# the failed write as fatal and tears the tunnel down. Shim `sysctl` (first on PATH)
# so the write is attempted — and applied where the host allows it — but never
# fatal; the setting only relaxes reverse-path filtering, which is not enforced in
# this netns (verified: the ProtonVPN handshake and transfer succeed without it).
RUN printf '#!/bin/sh\nif /usr/sbin/sysctl "$@"; then exit 0; fi\necho "capscode: sysctl $* not applied (non-fatal)" >&2\nexit 0\n' > /usr/local/sbin/sysctl \
 && chmod 0755 /usr/local/sbin/sysctl

# Committed reference ruleset (fallback only; the per-session ruleset arrives via
# CAPSCODE_KILLSWITCH_NFT and is what actually gets applied).
COPY images/killswitch.nft /etc/capscode/killswitch.nft

# The compiled secret broker from the builder stage.
COPY --from=builder /usr/local/bin/capscode-broker /usr/local/bin/capscode-broker

COPY images/gateway-entrypoint.sh /usr/local/bin/capscode-gateway-entrypoint
RUN chmod 0755 /usr/local/bin/capscode-gateway-entrypoint /usr/local/bin/capscode-broker

# No USER drop: this container legitimately needs NET_ADMIN to load the ruleset
# and bring up the tunnel. It runs no untrusted code. It does NOT setuid anything.
ENTRYPOINT ["/usr/local/bin/capscode-gateway-entrypoint"]
