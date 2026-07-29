# Sqrzl Support Certification

Sqrzl support certification names the local object-storage workflows we expect
to stay reliable, repeatable, and supportable for development and CI.

Certification is about local supportability, not production cloud parity. Sqrzl
focuses on the documented bucket/container and object/blob workflows across
S3-compatible APIs, Azure Blob Storage, Google Cloud Storage, and OCI Object
Storage.

## Source Of Truth

`compatibility-matrix.json` is the checked-in source of truth for support tiers
and operation-level status. When the matrix and prose disagree, the matrix wins.

## Support Tiers

Allowed support tiers:

- `certified`: covered by official SDK smoke tests and Sqrzl contract/interop
  tests.
- `partial`: implemented or contract-tested, but not part of the SDK
  certification gate.
- `unsupported`: intentionally not implemented.
- `deferred`: planned or under evaluation, but not supportable yet.

No workflow family is currently certified. Previous claims relied partly on
auth-disabled functional smoke tests and are demoted to `partial` while
authenticated positive and negative contracts, pagination, restart durability,
and provider error responses are remediated.

Certification requires an official SDK request using the provider's documented
authentication scheme. Auth-disabled SDK runs are functional smoke tests, not
certification evidence. GCS JSON SDK coverage remains explicitly auth-disabled;
enforced GCS authentication is covered separately by negative and
signed-request contract tests.

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
- `version`: Sqrzl package version.
- `api_port` and `ui_port`: configured listener ports.
- `auth_enforced` and `admin_auth_enforced`: current auth mode.
- `auth_enforced_providers`: provider contracts protected by configured
  credentials.
- `max_request_bytes`: current request body cap.
- `storage_ready`: whether the configured storage path is readable.
- `enabled_providers`: provider adapters compiled into this Sqrzl build
  (`s3-family`, `azure-blob`, `gcs`, `oci-object`).

For support tickets, collect:

- Sqrzl version and Git commit.
- Full `/healthz` response from the API port.
- Container image digest, if running in Docker.
- `compatibility-matrix.json` entry for the failing operation.
- SDK name and version.
- Minimal reproduction code and exact request or exception output.
- Whether the issue reproduces after restarting Sqrzl with the same `SQRZL_BLOBS_PATH`.

## SDK Certification Harness

Create a Python 3.12+ virtual environment and install the SDK test extra:

```bash
python3.12 -m venv .venv
. .venv/bin/activate
python -m pip install -e ".[sdk-tests]"
```

Run Sqrzl through the pytest harness:

```bash
python -m pytest
```

By default the harness builds and starts `target/debug/sqrzl-emulator` with
temporary storage and authentication disabled. This is functional smoke
coverage, not authenticated certification. To target an existing Sqrzl process:

```bash
SQRZL_API_URL=http://127.0.0.1:9000 python -m pytest
```

To run a subset:

```bash
SQRZL_SDK_PROVIDERS=s3,azure python -m pytest
```

The CI gate runs all SDK tests against a live Sqrzl process. The container smoke
gate builds the Docker image, verifies `/healthz`, and runs the S3 core SDK flow
against the running container.

## Request Size Boundary

Sqrzl buffers request bodies today. Configure the guardrail with:

```bash
SQRZL_MAX_REQUEST_BYTES=134217728
```

Requests above the configured limit are rejected before provider handling with
stable provider-compatible `413 Payload Too Large` responses. Streaming uploads
can be certified later, but oversized buffered uploads are not accepted by
design.

## Restart And Durability Expectations

Any workflow proposed for certification must survive a normal Sqrzl restart
when `SQRZL_BLOBS_PATH` points to the same filesystem path.

Sqrzl uses storage format v2. An empty root receives a
`.sqrzl-storage-format-v2` marker. A nonempty root without that marker is
treated as legacy storage: startup fails without modifying or deleting data.
Archive the root or clear `SQRZL_BLOBS_PATH` before restarting.

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
Sqrzl version:
Commit or image digest:
Runtime: local binary / Docker / Compose
API /healthz response:
Provider and SDK:
SDK version:
compatibility-matrix operation:
Expected behavior:
Actual behavior:
Minimal reproduction:
Does it reproduce after Sqrzl restart with the same SQRZL_BLOBS_PATH? yes/no
```
