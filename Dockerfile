# Build UI
FROM node:slim AS frontend

RUN apt-get update \
  && apt-get install -y --no-install-recommends bash ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://vite.plus | bash

ENV PATH="/root/.vite-plus/bin:${PATH}"

WORKDIR /ui

COPY ui/package.json ui/package-lock.json* ./

RUN --mount=type=cache,target=/root/.npm \
  npm ci

COPY ui/ ./

RUN npm run build

FROM rust:slim AS backend

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/sqrzl-emulator

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/usr/local/cargo/git \
  cargo build --release --locked

COPY --from=frontend /ui/dist /usr/src/sqrzl-emulator/ui/dist/

RUN strip target/release/sqrzl-emulator || true

FROM debian:trixie-slim AS runtime-fs

RUN mkdir -p /app/blobs \
  && chown 65532:65532 /app/blobs

FROM gcr.io/distroless/cc-debian13 AS runtime

WORKDIR /app

COPY --from=runtime-fs --chown=65532:65532 /app/blobs /app/blobs
COPY --from=backend /usr/src/sqrzl-emulator/target/release/sqrzl-emulator /app/sqrzl-emulator
COPY --from=frontend /ui/dist /app/ui/dist

ENV SQRZL_BLOBS_PATH=/app/blobs \
  SQRZL_LOG_FORMAT=text

USER 65532:65532

EXPOSE 9000 9001

VOLUME ["/app/blobs"]

ENTRYPOINT ["/app/sqrzl-emulator"]
