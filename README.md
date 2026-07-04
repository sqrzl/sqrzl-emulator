# Sqrzl

Sqrzl is a local object and blob storage emulator for development, CI, and
compatibility testing.

It gives you one shared filesystem-backed storage core behind S3-compatible,
Azure Blob Storage, Google Cloud Storage, and OCI Object Storage APIs, plus a
versioned admin API and an Askr admin UI for browsing buckets, navigating
folder-like blob keys, uploading and deleting blobs, viewing metadata, and
downloading content.

## Quick Start

### Local

```bash
cargo run
```

### Docker

```bash
docker compose up --build
```

Docker and Compose both default to readable text logs. Set `SQRZL_LOG_FORMAT=json`
only when you want structured tracing output.

The Compose example uses `admin` / `sqrzl-secret` credentials so the admin UI can
authenticate. The bare `docker run` example below keeps auth disabled.

If you want the bare container instead of Compose:

```bash
docker run --rm \
  -p 9000:9000 \
  -p 9001:9001 \
  -v sqrzl-blobs:/app/blobs \
  sqrzl/sqrzl-emulator
```

That container path starts with auth disabled unless you set
`SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY`.

### Native Executables

You can build a native release binary directly with Cargo:

```bash
cargo build --release --locked --bin sqrzl-emulator
```

For packaged Linux artifacts, use the GitHub Actions `Executables` workflow
and download the per-target build artifacts. The workflow runs on a single
Ubuntu runner and uses Dockerized cross-compilers to produce Linux binaries.

## What Sqrzl Covers

- S3-compatible, Azure Blob Storage, Google Cloud Storage, and OCI Object Storage
  API endpoints
- Shared filesystem-backed storage core
- Bucket/container and object/blob CRUD workflows
- Object listing, range reads, metadata, tags, and version-oriented workflows
  where supported
- Multipart, block, resumable, and provider-compatible upload flows
- Provider-compatible request signing and auth validation for supported SDK flows
- Versioned admin API and Askr admin UI for local inspection and storage
  operations
- Docker-ready local development and CI support

## Docs Map

- [Support certification](docs/support-certification.md)
- [Architecture diagrams](docs/architecture.md)
- [Storage UI guidelines](docs/sqrzl-storage-ui-guidelines.md)
- [Askr bug log](askr-bug.md)
- [UI quick start and architecture](ui/README.md)
- [UI contributor policy](ui/AGENTS.md)
- [Compatibility matrix](compatibility-matrix.json)
- [Admin API contract](public/openapi.yml)

## Configuration

Sqrzl reads all runtime configuration from environment variables.

- `SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY`: enable provider auth only when both
  values are set.
- `SQRZL_ADMIN_AUTH_DISABLED`: set to `true` to keep the admin API open for local
  development while provider auth remains enabled.
- `SQRZL_BLOBS_PATH`: filesystem storage root, defaulting to `./blobs`.
- `SQRZL_LIFECYCLE_HOURS`: hours between lifecycle rule executions, defaulting to `1`.
- `SQRZL_API_PORT`: API listener port, defaulting to `9000`.
- `SQRZL_UI_PORT`: UI listener port, defaulting to `9001`.
- `SQRZL_MAX_REQUEST_BYTES`: buffered request body cap, defaulting to 128 MiB.
  Requests above the limit fail with provider-compatible `413 Payload Too Large`
  responses.
- `SQRZL_BUCKET_LIST`: comma-delimited bucket names to create on startup.
  Existing buckets are left alone, and invalid bucket names abort startup.
- `SQRZL_LOG_FORMAT`: `text` by default for human-readable logs; set to `json`
  for structured tracing output. The Docker image and Compose file set `text`
  explicitly.

If you set `SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY`, the storage endpoints
enforce auth. The admin API at `/admin/v1` also requires auth with those same
values unless `SQRZL_ADMIN_AUTH_DISABLED=true`. The browser UI exchanges credentials
for an HttpOnly admin session cookie.

## Health And Support

Both the API and UI ports expose `GET /healthz`.

```bash
curl http://127.0.0.1:9000/healthz
curl http://127.0.0.1:9001/healthz
```

The health response reports the current status, package version, configured
listener ports, auth mode, `SQRZL_MAX_REQUEST_BYTES`, storage readiness, and the
compiled provider list. See
[Support certification](docs/support-certification.md) for the full support and
diagnostics workflow.

## SDK Certification

```bash
python3.12 -m venv .venv
. .venv/bin/activate
python -m pip install -e ".[sdk-tests]"
python -m pytest
```

To run against an existing Sqrzl process:

```bash
SQRZL_API_URL=http://127.0.0.1:9000 python -m pytest
```

The harness builds and starts `target/debug/sqrzl-emulator` by default with
temporary storage and auth disabled. Use `SQRZL_SDK_PROVIDERS=s3,azure` to run a
subset. The CI gate runs the full SDK test matrix against a live Sqrzl process,
and the container smoke gate builds the Docker image, verifies `/healthz`, and
runs the S3 core SDK flow against the container.

## Admin API

The versioned OpenAPI 3.1 contract for the admin storage API lives at
[`public/openapi.yml`](public/openapi.yml).

The contract targets the `/admin/v1` surface for session inspection, bucket
lifecycle and versioning, object browsing, binary upload/download, metadata,
tags, and version listing. It is intentionally separate from the
protocol-compatible storage endpoints.

Run `cd ui && npm run gen` after any contract change so the generated client in
`ui/src/adapters/api.g.ts` stays in sync.

## Admin UI

The Askr-based UI lives in `ui/`. It uses `@fgrzl/fetch` with the generated
client from `public/openapi.yml`; `ui/src/adapters/api.g.ts` is the only
endpoint transport surface.

```bash
cd ui
npm install
npm run gen
npm run type-check
npm test
npm run lint
npm run lint:fix
npm run fmt
npm run seed:sample
npm run build
```

Node 24 or newer is required. The console supports login/logout, bucket search,
bucket create/delete, folder-like bucket browsing, blob upload/delete, blob
details, and blob download.

Run `npm run seed:sample` after Sqrzl is running to populate `sqrzl-demo`,
`sqrzl-logs`, and `sqrzl-archive` with synthetic local objects for UI review. The
script uses the existing `/admin/v1` API, is safe to rerun, and accepts
`SQRZL_ADMIN_URL`, `SQRZL_ADMIN_USERNAME`, and `SQRZL_ADMIN_PASSWORD` overrides.

## Docker

```bash
docker build -t sqrzl/sqrzl-emulator .
docker run --rm \
  -p 9000:9000 \
  -p 9001:9001 \
  -v sqrzl-blobs:/app/blobs \
  sqrzl/sqrzl-emulator
docker compose up --build
```

The image and Compose stack default to readable text logs. Set
`SQRZL_LOG_FORMAT=json` only when you want structured tracing output.

## License

This project is licensed under the Apache License 2.0 - see the LICENSE file for
details.
