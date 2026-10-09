# SDK qualification

Qualification covers selected operations declared in
[`acceptance-manifest.json`](../sdk-tests/acceptance-manifest.json). Each test
lists its provider, authentication mode, operations, and checks. The
`operation_checks` map assigns evidence to individual operations: a workflow
that paginates a list and rejects a conditional write does not certify
pagination or conditional writes for every operation in that workflow.
Provider support remains bounded by the operation matrix.

## Reproduce the pinned baseline

Use CPython 3.12 and a fresh environment. The lock pins all qualification
dependencies; `pyproject.toml` also pins direct SDK dependencies. Pytest rejects
dependency versions that differ from the recorded baseline.

```sh
python3.12 -m venv .venv-sdk
.venv-sdk/bin/python -m pip install -r sdk-tests/requirements.lock
.venv-sdk/bin/python -m pip check
cargo build --locked --bin sqrzl-emulator
SQRZL_SDK_LANE=functional .venv-sdk/bin/python -m pytest

SQRZL_SDK_ENFORCE_AUTH=1 SQRZL_SDK_PROVIDERS=s3,azure,gcs,oci \
SQRZL_SDK_LANE=storage-auth .venv-sdk/bin/python -m pytest \
  sdk-tests/test_s3_sdk.py sdk-tests/test_azure_sdk.py \
  sdk-tests/test_gcs_sdk.py sdk-tests/test_oci_sdk.py \
  sdk-tests/test_storage_acceptance.py sdk-tests/test_storage_auth_acceptance.py

SQRZL_SDK_MESSAGING_AUTH=1 \
SQRZL_SDK_PROVIDERS=email,twilio,sns,aws-sms-voice-v2 \
SQRZL_SDK_LANE=messaging-auth .venv-sdk/bin/python -m pytest \
  sdk-tests/test_email_sdk.py sdk-tests/test_sms_sdk.py \
  sdk-tests/test_messaging_acceptance.py
```

The messaging lane starts the server with synthetic SendGrid, ACS, Twilio, and
AWS credentials before clients run. Wrong-key tests exercise the official SDK
exception parsers, assert native error codes, and check that rejected requests
preserve captured messages. The storage lane verifies native S3 SigV4, Azure
SharedKey/SAS, OCI RSA signatures, and the selected GCS signed-URL path.

`qualification-baseline.json` records the exact SDK and API version snapshot.
SDK upgrades can change API versions independently of emulator source; the
qualification scope remains the selected request/response contracts.

## GCS authentication evidence

JSON SDK workflows use the explicitly local secret-based bearer convenience
mode when authentication is enabled. Their evidence is labeled
`local-bearer-or-anonymous-convenience`, and does not qualify Google OAuth.

The native HMAC test uses public `Bucket.generate_signed_url` and
`Blob.generate_signed_url` methods from the official storage SDK. A small
`google.auth.credentials.Signing` adapter supplies HMAC-SHA1 through the
SDK's signing interface; the SDK builds the canonical request and signed URL.
The official client's HTTP transport sends those URLs. Tests verify bucket
creation, object write/read/range/metadata/delete, and native XML failures for
wrong keys, wrong access IDs, and expiry without replacing stored bytes.
This gate qualifies V2 HMAC signing. The pinned Python SDK's V4 generator
hardcodes RSA, so native V4 HMAC and native OAuth remain separate evidence gaps.
[Google signed-URL documentation](https://docs.cloud.google.com/storage/docs/access-control/signed-urls)
specifies XML API endpoints; its
[header reference](https://docs.cloud.google.com/storage/docs/xml-api/reference-headers#content-length)
requires explicit Content-Length, including zero for bodyless requests.

## Process and artifact boundaries

The fixture owns a real emulator subprocess, file-backed stdout/stderr,
synthetic credentials, isolated storage root, and fixed ports. Selected
S3/Azure/GCS JSON/OCI tests force three listing pages, race two create-only
writes behind a barrier, assert exactly one winner and native condition/error
parsing, and read the winner after a new process starts against the same root.
Messaging tests also read captures after restart; ACS Email replays the same
repeatability key and original operation identity without another capture.

For resource campaigns, `sqrzl_server.process_pid`, `storage_dir`,
`runtime_dir`, and `log_path` expose the owned child's evidence locations.
`sqrzl_server.restart(kill=False)` performs normal termination and restart;
`restart(kill=True)` performs abrupt termination. The runtime also exposes
`stop(kill=...)` and `start()` for staged interruption. Each stop waits for the
old process to exit, and each start verifies health and a new PID. Restart
tests skip remote endpoints explicitly. Remote/container smoke, normal
restart, abrupt termination, crash publication, and measured resource
qualification are distinct evidence categories.

Each run writes `target/sdk-evidence/<lane>.json`, or the path supplied by
`SQRZL_SDK_EVIDENCE`. The artifact records source SHA and dirty state, SDK and
dependency versions, API version baseline, lock digest, collected test IDs,
pass/fail/skip reasons, acceptance scope, process/PID events, and test metrics.
Skipped tests and disabled families do not count as accepted gates. CI retains
functional, native storage, configured messaging, and container artifacts.

## Intentional upgrade lane

Dispatch CI with the boolean `sdk_upgrade` input to run a separate candidate
lane against the allowed dependency ranges in `.[sdk-upgrade]`. It records
the resolved versions and sets `upgrade_candidate=true`; it does not modify
the committed baseline. Locally, install that extra in a separate environment
and set `SQRZL_SDK_ALLOW_UPGRADE=1` for each candidate run. Review all three
lane artifacts before intentionally regenerating the lock, direct pins, and
version snapshot. A green upgrade smoke does not promote provider support.
