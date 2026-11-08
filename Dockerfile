# syntax=docker/dockerfile:1.4

# The image is the release bundle in a box: the executable, the dashboard
# frontend and the documentation — exactly as the release archive ships them.
#
#   docker run -d --name sumimi \
#     --restart unless-stopped \
#     -p 3000:3000 -p 443:443 \
#     -v sumimi-data:/data \
#     -e SUMIMI_SSL_DOMAIN=stats.example.com \
#     -e SUMIMI_SSL_EMAIL=me@example.com \
#     ghcr.io/sumimi-dev/sumimi:latest
#
# The container serves the dashboard itself (`SUMIMI_APP_URL=/app`), so it
# makes no outbound request — the promise holds inside a container too.

# ----------------------------------------------------------------- the build
FROM rust:1-bookworm AS build

WORKDIR /src

COPY . .

# The cargo caches are mounted, so a rebuild only compiles what changed: the
# dependencies and DuckDB stay in the cache between builds.
#
# The debug symbols are most of the size of a DuckDB build, and nobody needs
# them in a release.
#
# **NUM_JOBS=1** compiles one C++ file at a time. DuckDB's amalgamation is
# built with -O3, and a single one of its translation units wants a couple of
# gigabytes — with the default parallelism the compiler is killed by the OOM
# killer on a machine with less memory than that. The build is slower; it
# finishes.
ENV CARGO_PROFILE_RELEASE_DEBUG=0 \
    NUM_JOBS=1 \
    CARGO_BUILD_JOBS=2
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p sumimi --bin sumimi \
    && cp target/release/sumimi /sumimi

# --------------------------------------------------------------- the runtime
FROM debian:bookworm-slim

# ca-certificates: the ACME client talks to Let's Encrypt over TLS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The user the server runs as. The container never needs root: the instance
# directory is a volume, and everything else is read-only.
RUN useradd --system --create-home --uid 10001 sumimi

# The bundle, exactly as the release archive ships it: the executable, the
# dashboard frontend and the documentation. The server finds the frontend
# next to the executable, so they have to stay together.
WORKDIR /app
# The executable is copied out of the build stage by hand: the target
# directory is a **cache mount**, and a cache mount is not part of the image
# layer — only what the build step copied into the layer is.
COPY --from=build /sumimi /app/sumimi
COPY js /app/js
COPY service/docs /app/docs
COPY LICENSE NOTICE /app/

# The instance directory: the database, the accounts, the certificates.
#
# It is created **before** the volume instruction, and owned by the user the
# server runs as — a volume inherits the ownership of the directory it is
# mounted on, and root-owned means the non-root server cannot write a byte.
RUN mkdir -p /data && chown sumimi:sumimi /data
VOLUME ["/data"]

ENV SUMIMI_DIR=/data \
    SUMIMI_APP_URL=/app \
    SUMIMI_PORT=3000

USER sumimi
WORKDIR /data

EXPOSE 3000 443

# The health check is the server's own heartbeat — no extra software in the
# image to ask the question.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD ["/app/sumimi", "ping", "--url", "http://127.0.0.1:3000"]

ENTRYPOINT ["/app/sumimi"]
CMD ["serve"]
