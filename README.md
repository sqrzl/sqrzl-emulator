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
be absent. Setting only one is rejected at startup.

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
      - "2525:2525"
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

The table below is the complete runtime configuration surface. `SQRZL_*` is
reserved for emulator settings, including shared authentication and callback
policy. Vendor credentials and resource settings use their provider prefix
(`AZURE_*`, `GCS_*`, `OCI_*`, `TWILIO_*`, `SENDGRID_*`, `ACS_*`, or `SES_*`).

| Variable | Accepted values | Default | Description |
| --- | --- | --- | --- |
| `SQRZL_ACCESS_KEY_ID` | Any string; set together with `SQRZL_SECRET_ACCESS_KEY` | Unset | S3 access key and admin username. Provider and admin authentication are enabled only when both generic credential variables are present. Incomplete or empty credentials fail startup. Also supplies the Azure account or GCS HMAC access ID when that provider's native override is unset. |
| `SQRZL_SECRET_ACCESS_KEY` | Any string; set together with `SQRZL_ACCESS_KEY_ID` | Unset | S3 signing secret and admin password. Also supplies the Azure account key or GCS HMAC secret when that provider's native override is unset. |
| `AZURE_ACCOUNT` | Azure storage account name; set with `AZURE_ACCOUNT_KEY` | Generic access key, otherwise unset | Azure Shared Key and SAS account override. These vendor-native variables intentionally have no `SQRZL_` prefix. |
| `AZURE_ACCOUNT_KEY` | Azure storage account key; set with `AZURE_ACCOUNT` | Generic secret, otherwise unset | Azure Shared Key and SAS signing key. Valid Base64 is decoded as Azure key material; other values are used literally. Also the local bearer token accepted by Get User Delegation Key. |
| `AZURE_ENCRYPTION_SCOPES` | Comma-separated Azure encryption scope names | Unset (no scopes exist) | Scopes that `x-ms-encryption-scope` and SAS `ses` may name. Any other scope returns `403 AuthorizationFailure`. |
| `GCS_HMAC_ACCESS_ID` | GCS interoperability HMAC access ID; set with `GCS_HMAC_SECRET` | Generic access key, otherwise unset | GCS GOOG1/V2 and GOOG4 HMAC signed-URL identity. |
| `GCS_HMAC_SECRET` | GCS interoperability HMAC secret; set with `GCS_HMAC_ACCESS_ID` | Generic secret, otherwise unset | GCS GOOG1/V2 and GOOG4 HMAC signing secret. |
| `OCI_TENANCY_OCID` | OCI tenancy OCID | Unset | First component of the accepted OCI request-signing key ID. Configure all four OCI variables together. |
| `OCI_USER_OCID` | OCI user OCID | Unset | Second component of the accepted OCI request-signing key ID. |
| `OCI_KEY_FINGERPRINT` | OCI API signing-key fingerprint | Unset | Final component of the accepted OCI request-signing key ID. |
| `OCI_PUBLIC_KEY_PATH` | Readable PKCS#1 or PKCS#8 RSA public-key PEM path | Unset | Public key used to verify OCI RSA-SHA256 request signatures. |
| `SQRZL_ADMIN_AUTH_DISABLED` | `1`, `true`, `yes`, or `on` (case-insensitive) enable it; every other value is false | `false` | Keeps the UI and `/admin/v1` in open mode while provider API authentication remains enabled. |
| `SQRZL_MCP_AUTH_ENABLED` | Boolean (`1`, `true`, `yes`, or `on`) | `false` | Requires OAuth bearer tokens for `/mcp`. MCP is also protected whenever both generic access-key variables are set. |
| `SQRZL_MCP_RESOURCE_URL` | Absolute HTTP(S) URL with path exactly `/mcp` | Unset | Canonical protected-resource URI. Required when MCP auth is enabled. |
| `SQRZL_MCP_AUTHORIZATION_SERVER` | Absolute HTTP(S) URL | Unset | External authorization server advertised in protected-resource metadata. Sqrzl does not issue tokens. |
| `SQRZL_MCP_ISSUER` | JWT issuer string | Unset | Exact `iss` value required in OAuth access tokens. |
| `SQRZL_MCP_AUDIENCE` | JWT audience string | Unset | Exact audience required in OAuth access tokens; normally the MCP resource URL. |
| `SQRZL_MCP_JWKS_URL` | HTTPS URL | Unset | External authorization server's JSON Web Key Set endpoint. |
| `SQRZL_MCP_ALLOWED_HOSTS` | Comma-separated HTTP Host values | Loopback hosts | Adds Host values accepted by the MCP transport. |
| `SQRZL_MCP_ALLOWED_ORIGINS` | Comma-separated origins | `http://localhost:<SQRZL_UI_PORT>`, `http://127.0.0.1:<SQRZL_UI_PORT>` | Origins accepted by the MCP transport. Requests without an `Origin` header are allowed for non-browser MCP clients. |
| `SQRZL_BLOBS_PATH` | Writable container path | `/app/blobs` in Docker; `./blobs` natively | Storage format v2 root. Mount a volume here for persistence. |
| `SQRZL_LIFECYCLE_HOURS` | Positive integer hours | `1` | Interval between lifecycle-rule passes. Invalid, zero or overflowing values fail startup before storage is created. |
| `SQRZL_API_PORT` | Unsigned 16-bit port (`0`–`65535`) | `9000` | Storage API listener inside the container. Normally keep this at `9000` and change only the host side of the Docker port mapping. |
| `SQRZL_UI_PORT` | Unsigned 16-bit port (`0`–`65535`) | `9001` | Admin UI listener inside the container. Normally keep this at `9001`. |
| `SQRZL_MAX_REQUEST_BYTES` | Positive integer byte count | `134217728` (128 MiB) | Maximum bytes in one HTTP request body or SMTP `DATA` payload. S3 PUT/UploadPart, Azure Put Blob/Put Block, GCS data uploads/resumable chunks, and OCI PutObject/UploadPart stream to disk while enforcing this per-request cap. Oversized provider requests receive a provider-shaped `413`; SMTP receives `552`. Zero and invalid values fail startup before storage is created. |
| `SQRZL_MAX_CONNECTIONS` | Positive integer | `128` | Shared API/UI/SMTP connection cap; excess connections close (SMTP replies `421` where possible). |
| `SQRZL_MAX_INFLIGHT_REQUESTS` | Positive integer | `4` | Shared nonqueued HTTP, MCP tool and SMTP DATA work budget. HTTP overload returns `503`/`Retry-After`; SMTP DATA returns `452`. |
| `SQRZL_MAX_INFLIGHT_BYTES` | Positive integer bytes, at least `SQRZL_MAX_REQUEST_BYTES` | `536870912` (512 MiB) | Conservative encoded-input reservation: each admitted request reserves the configured maximum body size until its response/worker finishes. This is not an OS RSS limit. |
| `SQRZL_MAX_MCP_SESSIONS` | Positive integer | `128` | Maximum retained MCP sessions; initialization beyond the cap returns `503`. |
| `SQRZL_MAX_SMTP_RECIPIENTS` | Positive integer | `1000` | Maximum accepted RCPT commands per transaction; excess recipients receive `452` while the accepted envelope remains usable. |
| `SQRZL_DRAIN_TIMEOUT_MS` | Integer `1`–`300000` | `30000` | One shared SIGINT/SIGTERM drain deadline across listeners; admitted work completes, then stalled async connections are cancelled. Blocking filesystem writes retain storage ownership until they finish. |
| `SQRZL_BUCKET_LIST` | Comma-separated bucket names | Empty | Buckets created at startup. Whitespace and empty entries are ignored. Names use the Amazon S3 general-purpose rules: 3–63 lowercase letters, digits, periods, or hyphens; an alphanumeric first and last character; no adjacent periods, IP-address form, or AWS-reserved affix. |
| `SQRZL_LOG_FORMAT` | `text` or `json` (case-insensitive) | `text` | Log output format. Unknown values fall back to `text`. |
| `SQRZL_SMTP_PORT` | Unsigned 16-bit port | `2525` | SMTP capture listener. This is the only extra listener used by the mail domain. |
| `SENDGRID_API_KEY` | SendGrid API key | Unset | Enables SendGrid bearer authentication when set. |
| `TWILIO_ACCOUNT_SID` | Twilio account SID; set with `TWILIO_AUTH_TOKEN` | Unset | Enables Twilio Basic authentication when both Twilio values are present; incomplete credentials fail startup. |
| `TWILIO_AUTH_TOKEN` | Twilio auth token; set with `TWILIO_ACCOUNT_SID` | Unset | Twilio Basic authentication secret and callback-signing key. |
| `TWILIO_API_KEY_SID` | Twilio API key SID (`SK...`); set with `TWILIO_API_KEY_SECRET` | Unset | Additional Basic credential accepted for the configured account when Twilio authentication is enabled. Callbacks stay signed with the auth token. |
| `TWILIO_API_KEY_SECRET` | Twilio API key secret; set with `TWILIO_API_KEY_SID` | Unset | Password paired with `TWILIO_API_KEY_SID`. |
| `ACS_CONNECTION_STRING` | `endpoint=<url>;accesskey=<base64>` | Unset | Enables ACS email/SMS HMAC authentication. |
| `SQRZL_TEXT_CALLBACK_ALLOWED_HOSTS` | Comma-separated hostnames or IP addresses | Loopback hosts | Adds callback hosts to `localhost`, `127.0.0.1`, and `::1`. Redirects remain disabled. |
| `SQRZL_TEXT_CALLBACK_TIMEOUT_MS` | Positive integer milliseconds | `5000` | Timeout for each inbound or delivery callback attempt. |
| `SES_CONFIGURATION_SETS` | Comma-separated SES configuration set names | Unset | When set, SES `SendEmail` with an unlisted `ConfigurationSetName` returns `NotFoundException`. When unset, any valid name is accepted. |
| `SQRZL_MAIL_CALLBACK_ALLOWED_HOSTS` | Comma-separated hostnames or IP addresses | Loopback hosts | Adds SES event callback hosts to `localhost`, `127.0.0.1`, and `::1`. Redirects remain disabled. |
| `SQRZL_MAIL_CALLBACK_TIMEOUT_MS` | Positive integer milliseconds | `5000` | Timeout for each simulated SES event callback. |

## SES event publishing simulation

SES event simulation accepts an optional `request_id` (1–128 ASCII letters,
digits, hyphens or underscores). Supply it before sending when a lost response may be retried. Reuse it with the same payload to read the
persisted outcome without sending the callback again. Each response includes a
request ID; retain it for retries. Reusing an ID with a changed payload is rejected.
Events are persisted with `state: pending` before local updates or callback
delivery. Completed processing records `state: completed` and the callback result.
A pending record after interruption has an incomplete or uncertain outcome and is
never resent automatically. Without a supplied request ID, each call creates a
new event. These semantics prevent automatic duplicate delivery; they do not claim
exactly-once delivery to an HTTP receiver.

Captured SES messages keep their `EmailTags` and `ConfigurationSetName`; both
appear on the admin message detail (`email_tags`, `configuration_set_name`),
in the UI, and through the `mail_message_get` MCP tool. To test code that
consumes SES event-publishing records:

1. Optionally route a configuration set to a callback:
   `PUT /admin/v1/ses-event-destinations/{configurationSet}` with
   `{"callback_url": "http://127.0.0.1:8080/ses", "event_types": ["Delivery", "Bounce"]}`
   (omit `event_types` to deliver every type).
2. Emit an event: `POST /admin/v1/ses-events` with
   `{"message_id": "<SES MessageId>", "event_type": "Bounce", "bounce_type": "Transient", "recipients": ["user@example.com"]}`.
   Event types are `Send`, `Delivery`, `Bounce`, `Complaint`, `Reject`,
   `DeliveryDelay`, and `Rendering Failure`.
3. Read records back with `GET /admin/v1/ses-events?message_id=<id>`.

Each record has `eventType`, the `mail` object (`timestamp`, `messageId`,
`source`, `destination`, `commonHeaders`, and `tags` with the caller's tags plus
`ses:configuration-set`, `ses:from-domain`, `ses:source-ip` and
`ses:caller-identity`), and the per-type detail object. When the message's
configuration set has a destination, the record is posted as an SNS
`Notification` envelope (`x-amz-sns-message-type: Notification`) and the
attempt is stored with the record. Delivery, Bounce, Reject, and Rendering
Failure also update the captured copies' delivery state. The same operations
are available as `mail_ses_event_*` MCP tools. Envelopes are unsigned; SNS
subscriptions, signing certificates, EventBridge, and Firehose are not modelled.

## MCP and usage diagnostics

Sqrzl serves the standard MCP Streamable HTTP transport at `/mcp` on the UI
listener (normally port `9001`). It advertises tools for buckets, objects,
multipart uploads, captured mail, simulated text messages, usage queries, and
read-only health and usage resources. Tool calls use the same stores and admin
services as the UI and `/admin/v1` endpoints.

MCP is open when provider authentication is disabled. In protected mode, an
external authorization server issues bearer tokens with the configured issuer
and audience. The scopes are `sqrzl:read` for discovery and reads,
`sqrzl:write` for mutations.
Protected-resource metadata is available at
`/.well-known/oauth-protected-resource/mcp`. Browser-cookie sessions continue
to protect the admin UI and admin HTTP API; MCP clients send OAuth bearer
tokens.

The read-only `/metrics` endpoint exports Prometheus text for provider, bucket,
client, operation, byte, request-size, and list activity. Add
`?min_requests=N` to select a request-count threshold; the `usage_query` tool
and `sqrzl://usage` resource also accept caller-selected request and byte
thresholds. These counters are process-local and reset on restart. When admin
auth is enabled, `/metrics` requires the same admin session cookie as the UI.

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

Run one emulator writer per storage root. Startup holds an exclusive
`.sqrzl-writer.lock`; a second process using that root exits. Use distinct roots
for concurrent instances. Provider, admin, and lifecycle operations share
serialized protection decisions. See [writer ownership](docs/storage-writer-ownership.md)
for the enforced namespace boundaries and library embedding contract.

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
| Azure Blob | Account URL `http://localhost:9000/devstoreaccount1` | The account is the first path segment. Shared Key and service SAS use `AZURE_ACCOUNT` and `AZURE_ACCOUNT_KEY` when configured. User delegation SAS and encryption scopes are described below. |
| GCS JSON API | API endpoint `http://localhost:9000` | Use anonymous credentials when auth is disabled. HMAC signing uses `GCS_HMAC_ACCESS_ID` and `GCS_HMAC_SECRET`; the local JSON SDK qualification also accepts the configured secret as a bearer token. |
| GCS XML API | `http://localhost:9000/<bucket>/<object>` | Send `Host: storage.googleapis.com` when making raw XML API requests. |
| OCI Object Storage | Client endpoint `http://localhost:9000` | OCI paths use `/n/sqrzl-emulator/b/<bucket>/...`; namespace discovery returns `sqrzl-emulator`. Authenticated requests use the four `OCI_*` identity/public-key variables and OCI RSA-SHA256 request signing. |
| SMTP | SMTP server `localhost:2525` | Plaintext local capture with strict envelope paths and null reverse-path support; SMTP AUTH and STARTTLS are intentionally unsupported. |
| SendGrid Mail Send | API base `http://localhost:9000` | Supports the matrix-listed v3 personalizations/content/attachment subset at `POST /v3/mail/send` and optional `SENDGRID_API_KEY` bearer authentication. |
| Amazon SES v2 | Endpoint URL `http://localhost:9000` | Supports the matrix-listed `Content.Simple` subset through `POST /v2/email/outbound-emails` with SigV4, including validated `EmailTags` and `ConfigurationSetName`; Raw, Template, and attachments are rejected. Admin-driven event publishing emits SES records (see below). |
| ACS Email | Connection-string endpoint `http://localhost:9000` | Supports the matrix-listed `POST /emails:send` subset, operation polling, caller operation IDs, repeatability, and ACS HMAC authentication. |
| Twilio Messages | API base `http://localhost:9000` | Supports outbound SMS/MMS references plus admin-driven inbound and delivery simulation. |
| Amazon SNS | Endpoint URL `http://localhost:9000` | Supports direct `PhoneNumber` SMS `Publish` only. |
| AWS SMS Voice v2 | Endpoint URL `http://localhost:9000` | Supports `SendTextMessage` and `SendMediaMessage`. |
| ACS SMS | Connection-string endpoint `http://localhost:9000` | Supports one-to-many SMS and admin-driven Event Grid callbacks. |

## Large object uploads

Sqrzl follows each storage vendor's native large-upload protocol. Request bytes
are spooled and hashed incrementally, parts survive adapter restart, and final
objects are composed on disk without loading the complete object into memory.
The default `SQRZL_MAX_REQUEST_BYTES` remains a per-request safety limit, so
choose a part or chunk size at or below 128 MiB or raise that setting for larger
individual requests.

| Provider | Native pattern | Delegated upload | Emulated service boundaries |
| --- | --- | --- | --- |
| Amazon S3 | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, or AbortMultipartUpload | SigV4-presigned UploadPart URLs | 10,000 parts; 5 MiB minimum except the final part; 5 GiB maximum per part; about 48.8 TiB effective object maximum |
| Azure Blob Storage | Put Block followed by Put Block List | One blob SAS can stage and commit blocks | 100,000 uncommitted blocks; 50,000 committed blocks; 4,000 MiB per block; about 190.7 TiB block-blob maximum; 5,000 MiB direct Put Blob maximum |
| Google Cloud Storage | JSON or XML resumable session with 256 KiB-aligned non-final chunks and status probes | GOOG4 HMAC-signed initiation returns the resumable session URI | 5 TiB object maximum; seven-day resumable-session lifetime |
| OCI Object Storage | CreateMultipartUpload, UploadPart, CommitMultipartUpload, or AbortMultipartUpload | Object PAR direct PUT or native PAR multipart initiation with `opc-multipart: true` | 10,000 parts; 10 MiB minimum except the final part; 50 GiB maximum per part; 10 TiB object maximum; 50 GiB direct PutObject maximum |

Azure MD5/CRC64, GCS CRC32C, OCI MD5/SHA-256/SHA-384/CRC32C/CRC64, and S3
request checksum paths are calculated while streaming where their accepted API
surface requires them. Successful aborts and removals reclaim session payloads; failed requests preserve
previously accepted parts. Azure omitted uncommitted blocks retain the selected
retention behavior, and Put Blob overwrite clears them. GCS XML cancellation
retains a small expiring decision while removing active chunks. This is a local emulator contract, not a
claim that a single Sqrzl instance has enough disk for every theoretical vendor
maximum.

The historical opt-in harness defaults to 8 GiB per provider. That size is a
requested workload, not passing resource evidence. The separate measured
campaign records payload size, service/client RSS and disk budgets, complete
bounded-range digest readback, restart/interruption and staging cleanup. See
[large-upload qualification](docs/large-upload-qualification.md). Run the
selected harness with:

```bash
SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION=1 \
SQRZL_SDK_PROVIDERS=s3,azure,gcs,oci \
python -m pytest sdk-tests/test_large_upload_qualification.py
```

Set `SQRZL_LARGE_UPLOAD_BYTES` to a smaller byte count for a quick harness
smoke. Keep it above 64 MiB to exercise multipart behavior on every provider.

S3 direct PUT accepts the selected additional checksum algorithms. Checksum-aware
multipart supports explicit `CRC32C` / `FULL_OBJECT` initiation, validated part
and completion receipts, and atomic conditional completion. Other multipart
checksum algorithms, composite profiles, checksum copy and trailers remain
explicit `501` boundaries. See [streaming checksum qualification](docs/checksum-streaming-qualification.md)
for provider specification links and reproducible authenticated tests. Plain S3
multipart uploads require boto3's `request_checksum_calculation="when_required"`
configuration; the SDK-default CRC32 UploadPart variant is explicitly rejected.
Whole materialized S3/Azure reads, copies and selected payload-rewriting Azure
operations have a 64 MiB local cap; metadata-only HEAD and bounded ranges can
inspect larger objects. Azure page/append extents share that local cap. Native
admission constants above do not establish cloud-capacity qualification.

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

GCS JSON `objects.compose` (up to 32 same-bucket sources), `objects.rewrite`,
and `objects.copy` support the "upload parts, compose, verify, then publish
only if absent" flow: per-source `generation` and `ifGenerationMatch`,
`ifSourceGenerationMatch`/`ifSourceGenerationNotMatch`, and destination
`ifGenerationMatch` (including `0`) are checked atomically with the write and
fail with `412` without mutation. Composed objects report CRC32C and
`componentCount`; rewrites always finish in one call with `done: true`.
Sources are materialized locally up to 64 MiB. `rewriteToken` continuation,
destination ACLs, and other unsupported destination fields are rejected.

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
mutation is dispatched. GCS JSON API request bodies honour
`Content-Encoding: gzip` (the Google .NET client default) for metadata documents
and resumable-session metadata, bounded by `SQRZL_MAX_REQUEST_BYTES` after
decompression; other codings, and any coding on streamed media bodies, return
`415 unsupportedMediaType` instead of being parsed as identity bytes. GCS JSON object metadata reports stored
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
| `checksum-receipt-missing`, `checksum-receipt-corrupt` | After a successful mutation, removes or changes CRC32C response receipts without changing stored bytes or metadata, for pinned-readback fallback tests. |
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
GCS customer-managed encryption keys are modelled locally without
cryptography: bucket `encryption.defaultKmsKeyName` is accepted on create and
PATCH (`null` clears it), JSON uploads accept `kmsKeyName`, and XML uploads
accept `x-goog-encryption-kms-key-name`. Key names must match
`projects/*/locations/*/keyRings/*/cryptoKeys/*` or the upload fails with `400`
before mutation. The bucket default applies when an upload names no key, and
object resources return `kmsKeyName` with a `/cryptoKeyVersions/1` suffix. Key
existence, IAM, and rotation are not modelled; encryption enforcement configs
return `501`.
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
`401 NotAuthenticated`. Configured tenancy/user/fingerprint identity and the RSA
public key verify the selected native RSA-SHA256 request signature contract.
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

### Azure encryption scopes, server-side copy, and user delegation SAS

- **Encryption scopes.** Declare the scope names your tests use in
  `AZURE_ENCRYPTION_SCOPES`. Put Blob, Put Block, Put Block List, and
  Copy Blob From URL record the scope from `x-ms-encryption-scope` or from a
  SAS `ses` field. The scope is returned in `x-ms-encryption-scope`, in Get
  Blob Properties, and in listings.
  - A SAS-pinned scope that differs from the header returns
    `400 InvalidHeaderValue`.
  - A Put Block List that commits blocks staged under another scope fails the
    same way.
  - Container default scopes return `501`.
  - Nothing is actually encrypted.
- **Copy Blob From URL.** `PUT` with `x-ms-copy-source` and
  `x-ms-requires-sync: true` copies a blob in the same account and returns
  `202` with `x-ms-copy-status: success`.
  - When authentication is enabled, the source URL must carry a SAS with read
    permission.
  - `x-ms-source-if-match` and `x-ms-source-if-none-match` are checked against
    the source. A failure returns `412 SourceConditionNotMet`.
  - `If-Match` and `If-None-Match: *` are applied atomically with the write.
    They return `412 ConditionNotMet` or `409 BlobAlreadyExists`.
  - Sources are limited to 64 MiB.
  - Asynchronous Copy Blob, cross-account sources, and snapshot or version
    sources return `501`.
- **User delegation SAS.** `POST /<account>?restype=service&comp=userdelegationkey`
  accepts only `Authorization: Bearer <AZURE_ACCOUNT_KEY>`; Shared Key is
  refused, as on Azure.
  - The key is issued for one fixed local principal and is valid for at most
    seven days.
  - A blob or container SAS signed with that key authorizes the same
    permissions as a service SAS. It is refused after the key's `SignedExpiry`.
  - The Azure SDKs refuse token credentials over plain HTTP. Send the key
    request directly, or through an HTTPS proxy, and pass the returned key to
    `generate_blob_sas`.

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

`/healthz` reports listener ports, blob/mail/SMS readability, runtime admission counts and limits, request-size limit,
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
| Upload receives `413` | Increase `SQRZL_MAX_REQUEST_BYTES` to a positive byte count and recreate the container, or use the provider's native multipart/block/resumable pattern with smaller individual requests. |
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

### Runtime draining and diagnostics

SIGINT and SIGTERM stop admission across API, UI, MCP and SMTP. Active uploads,
SMTP DATA and callbacks share one drain deadline. Idle SMTP sessions receive
`421`; work admitted before shutdown can finish. A deadline cancels stalled
async work. Uninterruptible blocking storage operations may extend process exit;
the writer lock remains held until those writes finish. Callback outcomes lost
during cancellation remain pending and are never automatically resent.

`/healthz` returns `503` while draining or when a storage/capture root becomes
unreadable. Readability does not guarantee free disk space or writable media.
One separate probe slot keeps health and UI `/metrics` available during request
saturation, subject to the shared connection cap. Metrics include active work,
reserved bytes, rejections, fixed HTTP status classes and duration buckets.
There are no path, credential or message-content labels. SMTP work uses the same
counters; cancelled/unclassified work has status class zero.

Admission bounds active counts and encoded input reservations. Decoding,
provider fanout, storage caches and OS page cache consume additional memory;
use container memory/disk limits when a hard process boundary is required.
There is no unbounded admission queue. Mail capture separately enforces its
64 MiB projected allocation budget.

Admin mailbox pages use transaction-published metadata summaries, so listing or
changing delivery state does not deserialize or rewrite attachments. Pages hold
at most 500 items and normally at most 8 MiB of summaries; one large summary can
use up to 64 MiB. Mailbox index scans remain linear, and substring search may
scan multiple batches. Explicit bulk library methods remain bulk operations.
Existing mail is indexed once at startup, one payload at a time.

SES event intent, payload and global/message indexes commit together in
`.ses-events-v1.redb` with an 8 MiB database cache. Existing `.ses-events` JSON
records migrate idempotently; retained JSON files are backups, and the database
becomes authoritative. Back up the entire storage root while the emulator is
stopped. Do not downgrade to a writer that only understands the legacy JSON
archive. Event pages seek the selected index and decode only selected records.

The versioned [local-core-v1 certification policy](operation-certification.json)
defines nine selected operation scopes. Exact-source CI produces their gate
receipts; other operations and selectors retain the matrix's partial,
unsupported or deferred status. See [support qualification](docs/support-certification.md).
