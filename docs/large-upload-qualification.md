# Measured large upload qualification

The opt-in campaign measures selected **1 GiB** objects for S3 multipart,
Azure block blobs, GCS JSON resumable upload, and OCI multipart upload. It
uses 64 MiB parts/chunks under the configured 128 MiB request cap. It provides
no evidence for an 8 GiB object or a provider's native maximum capacity.
Routine functional tests skip these four gates.

Run from a clean checkout with the pinned SDK environment:

```sh
python sdk-tests/build_provenance.py --output-dir /tmp/sqrzl-measured-binary
SQRZL_BINARY=/tmp/sqrzl-measured-binary/sqrzl-emulator \
SQRZL_BINARY_PROVENANCE=/tmp/sqrzl-measured-binary/build-provenance.json \
SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION=1 SQRZL_LARGE_UPLOAD_BYTES=1073741824 \
SQRZL_SDK_ENFORCE_AUTH=1 SQRZL_SDK_PROVIDERS=s3,azure,gcs,oci \
SQRZL_SDK_LANE=measured-upload \
  .venv-sdk/bin/python -u -m pytest -vv -s sdk-tests/test_large_upload_qualification.py
.venv-sdk/bin/python scripts/validate_contract_evidence.py \
  --sdk-evidence target/sdk-evidence/measured-upload.json \
  --results-source-sha "$(git rev-parse HEAD)" \
  --require-measured-providers s3,azure,gcs,oci \
  --output target/measured-contract-acceptance.json
```

The separate `Measured large upload qualification` workflow runs on pull
requests that change runtime `src/**`, `Cargo.toml`, `Cargo.lock`, the SDK
harness, evidence validators, or this workflow, and can also be
triggered manually. Its manual provider selector records disabled providers as
skipped gates; full four-provider acceptance requires all four tests to pass.
It checks out the exact pull-request head and retains the measured JSON, build
manifest and mechanically validated operation evidence. The immutable build
output directory must be new for each run.

`--require-measured-providers` makes qualification fail unless every explicitly
selected provider has an eligible passing result in the authenticated
`measured-upload` lane. Missing, skipped, or rejected campaigns cannot make
that job pass. The report is retained on rejection; unrelated operation
references may remain unproven. The collector without this option reports
evidence gaps without requiring a completed resource campaign.

Each test writes dense, index-dependent bytes with a bounded 1 MiB generator
and an independent SHA256 oracle. It uploads through official SDK operations,
checks object length, reads **every byte** with bounded 8 MiB range requests,
calculates SHA256, performs a normal process stop/start against the same
storage root, and repeats the complete bounded readback. Range requests
avoid treating a buffered full-object download as a streaming guarantee.

Measured SDK requests use one attempt so a failed transfer remains visible.
The direct GCS media upload has an explicit zero-retry strategy and a 60-second
bulk-send/read timeout, matching the main upload. Requests also applies the
connect timeout during a contiguous body's `sendall`; a five-second value
can expire while a 64 MiB chunk is still making progress on a debug build.
OCI's no-retry strategy is configured before namespace discovery and covers
its upload and control calls. These deadlines do not change payload sizes,
resource budgets, or recovery and cleanup requirements.

A separate incomplete session acknowledges its first 64 MiB part/chunk. The
campaign instruments the official SDK transport **after preparation and
signing**, sending identical file bytes and pausing the next 64 MiB request
at a 1 MiB prefix. It verifies that a partial request spool exists, kills and
reaps the server process, observes a client connection failure, and starts a
new healthy PID. The first acknowledged part/chunk must survive, the partial
second part must remain absent, and the incomplete object must remain
unpublished. Native abort/cancel (Azure uses a zero-byte Put Blob to discard staging, then deletes the empty blob)
then removes session state and staged data. Startup must remove the orphan
request spool. S3, Azure and GCS inspect recovery through their native part/block/
session queries. OCI list-parts and list-uploads are outside the implemented
operation scope: the campaign inspects its owned filesystem upload record,
acknowledged part identity/size and SHA256, and absence of the next part, then
uses native abort and verifies staging cleanup before deleting the bucket.
Cleanup checks owned multipart/upload/session/spool namespaces; durable
non-upload controls, such as Azure container-deletion tombstones, are preserved.
This inspection is labeled filesystem recovery evidence and does not qualify
the native OCI listing operations. This is a process interruption gate, separate from filesystem
publication crash tests and power-loss guarantees.

Every managed start verifies that the child owns the actual accepted API and
UI health connections. Linux checks socket inodes and endpoint pairs in
`/proc`; macOS checks established connections with `lsof`. A foreign HTTP200
response cannot establish readiness. Failed startup reaps the attempted child
and closes its log. Unexpected process exits fail the run and cannot be
recorded as normal termination.

The interrupted SDK worker must finish within 15 seconds after the prefix gate
is released and must be joined before restoring its transport or restarting
the service. A stuck worker fails the campaign, releases the reader, and kills
and reaps only its owned service. Its daemon fallback prevents an unresponsive
SDK thread from blocking interpreter shutdown; it cannot establish acceptance.
The workflow streams test and phase progress, dumps Python thread stacks if a
test lasts more than five minutes, and retains `measured-pytest.log` and the
per-provider `campaign-progress/*.jsonl` files alongside the acceptance data.
These diagnostic logs are separate from the validated resource measurements.

GCS JSON uses the local bearer convenience credential. Native GCS V2 HMAC
qualification runs separately in the SDK authentication lane; this resource
campaign does not qualify OAuth or native V4 HMAC. JSON resumable cancellation
asserts Google's documented 499 response, as described in the
[primary resumable upload reference](https://docs.cloud.google.com/storage/docs/performing-resumable-uploads#cancel-upload).

Default budgets are **512 MiB sampled client RSS**, **512 MiB sampled server
RSS**, and **5 GiB owned logical disk usage**. The sampler records RSS every
50 ms and owned file sizes every 250 ms, including payload, isolated storage,
request/staging files, process log, and immutable executable. Linux uses
`/proc/<pid>/status`; macOS uses `ps`. These are sampled measurements, not OS
hard limits or an instantaneous maximum claim. The artifact includes full
samples, peaks, budgets, phase timings, PIDs and interruption observations.
It requires enough free disk before generation and fails if sampling fails
or any observed budget is exceeded.

The budgets can be set with `SQRZL_LARGE_CLIENT_RSS_BYTES`,
`SQRZL_LARGE_SERVICE_RSS_BYTES`, and `SQRZL_LARGE_DISK_BYTES`.
`SQRZL_LARGE_SAMPLE_SECONDS` accepts 0.01–0.1 seconds. A smaller payload of at
least 256 MiB in multiples of 64 MiB is allowed for harness preflight and is
labeled `smaller-payload-preflight`; it is not 1 GiB qualification evidence. Its result records
`qualification_eligible=false` and a rejection reason, even when the preflight
test passes. Every result records those eligibility fields consistently.
Remote endpoints explicitly skip process-controlled gates.

Evidence is written to `target/sdk-evidence/measured-upload.json` (or the
selected lane/evidence path). It binds harness source, verified binary build
source/digest, pinned SDK/dependency versions, resolved API selectors,
selected operations, resource metrics, normal restart and abrupt termination
as distinct events, and pass/fail/skip outcomes. Dirty or unverified binaries
cannot establish exact source acceptance. The workflow retains the JSON and
build manifest; its existence alone does not count as a measured run. The
`large_upload_campaign` property follows
[`large-campaign.schema.json`](../sdk-tests/large-campaign.schema.json). Acceptance
requires the exact 1 GiB kind/size, completed=true, eligible=true, nonempty
resource samples within budgets, two complete SHA256 readbacks, distinct normal
and abrupt process events, acknowledged staging recovery and abort cleanup.

The validator also recomputes peaks from the samples, checks their timing and
PID coverage, links restart phases to reaped child-process events, and requires
the configured 64 MiB parts and 8 MiB read ranges. A smaller successful preflight,
a JUnit pass without measurements, or inconsistent cleanup/source records cannot
qualify this scope. Artifact authors are trusted runners; these consistency
checks are not cryptographic attestation or instantaneous OS resource limits.
