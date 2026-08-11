# Sqrzl Support Certification

Sqrzl support certification names the local storage and messaging workflows we
expect to stay reliable, repeatable, and supportable for development and CI.

Certification is about local supportability, not production cloud parity. Sqrzl
focuses on the documented bucket/container and object/blob workflows across
S3-compatible APIs, Azure Blob Storage, Google Cloud Storage, and OCI Object
Storage, plus the explicitly listed SMTP, email-provider, and text-provider
submission paths.

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

`partial` limits the breadth of an operation family; it does not permit a
different wire contract for the subset Sqrzl accepts. Every accepted request in
that subset must preserve the provider's validation, status, headers, response
shape, conditional-mutation atomicity, and documented failure semantics.
Unsupported variants must receive an explicit provider-shaped error without a
local mutation. The emulator must not report success after substituting default
fields, ignoring a condition, or normalizing one provider into another.

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

Run the currently supported authenticated SDK subset with:

```bash
SQRZL_SDK_ENFORCE_AUTH=1 SQRZL_SDK_PROVIDERS=s3,azure python -m pytest sdk-tests/test_s3_sdk.py sdk-tests/test_azure_sdk.py
```

The CI gate runs all SDK tests against a live Sqrzl process, then separately
runs authenticated S3 and Azure tests. The container smoke gate builds the
Docker image, verifies `/healthz`, and runs the complete SDK suite—including
SMTP and messaging—against the running container.

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
- Atomic GCS generation and Azure ETag create/update/delete preconditions for
  deterministic lease and compare-and-swap races.
- Atomic S3 `If-Match` PUT/DELETE and `If-None-Match: *` PUT preconditions,
  including S3-specific `404` versus `412` outcomes. A deterministic
  `conditional-request-conflict` failpoint supplies the provider-shaped `409`
  race outcome for conditional PUT or multipart completion; the in-process
  filesystem scheduler does not claim to recreate the corresponding real-cloud
  timing race organically.
- Provider-specific zero-byte request framing, including required
  `Content-Length` on Azure Put Blob and accepted GCS JSON/XML uploads, GCS's
  explicit chunked-transfer exception, mutation status codes, quoted S3/Azure
  ETags, and GCS metadata document sizing and metadata-update identity changes.
- Pre-commit GCS JSON CRC32C validation for multipart object metadata and
  `X-Goog-Hash` on media and final resumable requests, including native JSON
  `400` checksum mismatch responses, explicit rejection of unsupported hash
  tokens, server-calculated response metadata, and resumable retry after a
  failed final checksum.
- Deterministic pre-commit and post-commit HTTP failpoints for redirects,
  throttling, transient failures, timeouts, response loss, truncation, and
  pagination-token faults. The conformance matrix exercises S3, Azure Blob, GCS
  JSON, GCS XML, and OCI independently; redirect and transient status families
  are table-driven across every front door rather than inferred from one
  provider adapter.
- OCI PutObject empty-body responses with ETag, last-modified, and
  `opc-content-md5` identity headers, strong current-view reads, provider-tier
  validation, checksum rejection before mutation, and request-ID correlation.
- Durable S3 delete markers, GCS soft-delete/retention modes, and opt-in local
  Azure version retention. These provider-owned data-protection families are
  mutually exclusive per bucket/container: cross-front-door activation or
  protected mutation returns a provider-shaped `409` without changing data or
  mode metadata. Enabling S3 cannot adopt or clear a foreign protected bucket.
- Persisted Azure staged and committed block bytes, including exact
  `Committed`, `Uncommitted`, and `Latest` selection, real block sizes, and
  required `Content-Length` framing on the accepted block/append/page writes.
- Azure Locked immutability policies reject shortening, unlocking, and deletion;
  malformed legal-hold values and unsupported version-scoped policy operations
  fail before metadata or blob bytes change. Lease and WORM metadata updates use
  an in-place atomic CAS, preserving the blob's ETag, timestamp, version ID,
  bytes, and version-history cardinality.
- Azure asynchronous container deletion state, inclusive continuation markers,
  prefix-aware pagination, historical-version range reads, and atomic
  conditions on supported blob subresource mutations.
- Persisted GCS resumable upload sessions.
- Hidden provider-state directories that are excluded from bucket listings.

## Known Limitations

These are support boundaries, not bugs unless `compatibility-matrix.json` marks
the operation as `certified`.

- Lifecycle configuration can be stored and returned, but production lifecycle
  execution parity is not certified.
- ACL and policy behavior is simplified for common local workflows.
- S3 requester-pays billing, static website hosting behavior, advanced SSE key
  management, bucket-default Object Lock retention, IAM-authorized governance
  bypass, version-scoped tagging, Object Lock parameters on multipart
  initiation, and full governance/compliance control-plane parity are not
  certified; modeled unsupported variants fail before mutation.
- Azure append blob, page blob, lease, snapshot, and immutability edge cases are
  partial. Unsupported container/blob subresources return
  `501 FeatureNotSupported` before ordinary namespace or blob mutation.
- Azure container deletion uses a documented local timing control and lazy
  purge to model the deleting-name interval; it does not emulate account-wide
  service-property administration or provider garbage-collection timing.
- OCI RSA-SHA256 signature verification remains unsupported. A malformed
  Signature is rejected with the native authentication error shape and a
  syntactically complete RSA-SHA256 request is rejected explicitly rather than
  accepted through an HMAC approximation.
- OCI current-object paths are decoded exactly once and preserve empty key
  components. Version-scoped object requests, conditional multipart completion,
  and selective commits are unsupported and fail explicitly without changing
  current bytes or consuming the multipart session.
- GCS signed URL V2 validation is contract-tested, but official SDK signed URL
  generation is not in the certification gate.
- GCS historical-generation retrieval is not emulated. GCS soft-deleted bytes and
  Azure local versions are retained through the shared version store, but this
  is not full provider recovery/control-plane parity.
- GCS bucket retention is limited to unlocked policies with validated provider
  duration ranges. Retention-policy locking, per-object retention, writable
  server-owned policy fields, and disabling an enabled soft-delete policy return
  an explicit `501 UNIMPLEMENTED` response.
- Storage-provider control-plane behavior outside the named object/blob
  workflows is out of scope.
- Email support is submission-and-capture only. Each HTTP front door accepts
  only the fields listed in the matrix: SendGrid v3 personalizations/content/
  attachments, SES v2 `Content.Simple`, and ACS Email recipients/content/
  attachments/headers/reply/tracking. Unsupported variants are rejected before
  fan-out. ACS repeatability and caller operation IDs are durable; mail fan-out
  is all-or-nothing across recipient mailboxes. Domain/sender verification,
  reputation, suppression lists, unlisted templates, remote delivery, and
  provider event systems remain out of scope.
- Text support is submission-and-simulation only. The accepted outbound subsets
  are listed in the matrix: Twilio's core SMS/MMS-reference form fields, direct
  SNS PhoneNumber Publish, AWS SMS Voice v2 SendTextMessage/SendMediaMessage,
  and ACS SMS one-to-many sends with the documented local options. Unknown
  fields are rejected unless the real provider explicitly ignores them (for
  example, irrelevant SNS structured-message protocol keys). Number
  registration, carrier/compliance policy, billing, automatic delivery timing,
  and provider management APIs remain outside scope.

## Qualification boundary

Sqrzl qualification is protocol evidence, not full production-provider
qualification. A client conformance run should cover non-empty and zero-byte
PUT/GET/HEAD/DELETE, exact mutation statuses, conditional create/update/delete,
metadata size and identity tokens, pagination, missing objects, redirect
rejection, committed-but-response-lost ambiguity, and restart/cache-loss
recovery. Sqrzl exercises raw WAL-, SST-, and catalog-shaped objects through all
five storage front doors: it reopens the filesystem backend, verifies bytes,
publishes a replacement catalog, and deletes the retired catalog and WAL while
the SST and replacement catalog remain. This is protocol CRUD/durability
evidence only. Storage engines must still use those front doors to prove WAL
replay, SST coverage, catalog interpretation/retirement, and safe remote-WAL
deletion; those engine-specific assertions belong to the client suite, not
Sqrzl.

Real-cloud tests remain required for IAM and workload identity, DNS/TLS,
quotas, service availability, provider policy configuration, verified email
identities/domains, registered sending numbers, carrier filtering, and messaging
compliance controls.

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
