# Sqrzl Emulator

Sqrzl is a Docker-ready object and blob storage emulator for local development
and CI. Persistent filesystem-backed stores are available through
S3-compatible, Azure Blob Storage, Google Cloud Storage, and OCI Object Storage
HTTP APIs, outbound email capture, and bidirectional SMS/MMS provider adapters.
A browser UI is included for inspecting buckets, mailboxes, and texts.

Sqrzl is a development tool, not a production storage service. Provider
compatibility is intentionally scoped; see the
[compatibility matrix](compatibility-matrix.json) for current coverage.

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
| `SQRZL_MAX_REQUEST_BYTES` | Positive integer byte count | `134217728` (128 MiB) | Maximum buffered HTTP request body. Oversized provider requests receive a provider-shaped `413 Payload Too Large`. Zero and invalid values use the default. |
| `SQRZL_BUCKET_LIST` | Comma-separated bucket names | Empty | Buckets created at startup. Whitespace and empty entries are ignored. Names must be 3–63 characters using lowercase ASCII letters, digits, and single hyphens; they cannot start or end with a hyphen. |
| `SQRZL_LOG_FORMAT` | `text` or `json` (case-insensitive) | `text` | Log output format. Unknown values fall back to `text`. |
| `SQRZL_SMTP_PORT` | Unsigned 16-bit port | `2525` | SMTP capture listener. This is the only extra listener used by the mail domain. |
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
| GCS JSON API | API endpoint `http://localhost:9000` | Use anonymous credentials for the auth-disabled development flow. |
| GCS XML API | `http://localhost:9000/<bucket>/<object>` | Send `Host: storage.googleapis.com` when making raw XML API requests. |
| OCI Object Storage | Client endpoint `http://localhost:9000` | OCI paths use `/n/<namespace>/b/<bucket>/...`; the default namespace response is `sqrzl-emulator`. |
| Twilio Messages | API base `http://localhost:9000` | Supports outbound SMS/MMS references plus admin-driven inbound and delivery simulation. |
| Amazon SNS | Endpoint URL `http://localhost:9000` | Supports direct `PhoneNumber` SMS `Publish` only. |
| AWS SMS Voice v2 | Endpoint URL `http://localhost:9000` | Supports `SendTextMessage` and `SendMediaMessage`. |
| ACS SMS | Connection-string endpoint `http://localhost:9000` | Supports one-to-many SMS and admin-driven Event Grid callbacks. |

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
| Startup bucket is rejected | Use the common 3–63 character lowercase letter/digit/hyphen naming subset described in the environment table. |

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
