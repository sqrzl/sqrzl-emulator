# Sqrzl Emulator

Sqrzl is a Docker-ready object and blob storage emulator for local development
and CI. Persistent filesystem-backed stores are available through
S3-compatible, Azure Blob Storage, Google Cloud Storage, and OCI Object Storage
HTTP APIs, outbound email capture, and bidirectional SMS/MMS provider adapters.
A browser UI is included for inspecting buckets, mailboxes, and texts.

Sqrzl is a development tool, not a production storage service. Provider
compatibility is intentionally scoped; see the
[compatibility matrix](compatibility-matrix.json) for current coverage.
Within that documented scope, Sqrzl treats the provider's HTTP or SMTP wire
contract as authoritative: request validation, mutation atomicity, status
codes, headers, response documents, identity tokens, and failure behavior are
provider-specific. An unsupported request variant must be rejected; it must not
be accepted through a normalized approximation or silently ignored.

## Start with Docker Compose

The repository includes a working [`compose.yml`](compose.yml):

```bash
git clone https://github.com/sqrzl/sqrzl-emulator.git
cd sqrzl-emulator
docker compose up --build
```

When the health check succeeds, open:

| Surface | URL | Purpose |
| --- | --- | --- |
| Storage APIs | <http://localhost:9000> | S3, Azure, GCS, and OCI endpoints |
| Admin UI | <http://localhost:9001> | Browse and manage local storage |
| API health | <http://localhost:9000/healthz> | Runtime and storage readiness |
| UI health | <http://localhost:9001/healthz> | UI listener readiness |

The checked-in Compose configuration enables authentication with:

```text
username/access key: admin
password/secret key: sqrzl-secret
```

Use those values on the UI login page. To run an open local instance, remove
`SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY` from `compose.yml`; both must
be absent. Setting only one also leaves authentication disabled.

Stop the container while retaining data:

```bash
docker compose down
```

Stop it and permanently delete the named storage volume:

```bash
docker compose down --volumes
```

## Copy-paste Compose setup

This minimal configuration keeps authentication disabled so provider SDKs can
be connected with the least setup:

```yaml
services:
  sqrzl:
    build: .
    container_name: sqrzl
    ports:
      - "9000:9000"
      - "9001:9001"
    environment:
      SQRZL_BLOBS_PATH: /app/blobs
      SQRZL_LOG_FORMAT: text
    volumes:
      - sqrzl-blobs:/app/blobs

volumes:
  sqrzl-blobs:
```

To protect the storage APIs and require UI login, add both credentials:

```yaml
    environment:
      SQRZL_BLOBS_PATH: /app/blobs
      SQRZL_LOG_FORMAT: text
      SQRZL_ACCESS_KEY_ID: admin
      SQRZL_SECRET_ACCESS_KEY: change-this-local-secret
```

To keep the UI and `/admin/v1` open while provider API authentication remains
enabled, also set:

```yaml
      SQRZL_ADMIN_AUTH_DISABLED: "true"
```

## Run the container directly

Build the image from this repository:

```bash
docker build -t sqrzl-emulator:local .
```

Run it with a persistent named volume and authentication disabled:

```bash
docker run --rm \
  --name sqrzl \
  -p 9000:9000 \
  -p 9001:9001 \
  -v sqrzl-blobs:/app/blobs \
  sqrzl-emulator:local
```

Pass configuration with repeated `--env` flags or an env file:

```bash
docker run --rm \
  --name sqrzl \
  -p 9000:9000 \
  -p 9001:9001 \
  -v sqrzl-blobs:/app/blobs \
  --env SQRZL_ACCESS_KEY_ID=admin \
  --env SQRZL_SECRET_ACCESS_KEY=change-this-local-secret \
  --env SQRZL_BUCKET_LIST=uploads,fixtures \
  sqrzl-emulator:local
```

## Environment variables

The table below is the complete runtime configuration surface.

| Variable | Accepted values | Default | Description |
| --- | --- | --- | --- |
| `SQRZL_ACCESS_KEY_ID` | Any string; set together with `SQRZL_SECRET_ACCESS_KEY` | Unset | Access key and admin username. Provider and admin authentication are enabled only when both credential variables are present. |
| `SQRZL_SECRET_ACCESS_KEY` | Any string; set together with `SQRZL_ACCESS_KEY_ID` | Unset | Signing secret and admin password. Azure Shared Key treats valid Base64 as decoded key bytes; otherwise the literal bytes are used. |
| `SQRZL_ADMIN_AUTH_DISABLED` | `1`, `true`, `yes`, or `on` (case-insensitive) enable it; every other value is false | `false` | Keeps the UI and `/admin/v1` in open mode while provider API authentication remains enabled. |
| `SQRZL_BLOBS_PATH` | Writable container path | `/app/blobs` in Docker; `./blobs` natively | Storage format v2 root. Mount a volume here for persistence. |
| `SQRZL_LIFECYCLE_HOURS` | Unsigned integer hours | `1` | Interval between lifecycle-rule passes. Invalid values use the default. Avoid `0`, which requests a continuous interval. |
| `SQRZL_API_PORT` | Unsigned 16-bit port (`0`–`65535`) | `9000` | Storage API listener inside the container. Normally keep this at `9000` and change only the host side of the Docker port mapping. |
| `SQRZL_UI_PORT` | Unsigned 16-bit port (`0`–`65535`) | `9001` | Admin UI listener inside the container. Normally keep this at `9001`. |
| `SQRZL_MAX_REQUEST_BYTES` | Positive integer byte count | `134217728` (128 MiB) | Maximum buffered HTTP request body or SMTP `DATA` payload. Oversized provider requests receive a provider-shaped `413`; SMTP receives `552`. Zero and invalid values use the default. |
| `SQRZL_BUCKET_LIST` | Comma-separated bucket names | Empty | Buckets created at startup. Whitespace and empty entries are ignored. Names use the Amazon S3 general-purpose rules: 3–63 lowercase letters, digits, periods, or hyphens; an alphanumeric first and last character; no adjacent periods, IP-address form, or AWS-reserved affix. |
| `SQRZL_LOG_FORMAT` | `text` or `json` (case-insensitive) | `text` | Log output format. Unknown values fall back to `text`. |
| `SQRZL_SMTP_PORT` | Unsigned 16-bit port | `2525` | SMTP capture listener. This is the only extra listener used by the mail domain. |
| `SQRZL_SENDGRID_API_KEY` | SendGrid API key | Unset | Enables SendGrid bearer authentication when set. |
| `SQRZL_TWILIO_ACCOUNT_SID` | Twilio account SID; set with `SQRZL_TWILIO_AUTH_TOKEN` | Unset | Enables Twilio Basic authentication only when both Twilio values are present. |
| `SQRZL_TWILIO_AUTH_TOKEN` | Twilio auth token; set with `SQRZL_TWILIO_ACCOUNT_SID` | Unset | Twilio Basic authentication secret and callback-signing key. |
| `SQRZL_ACS_CONNECTION_STRING` | `endpoint=<url>;accesskey=<base64>` | Unset | Enables ACS email/SMS HMAC authentication. |
| `SQRZL_TEXT_CALLBACK_ALLOWED_HOSTS` | Comma-separated hostnames or IP addresses | Loopback hosts | Adds callback hosts to `localhost`, `127.0.0.1`, and `::1`. Redirects remain disabled. |
| `SQRZL_TEXT_CALLBACK_TIMEOUT_MS` | Positive integer milliseconds | `5000` | Timeout for each inbound or delivery callback attempt. |

Docker port mappings are `HOST:CONTAINER`. For example, to expose Sqrzl on
host ports 19000 and 19001 without changing its internal configuration:

```yaml
ports:
  - "19000:9000"
  - "19001:9001"
```

## Persistence and storage format v2

Mount `/app/blobs` to a named volume or bind mount. Without a mount, data is
lost when the container is removed.

An empty root is initialized with `.sqrzl-storage-format-v2`. Sqrzl refuses to
start when the configured root is nonempty but lacks that marker. This prevents
an older on-disk layout from being silently misread. Sqrzl never migrates or
deletes legacy data automatically.

For a disposable named volume, reset with:

```bash
docker compose down --volumes
docker compose up --build
```

For a bind mount, archive or clear the host directory yourself before
restarting. Never delete a directory containing data you intend to keep.

## Connect provider clients

All provider APIs use the storage listener, normally
`http://localhost:9000`.

| Provider | Endpoint/client setting | Local notes |
| --- | --- | --- |
| S3-compatible | Endpoint URL `http://localhost:9000` | Use region `us-east-1`, SigV4, and path-style addressing. |
| Azure Blob | Account URL `http://localhost:9000/devstoreaccount1` | The account is the first path segment. For the simplest smoke setup, use no credential and leave Sqrzl auth disabled. |
| GCS JSON API | API endpoint `http://localhost:9000` | Use anonymous credentials when auth is disabled. When enabled, bearer tokens equal to either configured Sqrzl credential are accepted for local qualification. |
| GCS XML API | `http://localhost:9000/<bucket>/<object>` | Send `Host: storage.googleapis.com` when making raw XML API requests. |
| OCI Object Storage | Client endpoint `http://localhost:9000` | OCI paths use `/n/<namespace>/b/<bucket>/...`; the default namespace response is `sqrzl-emulator`. Use auth-disabled mode: OCI RSA-SHA256 verification is explicitly unsupported and non-provider signature approximations are rejected. |
| SMTP | SMTP server `localhost:2525` | Plaintext local capture with strict envelope paths and null reverse-path support; SMTP AUTH and STARTTLS are intentionally unsupported. |
| SendGrid Mail Send | API base `http://localhost:9000` | Supports the matrix-listed v3 personalizations/content/attachment subset at `POST /v3/mail/send` and optional `SQRZL_SENDGRID_API_KEY` bearer authentication. |
| Amazon SES v2 | Endpoint URL `http://localhost:9000` | Supports the matrix-listed `Content.Simple` subset through `POST /v2/email/outbound-emails` with SigV4; Raw, Template, and attachments are rejected. |
| ACS Email | Connection-string endpoint `http://localhost:9000` | Supports the matrix-listed `POST /emails:send` subset, operation polling, caller operation IDs, repeatability, and ACS HMAC authentication. |
| Twilio Messages | API base `http://localhost:9000` | Supports outbound SMS/MMS references plus admin-driven inbound and delivery simulation. |
| Amazon SNS | Endpoint URL `http://localhost:9000` | Supports direct `PhoneNumber` SMS `Publish` only. |
| AWS SMS Voice v2 | Endpoint URL `http://localhost:9000` | Supports `SendTextMessage` and `SendMediaMessage`. |
| ACS SMS | Connection-string endpoint `http://localhost:9000` | Supports one-to-many SMS and admin-driven Event Grid callbacks. |

For lease and compare-and-swap qualification, the GCS JSON endpoint supports
`ifGenerationMatch` (including create-only value `0`) on uploads and conditional
object mutations. Azure Blob writes and deletes enforce `If-Match` and
`If-None-Match` with quoted ETags or `*`. S3 PUT supports `If-Match` and the
required `If-None-Match: *` form; S3 DELETE supports `If-Match` (the S3 API does
not define `If-None-Match` for DeleteObject). These checks are atomic per object, missing resources use
provider-shaped `404` responses, and returned GCS generations and quoted Azure
ETags can be fed back into later conditional requests. Conditional operations
target the current object; historical GCS generation retrieval and the full
HTTP conditional-header matrix remain outside this local emulator contract.

## Protocol conformance and deterministic failures

Sqrzl keeps mutation responses provider-specific: S3 object PUT/DELETE use
`200`/`204`, Azure Put/Delete Blob use `201`/`202`, and GCS uploads/deletes use
`200`/`204`. S3 zero-byte object PUTs require an explicit `Content-Length: 0`;
missing framing returns S3-shaped `411 MissingContentLength`. Azure Put Blob,
Put Block, Put Block List, Append Block, and Put Page requests and accepted GCS
JSON/XML upload paths also require `Content-Length`; GCS accepts
`Transfer-Encoding: chunked` instead, while missing framing returns that
surface's `411` error shape. Azure committed and uncommitted block bytes remain
distinct, including `Committed`, `Uncommitted`, and `Latest` block-list
selection. Declared body length mismatches are rejected before a provider
mutation is dispatched. GCS JSON object metadata reports stored
bytes in its JSON `size` field while the HTTP `Content-Length` describes the
JSON document. GCS JSON metadata updates preserve the object's generation,
increment metageneration, and issue new `etag` and `updated` identity values.
GCS JSON uploads validate CRC32C as standard Base64 over four big-endian bytes:
multipart requests may supply `crc32c` in object metadata, while media and final
resumable requests use `X-Goog-Hash`. Malformed, conflicting, mismatched, or
unsupported checksum tokens fail before mutation, and a failed final resumable
checksum leaves the session retriable. Successful JSON object resources expose
the server-calculated `crc32c`. Current-generation JSON metadata, media, and
delete selectors must equal the stored generation; historical generation access
remains outside the accepted subset.

Tests can request one deterministic adverse response with
`x-sqrzl-failpoint`:

| Value | Phase and effect |
| --- | --- |
| `redirect-301`, `redirect-302`, `redirect-303`, `redirect-307`, `redirect-308` | Before commit; returns that redirect without mutating storage. Override the default dead-end target with `x-sqrzl-redirect-location`. |
| `conditional-request-conflict` | Before commit on conditional S3 PUT or multipart completion; returns `409 ConditionalRequestConflict` without mutating storage so clients can exercise the concurrent-delete race outcome. |
| `throttle`, `transient-500`, `transient-502`, `transient-503`, `transient-504` | Before commit; provider-shaped throttling or transient `5xx`, with `Retry-After: 1`. S3/Azure throttle as `503`; GCS/OCI throttle as `429`. |
| `timeout-before-commit` | Delays before dispatch. |
| `timeout-after-commit` | Dispatches first, then delays the response. |
| `response-loss-after-commit` | Commits a mutation, then closes with an intentionally incomplete response. |
| `truncate-response` | Returns half the body while retaining the original declared length. |
| `repeated-pagination-token`, `malformed-pagination-token` | Rewrites the provider continuation token deterministically. |

The conformance suite treats GCS JSON and GCS XML as separate front doors. It
exercises every redirect status and transient `5xx` above, framing mismatch,
before/after-commit timeout, committed response loss, truncation, throttling,
missing-object shape, and both pagination-token rewrites across S3, Azure Blob,
GCS JSON, GCS XML, and OCI Object Storage.

The same five-front-door suite writes WAL-, SST-, and catalog-shaped objects,
reopens the filesystem backend with fresh in-memory caches, verifies their raw
bytes, publishes a replacement catalog, and retires the old catalog and WAL.
That proves protocol CRUD persistence and retirement only; engine WAL replay,
SST coverage, catalog interpretation, and recovery remain client-suite work.

Timeout failpoints default to 1000 ms; set
`x-sqrzl-failpoint-delay-ms` to a positive test-specific delay. These control
headers are emulator-only and are never claims about a provider control plane.

Provider-owned data-protection/history modes are mutually exclusive for a given
bucket/container. Once S3 versioning/Object Lock, GCS retention/soft delete, or
Azure versioning/soft delete owns that namespace, conflicting activation and
protected mutations through another provider front door return that front
door's `409` response without changing data or mode metadata. In particular,
enabling S3 versioning never converts or unlocks a GCS- or Azure-protected
bucket.

S3 versioning preserves overwritten versions and creates delete markers;
suspended buckets preserve non-null history while replacing the single `null`
version on each PUT or simple DELETE. The accepted S3, Azure, GCS, and OCI
current-object HEAD/LIST paths remain strongly consistent, so Sqrzl does not
expose a stale-view failpoint for surfaces whose real providers promise strong
reads.
Create an S3 Object Lock mode bucket with the provider header
`x-amz-bucket-object-lock-enabled: true`; this enables versioning permanently.
Object-level lock headers are rejected outside that bucket mode, and Object
Lock PUTs in the accepted subset require a validated `Content-MD5`. Bucket
default retention updates, SDK checksum-algorithm variants, version-scoped
tagging, and Object Lock parameters on multipart initiation are explicitly
unsupported without mutation rather than silently approximated.
GCS JSON bucket create/PATCH accepts unlocked `retentionPolicy` durations greater
than zero and less than 100 years, plus `softDeletePolicy` durations from 7 days
through less than 90 days. Soft-delete uses durable object versions and active
retention rejects replacement or deletion through both JSON and XML object
surfaces. Policy locking, per-object retention, writable server-owned policy
fields, and disabling an enabled soft-delete policy are explicitly rejected as
`501 UNIMPLEMENTED` rather than approximated.
For focused Azure data-plane tests, create a container with
`x-sqrzl-azure-versioning-enabled: true` or
`x-sqrzl-azure-soft-delete-days: <1-365>`. These are emulator mode selectors,
not Azure request headers. Azure versions can be read or deleted
with `versionid=<id>`. These `x-sqrzl-*` controls intentionally avoid pretending
that Sqrzl implements Azure's full account-level service-properties API.
Container deletion models Azure's asynchronous deleting-name state: the
container disappears from reads and listings immediately, while recreating the
same name returns `409 ContainerBeingDeleted` until the lazy purge deadline.
The local-only `x-sqrzl-azure-delete-delay-ms` DELETE header can set a
nonnegative deterministic delay for tests; omitting it uses Azure's documented
30-second same-name exclusion window. Azure container/blob subresources outside
the documented accepted subset return `501 FeatureNotSupported` before ordinary
container or blob CRUD can run.

OCI bucket tiers accept `Standard` and `Archive`; object tiers accept
`Standard`, `InfrequentAccess`, and `Archive`, with an omitted object tier
inheriting the bucket default. Archive buckets reject non-Archive objects.
OCI list paging treats `start` as inclusive and `startAfter` as exclusive,
counts common prefixes toward `limit`, and returns the next unreturned name in
`nextStartWith`. OCI request correlation echoes a valid
`opc-client-request-id`, while malformed Signature authorization is rejected as
`401 NotAuthenticated` and a valid RSA-SHA256 shape is explicitly unsupported.
Object and multipart paths are decoded exactly once without collapsing empty
key components. Version-scoped object requests, conditional multipart
completion, and selective multipart commits are rejected explicitly without
touching the current object or consuming the upload session.

See [SMS and MMS emulation](docs/text-emulation.md) for callback security,
simulation behavior, media handling, and explicit unsupported scope.

### S3 example with boto3

```python
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9000",
    aws_access_key_id="local",
    aws_secret_access_key="local",
    region_name="us-east-1",
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)

s3.create_bucket(Bucket="example-bucket")
s3.put_object(Bucket="example-bucket", Key="hello.txt", Body=b"hello")
print(s3.get_object(Bucket="example-bucket", Key="hello.txt")["Body"].read())
```

With Sqrzl authentication disabled, the SDK may use any local placeholder
credentials. With authentication enabled, use the configured
`SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY`.

### Azure example

```python
from azure.storage.blob import BlobServiceClient

service = BlobServiceClient(
    account_url="http://localhost:9000/devstoreaccount1",
    credential=None,
)
container = service.create_container("example-container")
container.upload_blob("hello.txt", b"hello")
```

This example expects Sqrzl authentication to be disabled.

### GCS example

```python
from google.auth.credentials import AnonymousCredentials
from google.cloud import storage

gcs = storage.Client(
    project="sqrzl",
    credentials=AnonymousCredentials(),
    client_options={"api_endpoint": "http://localhost:9000"},
)
bucket = gcs.bucket("example-bucket")
bucket.create()
bucket.blob("hello.txt").upload_from_string("hello")
```

This example expects Sqrzl authentication to be disabled.

## Health checks and troubleshooting

Check readiness:

```bash
curl --fail http://localhost:9000/healthz
docker compose ps
docker compose logs --follow sqrzl
```

`/healthz` reports listener ports, storage readiness, request-size limit,
enabled providers, and authentication state. A healthy instance returns
`200 OK`; unreadable storage returns `503 Service Unavailable`.

Common problems:

| Symptom | Resolution |
| --- | --- |
| UI login fails | Use the exact two configured credential values. If neither is configured, refresh the UI; admin mode is open. |
| Provider SDK receives `401` or `403` | Either unset both credential variables for an open dev instance or configure the SDK to sign with the matching values and supported provider scheme. |
| Container exits with “Legacy nonempty storage” | The mounted root predates format v2 or contains unrelated files. Archive it, point `SQRZL_BLOBS_PATH` at an empty mount, or reset a disposable volume. |
| Data disappears after restart | Mount a named volume or bind mount at `/app/blobs`; container-local writable layers are disposable. |
| Host port is already in use | Change the host side of the Compose mapping, such as `"19000:9000"`. |
| Upload receives `413` | Increase `SQRZL_MAX_REQUEST_BYTES` to a positive byte count and recreate the container. |
| Startup bucket is rejected | Use the Amazon S3 general-purpose bucket-name rules summarized in the environment table. |

## Development and contract references

Docker users should not need these files to get started, but they provide
deeper implementation and support detail:

- [Support certification](docs/support-certification.md)
- [Compatibility matrix](compatibility-matrix.json)
- [Admin API OpenAPI contract](public/openapi.yml)
- [Architecture](docs/architecture.md)
- [Release notes](RELEASE_NOTES.md)
- [UI contributor guide](ui/README.md)

To run the repository’s complete local validation:

```bash
cntryl-tools validate-tests
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- \
  -D warnings -D clippy::pedantic
STRESS_PROFILE=smoke cargo test --workspace --all-targets --all-features
.venv/bin/python -m pytest
```

## License

Sqrzl is licensed under the [Apache License 2.0](LICENSE).
