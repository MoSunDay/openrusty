# openrusty gateway image: multi-stage build -> slim runtime.
#
# Stage 1 builds the workspace binary (NOT the plugins/ tree: that is a
# separate workspace of wasm modules, compiled per-plugin by
# scripts/build-plugins.sh and shipped via a plugins volume).
# Stage 2 is the runtime surface the sidecar actually needs: iptables +
# kmod (iptables-init rule programming and the modprobe probe) and
# ca-certificates (upstream TLS roots), running as the proxy UID 511.

FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --locked -p openrusty-server

FROM debian:bookworm-slim
# iptables|iptables-nft|iptables-legacy + modprobe (best-effort module
# load), system CA roots for upstream TLS; nothing else.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        iptables kmod ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# System user matching the sidecar UID convention (proxy-uid default 511).
RUN groupadd --system --gid 511 openrusty \
    && useradd --system --uid 511 --gid 511 --no-create-home openrusty
COPY --from=build /src/target/release/openrusty /usr/local/bin/openrusty
USER 511
ENTRYPOINT ["/usr/local/bin/openrusty"]
