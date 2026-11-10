# syntax=docker/dockerfile:1.7
# assessment-engine: the gRPC server (`assessment.v1`, health, reflection).
# Build: docker build -t assessment-engine .
# Run:   docker run --rm -p 50061:50061 assessment-engine

ARG RUST_IMAGE=rust:1.99-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# ---------------------------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS builder
ENV CARGO_TERM_COLOR=never \
    CARGO_NET_RETRY=5
WORKDIR /src

# Everything except target/ (see .dockerignore). rust-toolchain.toml is excluded too: the image
# tag pins the toolchain, so the build never downloads another one. The protos compile with
# protox, so no protoc is needed.
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p assessment-server --bin assessment-engine \
 && install -Dm755 target/release/assessment-engine /out/assessment-engine

# ---------------------------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
LABEL org.opencontainers.image.title="assessment-engine" \
      org.opencontainers.image.description="Change-impact assessment and SemVer for versioned work items, over gRPC" \
      org.opencontainers.image.source="https://github.com/f8fvwgzc/assessment-engine" \
      org.opencontainers.image.licenses="MIT"

# ca-certificates: the optional judge calls its API over TLS.
# Debian security updates move package versions; the base image tag is the pin.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 assessment \
 && useradd --system --uid 10001 --gid assessment --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin assessment \
 && install -d -o assessment -g assessment -m 0750 /data

COPY --from=builder /out/assessment-engine /usr/local/bin/assessment-engine

# Stateless by default: `Assess` needs no storage. Set ASSESSMENT_JOURNAL=/data/registry.jsonl
# (and mount /data) to keep the registry of the stateful calls (PutEntity, …) across restarts.
#
# The judge is off by default. To turn it on, set ASSESSMENT_JUDGE=on (or auto) and give the API
# key at run time as a secret: mount it as a file and set ANTHROPIC_API_KEY_FILE to its path, for
# example /run/secrets/anthropic_api_key. Never put a key into the image, a build argument or an
# ENV line here. A server with a judge also needs ASSESSMENT_AUTH_TOKEN, because it listens on
# 0.0.0.0.
ENV ASSESSMENT_ADDR=0.0.0.0:50061 \
    ASSESSMENT_LOG_FORMAT=json

USER 10001:10001
WORKDIR /data
EXPOSE 50061

HEALTHCHECK --interval=15s --timeout=5s --start-period=10s --retries=3 \
  CMD ["assessment-engine", "health"]

ENTRYPOINT ["tini", "--", "assessment-engine"]
CMD ["serve"]
