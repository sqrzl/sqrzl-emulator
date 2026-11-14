# Sqrzl polish and qualification gaps

Sqrzl implements a useful local subset of storage and messaging APIs. It has
transactional persistence, native error contracts, official SDK tests, and an
admin UI. It is not yet a fully qualified cloud emulator. A passing workflow
establishes its declared assertions; it does not establish provider parity.

The original polish fixes shipped in PR #94. The runtime and certification
completion builds on that merged work. The source of truth for supported
boundaries is [the compatibility matrix](../compatibility-matrix.json). The
machine-readable test scope is [the acceptance manifest](../sdk-tests/acceptance-manifest.json).

## Current qualification boundary

The matrix contains 207 operation variants: 196 partial, 10 unsupported and one
deferred. Broad tiers remain unchanged. The versioned `local-core-v1` policy
selects nine narrower operation/selector scopes: six plaintext SMTP command
contracts and current-object HEAD for selected S3, Azure and OCI API versions.
Certification is established only by the exact-source acceptance artifact;
matrix links alone are eligibility. Everything outside those scopes retains its
partial, unsupported or deferred status.

CI records source/binary identities, pinned SDK versions, operation assertion
receipts and measured upload results. Its required certification gate rejects
missing categories, unscoped candidates, selector mismatch, anonymous native-auth
proof, skipped results and dirty or upgrade-candidate builds. See
[support qualification](support-certification.md) for boundaries and commands.

## Addressed in PR #94

| Gap | Result and regression evidence |
| --- | --- |
| Copy requests could materialize oversized sources | Metadata-first size bounds and bounded reads; regression checks cover rejection before payload materialization. |
| GCS compose accepted malformed preconditions | Invalid shapes fail before mutation; regression covers preserved destination state. |
| GCS gzip omitted subsequent members | Multi-member decoding applies the decompressed-size cap across the entire request. |
| Vendor settings used emulator prefixes | Twilio, SendGrid, ACS, Azure scope and SES configuration settings use provider prefixes; `SQRZL_*` remains emulator configuration. |
| Startup accepted partial credentials and lifecycle overflow | Typed startup validation fails before storage creation; process tests check the named setting and untouched root. Health reports actual configured native authentication. |
| SES destination updates collided | Unique synced temporary files and coordinated destination writes; independent-handle concurrency regressions. |
| SES simulation could deliver before durable intent | Create-only durable intent claim precedes mutation/callback. Stable request IDs detect changed payloads and replay without resending an uncertain callback. Case-distinct IDs remain distinct on case-insensitive filesystems. |
| Pending SES state was invisible | API and UI expose pending intent/callback state. Lost-response retry retains the original request payload and identity. |
| SES admin publishing was absent | Real destination editing/removal, seven event types, simulation, callback details, payloads and paginated history. Component tests and desktop/mobile real-server callback journeys. |
| SES history retained every event payload | Transactional B-tree payload/global/message indexes, 8 MiB cache and bounded range pages; filtered pages decode selected records rather than the archive. Atomic-commit crash tests and real persistence-failure callback replay checks. |
| Google .NET default gzip was not qualified | Locked Google.Cloud.Storage.V1 5.0.0/.NET 11.0.100 test observes default gzip, bucket CMEK metadata, resumable initiation/completion, complete byte readback, missing-object response and cleanup. Anonymous/local-bearer CI lanes; not Google OAuth. |
| Admin direct links and error recovery were incomplete | Dotted routes, open-mode entry, visible retry/home actions and one main landmark; real browser journeys run in CI. |
| Native artifacts omitted the UI | Native bundles include built UI, manifest, version/source identity, payload/archive checksums and instructions. Asset discovery supports an executable launched from another directory. |
| Distribution could bypass qualification | Distribution checks require successful CI and automatic full measured campaigns for the exact commit. Narrow manual campaigns cannot qualify release. Both Linux architectures build and smoke natively; no cross-built executable is treated as smoke-tested. |

## Runtime and resource completion

| Area | Implemented boundary and regression evidence |
| --- | --- |
| Graceful draining | Shared admission closure and one deadline across owned API/UI/SMTP connections and MCP workers. Real process tests cover idle connections, upload ownership, SMTP DATA, SES callbacks, stalled clients and acknowledged mail after SIGKILL. Blocking filesystem writes retain ownership until completion, so an OS-blocked write can extend exit. |
| Aggregate admission | Configurable shared connection, request and encoded-byte reservations, nonqueued overload replies, bounded retained MCP sessions and SMTP recipients. A separate single probe slot preserves diagnostics during request saturation. Body EOF/drop and worker lifetime tests protect reservation refund. These are admission bounds, not a hard OS RSS cap. |
| Mail listing resources | Metadata-only summaries and index records publish in the capture transaction; all 26 publication boundaries receive crash recovery checks. Deleted-boundary cursors remain stable. Corrupt-payload regressions prove listing/counts do not read captured attachments. Delivery status updates only rewrite metadata. |
| SES history resources | Durable redb index commits intent, payload and both indexes together. Range reads, migration, concurrent claims, before/after-commit interruption and final-outcome I/O failure have focused regressions. |
| Diagnostics | Blob/mail/SMS readability and drain state determine health; fixed status/duration counters, active connections/requests/probes/MCP sessions, reserved bytes and rejection totals are exposed. Arbitrary operation-name flood cannot grow usage metrics beyond the global cap. Readability is not a free-space or write-readiness guarantee. |
| Operation certification | Reviewed `local-core-v1` gates cover nine selected scopes, with authenticated storage SDK receipts and explicit applicability reasons. All other operation/selector combinations retain their existing status. Jev review remains advisory; executable assertions establish acceptance. |

## Explicit boundaries retained

Mailbox index scans remain linear; substring search can rescan batches. Public
bulk library methods remain bulk operations. Admission reservations cover encoded
input, while decoding, caches and provider fanout add memory; a hard RSS/disk
boundary belongs in the container/runtime configuration. Existing callback policy
settings are not all consolidated into a single immutable startup snapshot.
Power-loss survival, universal operation certification and cloud parity are not
claimed. These boundaries do not weaken the selected certification policy.

Native bundle construction and release qualification are distinct from publishing
a GitHub release. Release checks still require successful exact-commit CI and
all automatic measured provider campaigns. This work does not publish a release.

## Intentional emulator limits

Cloud IAM/control planes, real email/SMS delivery, actual cloud KMS encryption,
SNS certificate/subscription infrastructure and unsupported provider operations
remain outside the implemented model. Adding them requires explicit product
scope and independent qualification. Do not describe stored CMEK metadata as
real encryption or local bearer convenience as Google OAuth.

Jev is used for bounded adversarial candidates. Its probabilities are review
signals, not acceptance proof; source inspection and executable regressions
decide whether a candidate is a defect.
