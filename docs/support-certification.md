# Peas Support Certification

Peas support certification names the local object-storage workflows we expect
to stay reliable, repeatable, and supportable for development and CI.

Certification is about local supportability, not production cloud parity. Peas
focuses on the documented bucket/container and object/blob workflows across
S3-compatible APIs, Azure Blob Storage, Google Cloud Storage, and OCI Object
Storage.

## Source Of Truth

`compatibility-matrix.json` is the checked-in source of truth for support tiers
and operation-level status. When the matrix and prose disagree, the matrix wins.

## Support Tiers

Allowed support tiers:

- `certified`: covered by official SDK smoke tests and Peas contract/interop
  tests.
- `partial`: implemented or contract-tested, but not part of the SDK
  certification gate.
- `unsupported`: intentionally not implemented.
- `deferred`: planned or under evaluation, but not supportable yet.

Certified workflow families include:

- S3-compatible APIs: bucket CRUD, object put/get/head/delete/list/range,
  metadata, SigV4 SDK requests, multipart upload, and versioning.
- Azure Blob Storage: container CRUD, blob upload/download/properties/delete/list/range,
  metadata, Shared Key-shaped SDK requests, and block blob staging/commit.
- Google Cloud Storage: JSON API bucket/object CRUD, object metadata/list/range/media
  download, and resumable uploads.
- OCI Object Storage: namespace discovery, bucket CRUD, object put/get/head/delete/list/range,
  metadata, request signing-shaped SDK requests, and multipart upload.

## Health And Diagnostics

Both the API and UI ports expose:

```text
GET /healthz
```

The response is JSON. When storage is healthy, the handler returns `200 OK`; if
storage cannot be read, it returns `503 Service Unavailable` with
`status: degraded`.

The response includes:

- `status`: `ok` or `degraded`.
- `version`: Peas package version.
- `api_port` and `ui_port`: configured listener ports.
- `auth_enforced` and `admin_auth_enforced`: current auth mode.
- `max_request_bytes`: current request body cap.
- `storage_ready`: whether the configured storage path is readable.
- `enabled_providers`: provider adapters compiled into this Peas build
  (`s3-family`, `azure-blob`, `gcs`, `oci-object`).

For support tickets, collect:

- Peas version and Git commit.
- Full `/healthz` response from the API port.
- Container image digest, if running in Docker.
- `compatibility-matrix.json` entry for the failing operation.
- SDK name and version.
- Minimal reproduction code and exact request or exception output.
- Whether the issue reproduces after restarting Peas with the same `BLOBS_PATH`.

## SDK Certification Harness

Create a Python 3.12+ virtual environment and install the SDK test extra:

```bash
python3.12 -m venv .venv
. .venv/bin/activate
python -m pip install -e ".[sdk-tests]"
```

Run Peas through the pytest harness:

```bash
python -m pytest
```

By default the harness builds and starts `target/debug/peas-emulator` with
temporary storage and authentication disabled. To target an existing Peas
process:

```bash
PEAS_API_URL=http://127.0.0.1:9000 python -m pytest
```

To run a subset:

```bash
PEAS_SDK_PROVIDERS=s3,azure python -m pytest
```

The CI gate runs all SDK tests against a live Peas process. The container smoke
gate builds the Docker image, verifies `/healthz`, and runs the S3 core SDK flow
against the running container.

## Request Size Boundary

Peas buffers request bodies today. Configure the guardrail with:

```bash
MAX_REQUEST_BYTES=134217728
```

Requests above the configured limit are rejected before provider handling with
stable provider-compatible `413 Payload Too Large` responses. Streaming uploads
can be certified later, but oversized buffered uploads are not accepted by
design.

## Restart And Durability Expectations

Certified workflows must survive a normal Peas restart when `BLOBS_PATH` points
to the same filesystem path.

Durability hardening covers:

- Atomic temp-file-then-rename writes for object data, object metadata, bucket
  metadata, upload records, and provider sidecars.
- Per-object write coordination for same-object mutations.
- Persisted Azure staged block state and committed block lists.
- Persisted GCS resumable upload sessions.
- Hidden provider-state directories that are excluded from bucket listings.

## Known Limitations

These are support boundaries, not bugs unless `compatibility-matrix.json` marks
the operation as `certified`.

- Lifecycle configuration can be stored and returned, but production lifecycle
  execution parity is not certified.
- ACL and policy behavior is simplified for common local workflows.
- S3 requester-pays billing, static website hosting behavior, advanced SSE key
  management, and full object-lock governance/compliance parity are not
  certified.
- Azure append blob, page blob, lease, snapshot, and immutability edge cases are
  partial.
- GCS signed URL V2 validation is contract-tested, but official SDK signed URL
  generation is not in the certification gate.
- Provider control-plane behavior outside object/blob storage workflows is out
  of scope.

## Reproducible Issue Template

```text
Peas version:
Commit or image digest:
Runtime: local binary / Docker / Compose
API /healthz response:
Provider and SDK:
SDK version:
compatibility-matrix operation:
Expected behavior:
Actual behavior:
Minimal reproduction:
Does it reproduce after Peas restart with the same BLOBS_PATH? yes/no
```
