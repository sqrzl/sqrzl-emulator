FROM node:26.13.1-bookworm-slim AS frontend

WORKDIR /ui

COPY ui/package.json ui/package-lock.json ./

RUN --mount=type=cache,target=/root/.npm \
  npm ci

COPY ui/ ./

RUN npm run build

FROM rust:1.100.0-slim-trixie AS backend

RUN apt-get update \
  && apt-get install -y --no-install-recommends build-essential ca-certificates pkg-config \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/sqrzl-emulator

COPY Cargo.toml Cargo.lock compatibility-matrix.json ./
COPY src/ ./src/
COPY crates/ ./crates/
COPY benches/ ./benches/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/usr/local/cargo/git \
  CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked --bin sqrzl-emulator \
  && mkdir -p /app/blobs

FROM gcr.io/distroless/cc-debian13 AS runtime

WORKDIR /app

COPY --from=backend --chown=65532:65532 /app/blobs ./blobs
COPY --from=backend /usr/src/sqrzl-emulator/target/release/sqrzl-emulator ./
COPY --from=frontend /ui/dist ./ui/dist

ENV SQRZL_BLOBS_PATH=/app/blobs \
  SQRZL_LOG_FORMAT=text

USER 65532:65532

EXPOSE 9000 9001 2525

VOLUME ["/app/blobs"]

ENTRYPOINT ["/app/sqrzl-emulator"]
