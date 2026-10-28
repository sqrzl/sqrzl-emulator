# Checksum-aware streaming uploads

Issue [#79](https://github.com/sqrzl/sqrzl-emulator/issues/79) covers the
checksum-aware multipart/resumable paths reported by downstream Midge #915.
Qualification here covers Sqrzl's public HTTP contracts. It does not establish
real-provider qualification, Midge engine acceptance, or memory/performance bounds.

## Specification basis

The implementation follows these provider specifications, reviewed 2026-10-28:

- AWS [CreateMultipartUpload](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CreateMultipartUpload.html)
  defines the checksum algorithm/type negotiation and response headers.
- AWS [UploadPart](https://docs.aws.amazon.com/AmazonS3/latest/API/API_UploadPart.html)
  defines part checksum receipts. An SDK algorithm selector requires a checksum
  or trailer; an individual checksum overrides the selector.
- AWS [CompleteMultipartUpload](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html)
  defines the checksum manifest, full-object receipt in XML, checksum-type
  mismatch (`BadDigest`), size mismatch (`InvalidRequest`), and conditional
  completion (`412 PreconditionFailed`).
- AWS [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
  requires both `s3:PutObject` and `s3:GetObject` for `If-Match`. A missing current
  version or current delete marker returns `404`; a nonmatching existing ETag
  returns `412`. Existence, the ETag predicate, and publication are evaluated
  under one object lock.
- AWS [CompletedPart](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompletedPart.html)
  requires consecutive additional-checksum part numbers starting at one,
  with `400 InvalidPartOrder` for violations.
- AWS [upload integrity](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html)
  distinguishes full-object CRC32C from a composite checksum and requires
  server-side validation before storing a supplied full-object digest.
- Google [resumable uploads](https://docs.cloud.google.com/storage/docs/performing-resumable-uploads)
  defines session capabilities, JSON/XML initiation, aligned non-final chunks,
  `308` prefix acknowledgements, status probes, and resuming interrupted uploads.
- Google [XML initiation](https://docs.cloud.google.com/storage/docs/xml-api/post-object-resumable)
  documents the `201` response and a `Location` containing `upload_id`.
  [XML object uploads](https://docs.cloud.google.com/storage/docs/xml-api/put-object-upload)
  return metadata headers and an empty response body.

Session URIs are opaque. The old path-only UUID was unique, but incompatible
with the downstream client's query-identity check; this was not evidence of a
missing UUID generator. New Sqrzl locations use
`/upload/resumable/{uuid}?upload_id={uuid}`. Existing path-only sessions remain
usable. Conflicting or duplicated supplied query identities are rejected.
The exact emulator path is not a promise about Google's URL layout.

## Supported subset

S3 supports explicit `CRC32C` / `FULL_OBJECT` initiation, checksum-aware part
uploads, persisted part checksums, `ListParts` receipts, optional part digests
in completion manifests, full-object checksum/size validation, and atomic
`If-Match` or `If-None-Match: *` completion. Missing caller digests are calculated
by the server. Completion receipts appear in XML; checksum-enabled GET/HEAD
returns the stored full-object checksum. Assembly validates staged bytes before
publication with a bounded buffer. A rejected completion retains its session.
Present multipart integrity and publication headers must contain one ASCII
value. Malformed or duplicated values fail before staging or publication;
authenticated requests with duplicated `x-amz-*` headers can be rejected earlier
by the SigV4 verifier. They are never interpreted as absent preconditions or
absent caller digests. An `If-Match` read-authorization failure retains both the
existing object and the upload, allowing completion after permissions are restored.

Other multipart checksum algorithms, composite profiles, checksum copy, and
streaming checksum trailers return explicit `501 NotImplemented`. Plain
multipart remains supported; SDK-default CRC32 parts still require opting out.
Multipart Object Lock is outside this subset.

GCS XML and JSON sessions accept independent interleaved uploads and survive
process restart. Invalid checksums cannot publish an object. XML completion
returns ETag, generation, metageneration and `x-goog-hash` CRC32C headers with an
empty body. JSON completion returns object metadata including `crc32c`.

For client assurance tests, explicit `x-sqrzl-failpoint` values
`checksum-receipt-missing` and `checksum-receipt-corrupt` alter successful
mutation response receipts only. They do not modify persisted bytes or checksums.
These are emulator fault controls, not normal cloud-provider behavior. The SDK
tests exercise identity-pinned independent readback after each fault.

## Reproduction and evidence

The original source `c7b3fd6` fails all three focused authenticated regressions:
S3 returns `501` at checksum-aware initiation; both GCS variants issue a usable
path session without the downstream-compatible `upload_id` query identity.
The XML test uses the official Google client's V2 HMAC signing interface with
an anonymous transport, avoiding unrelated bearer transport headers in signatures.

The tests also prove red cases for missing/corrupt receipt failpoints, malformed
S3 checksum manifests, nonconsecutive checksum part numbers, and an SDK checksum
selector without its required digest. Run the complete S3/GCS suite with:

```bash
python3.12 -m venv /tmp/sqrzl-sdk-venv
/tmp/sqrzl-sdk-venv/bin/pip install -r sdk-tests/requirements.lock
SQRZL_SDK_ENFORCE_AUTH=1 SQRZL_SDK_PROVIDERS=s3,gcs \
  SQRZL_SDK_LANE=checksum-streaming \
  /tmp/sqrzl-sdk-venv/bin/python -m pytest \
  sdk-tests/test_s3_sdk.py sdk-tests/test_gcs_sdk.py
```

Run from a clean commit without `SQRZL_BINARY` or `SQRZL_API_URL` overrides.
The harness owns and authenticates the HTTP server, builds an immutable binary,
and writes `target/sdk-evidence/checksum-streaming.json` with the source commit,
source tree, binary SHA-256, dependency versions, process events, and exact test
results. A dirty-source run is diagnostic evidence only. The tests cover S3
and both GCS protocols with objects larger than 5 MiB, exact independent
readback, wrong digests, rejected replacement parts, interrupted uploads,
distinct active GCS sessions, and receipt-fault readback.

This lane records a source/binary digest rather than a container image digest;
no container image is published by this change. Consumers qualifying a container
must record its immutable image digest and rerun their owning acceptance tests.
