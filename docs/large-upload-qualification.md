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
  .venv-sdk/bin/python -m pytest -q sdk-tests/test_large_upload_qualification.py
```

The separate `Measured large upload qualification` workflow is manually
triggered. Its provider selector records disabled providers as skipped gates;
full four-provider acceptance requires all four tests to pass.

Each test writes dense, index-dependent bytes with a bounded 1 MiB generator
and an independent SHA256 oracle. It uploads through official SDK operations,
checks object length, reads **every byte** with bounded 8 MiB range requests,
calculates SHA256, performs a normal process stop/start against the same
storage root, and repeats the complete bounded readback. Range requests
avoid treating a buffered full-object download as a streaming guarantee.

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
