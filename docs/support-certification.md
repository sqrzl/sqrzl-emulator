# Sqrzl support and qualification

`compatibility-matrix.json` is the source of truth for the selected local model.
Its family entries provide navigation; `operation_contracts.entries` gives each
operation, HTTP method or SMTP command, route, variant and explicit boundary.
All implemented families remain **partial**. Gmail is **deferred**.

Partial breadth does not relax the contract of an accepted request. Typed fields,
conditions, checksums, authentication, status codes and provider error envelopes
must be validated before mutation. Unsupported variants receive an explicit
provider-shaped failure. Cloud control planes, IAM/RBAC/OAuth issuance, key
management, replication, archive restoration, real mail delivery and automatic
carrier delivery are outside the local model.

## Evidence and support tiers

- `certified` requires reviewed, operation-specific evidence at an exact source
  revision and selected SDK/API versions. A passing smoke test is insufficient.
- `partial` implements a documented subset with explicit limitations and gaps.
- `unsupported` intentionally rejects the operation or variant.
- `deferred` has no current support contract.

Qualification separates positive requests, validation negatives, boundaries,
conditions, pagination, native error parsing, native authentication, normal
process restart, process interruption, and measured resource campaigns. A gate
marked `pending` is an outstanding qualification requirement. `not-applicable`
is not passing evidence. Candidate references in the matrix identify useful
tests; they do not prove every method or variant in their family.

The exact-ID validator collects Rust tests from compiled binaries and pytest
node IDs, then compares references and supplied results. Failed references or
uncollected IDs fail validation. Missing, skipped and ignored tests remain
unproven. Results must name the checked-out source SHA; all source changes must be
committed or isolated. SDK assertion scopes and authentication modes remain visible in the
acceptance artifact. The runner never promotes a support tier automatically.

Use the pinned environment and three lane commands in
[SDK qualification](sdk-qualification.md), retaining their JSON artifacts. Then
validate the combined evidence from the same clean commit:

```bash
SOURCE_SHA=$(git rev-parse HEAD)
cargo test --lib --tests --all-features > /tmp/sqrzl-rust-results.log 2>&1
python scripts/validate_contract_evidence.py \
  --rust-results /tmp/sqrzl-rust-results.log \
  --sdk-evidence target/sdk-evidence/functional.json \
  --sdk-evidence target/sdk-evidence/storage-auth.json \
  --sdk-evidence target/sdk-evidence/messaging-auth.json \
  --results-source-sha "$SOURCE_SHA" \
  --output /tmp/sqrzl-contract-acceptance.json
```

Verified SDK evidence requires an immutable executable built from that source
commit, a clean worktree and a successful lane. An external binary without a
verified build manifest or a remote smoke endpoint cannot establish exact-source
acceptance. JUnit input remains available for candidate-result inspection; it
does not qualify the manifest's scoped SDK assertions. Separate lane artifacts
retain the selected provider and authentication scope.

## Authentication lanes

The default SDK lane uses authentication-disabled storage for functional
coverage. The native storage lane configures S3 SigV4, Azure SharedKey/service
SAS, GCS XML HMAC signed URLs and OCI RSA-SHA256. It runs accepted requests and
credential negatives through official SDKs.

GCS JSON client requests can use a configured local secret bearer token. This
is a convenience mode, not native OAuth qualification. An HMAC access identifier
alone is rejected as bearer authority. Native GCS HMAC coverage uses official
signed-URL generation and is reported separately from JSON convenience tests.

The configured messaging lane exercises SendGrid tokens, Twilio credentials,
AWS SigV4 and ACS HMAC. A storage-auth pass does not establish messaging-auth
coverage. ACS signing uses an explicitly local 15-minute freshness policy; the
emulator does not claim ACS OAuth or RBAC parity.

Azure accepts service versions `2023-11-03`, `2025-01-05`, `2026-04-06`,
`2026-06-06` and `2026-10-06`. Other well-formed versions receive an unsupported
error; malformed versions are rejected. Service SAS uses HTTP localhost.
HTTPS-only, account/user-delegation SAS, signed IP constraints, stored access
policies and response overrides are rejected. Create-only SAS cannot overwrite
an existing blob, including at block-list commit.

## Request and memory boundaries

Data-plane uploads for S3, Azure BlockBlob/PutBlock, GCS media/resumable, and OCI
objects/parts spool to disk while digests are computed. Control documents and
other selected request forms remain buffered. `SQRZL_MAX_REQUEST_BYTES` limits
each HTTP request, including streamed requests. Its default is 128 MiB:

```bash
SQRZL_MAX_REQUEST_BYTES=134217728
```

An oversized or incomplete body fails before publication. Multipart/resumable
assembly uses disk-to-disk composition, so the completed object can exceed the
per-request limit. S3 additional checksums are accepted for direct PUT and
selected transactional control bodies; additional-checksum multipart/copy and
checksum trailers are explicitly unsupported. The pinned boto3 default
UploadPart checksum must be opted out for the plain multipart subset. A default
SDK request is tested separately from that opt-out workflow.

Whole materialized S3/Azure reads and copies, Azure snapshot creation and
payload-rewriting metadata operations have an explicit 64 MiB local limit.
Metadata-only HEAD/admission and bounded ranges can inspect larger objects.
Azure page extents and final append/page mutation extents are limited to 64 MiB;
native page alignment and per-update limits are checked independently. These
limits do not restrict streamed BlockBlob uploads. Other provider paths and
native cloud maximum capacities are not inferred from this local qualification.

Measured large-upload campaigns have separate payload sizes, service/client
RSS budgets, disk budgets, digest readback, restart/interruption and staging
cleanup evidence. Routine functional or SDK smoke runs do not establish those
resource bounds. See [large-upload qualification](large-upload-qualification.md).

## Ownership and durability

Production startup claims an exclusive cooperative filesystem writer lock before
opening any store or running recovery. Two active processes cannot own the same
root. Independent roots can run concurrently. Format-v2 roots without the new
bucket identity sidecar derive creation time once from the bucket name marker
and persist it. Object changes do not invent a new bucket creation timestamp.
A nonempty root without the format-v2 marker fails startup without modification.

Native storage dispatch, admin mutations and lifecycle passes share the same
operation gate across storage wrappers. Protection decisions and commits are
serialized, including cross-provider leases, holds, retention, bucket deletion,
copy and multipart completion. An embedding that calls low-level storage APIs
must hold the operation gate and root ownership itself. These are cooperative
local contracts; raw filesystem writers and arbitrary custom backends are not
qualified. See [writer ownership](storage-writer-ownership.md).

Object publication stages a generation and persists its commit decision before
replacing visible body and metadata files. Recovery rolls committed decisions
forward before indexing or reading. Predecision staged data does not become a
visible object. Invalid or ambiguous publication records fail closed. Stored
history snapshots and current-version promotion link/copy stored files without
loading the complete object into memory.

Messaging capture uses a durable batch decision across recipient records,
indexes, attachments/media and repeatability state. Matching ACS retries across
restart do not recapture; conflicts fail without replacing a prior batch.
See [messaging capture durability](messaging-capture-durability.md).

The interruption tests qualify process exits on a single-owner local filesystem.
They do not claim physical power-loss, arbitrary disk corruption, network
filesystem, multiprocess embedding or custom-backend durability. Normal restart,
process interruption and HTTP response-loss ambiguity are separate evidence.

## Diagnostics

Both listeners expose `GET /healthz`: 200 when storage is readable, 503 when it is
degraded. Collect the exact source SHA/image digest, health response, selected
operation ID, SDK/API versions, request or parsed native error, configured auth
mode, request/resource limits and same-root restart result for a reproduction.

Lifecycle scheduling, ACL/policy evaluation, requester-pays billing, SSE/KMS,
WORM administration, provider-owned history and recovery, delivery/event timing
and callback networks remain selected local models. The operation matrix lists
the accepted and rejected variants; an unsupported cloud feature is not by
itself an emulator defect.
