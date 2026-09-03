# Multi-stage build for the Hermes Rust gateway.
#
# The gateway is self-contained (TLS roots are compiled in via webpki-roots),
# so the runtime image needs no system certs or extra libraries. To also run
# the strangler agent bridge (python -m hermes_cli.stream_turn), mount the
# hermes repo + a Python env and point HERMES_AGENT_PYTHON / HERMES_AGENT_CWD
# at them; the gateway itself runs without them (health + adapters only).

# ---- builder ----
# Pin the builder to bookworm so the binary links against the same glibc as the
# bookworm-slim runtime (a newer rust:1-slim base links glibc the runtime lacks).
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# Cache dependencies first: copy manifests, then sources.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p hermes-gateway

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
# ca-certificates is optional (roots are bundled) but harmless and helps if a
# future dep switches to the system trust store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/hermes-gateway /usr/local/bin/hermes-gateway

# Bind on all interfaces inside the container by default.
ENV HERMES_GATEWAY_BIND=0.0.0.0:8787
EXPOSE 8787

# Run as a non-root user.
RUN useradd --system --uid 10001 hermes
USER hermes

ENTRYPOINT ["/usr/local/bin/hermes-gateway"]
