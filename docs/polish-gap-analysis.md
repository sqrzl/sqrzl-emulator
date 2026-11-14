# Sqrzl polish and qualification gaps

Sqrzl implements a useful local subset of storage and messaging APIs. It has
transactional persistence, native error contracts, official SDK tests, and an
admin UI. It is not yet a fully qualified cloud emulator. A passing workflow
establishes its declared assertions; it does not establish provider parity.

This analysis and the fixes remain in PR #94. The source of truth for supported
boundaries is [the compatibility matrix](../compatibility-matrix.json). The
machine-readable test scope is [the acceptance manifest](../sdk-tests/acceptance-manifest.json).

## Current qualification boundary

The matrix contains 207 operation variants: 196 partial, 10 unsupported and
one deferred. None is certified. Across the ten proof categories, 1,956 gates
remain pending and 114 are not applicable. Pending means acceptance has not
been reviewed and established; it does not mean 1,956 demonstrated defects.

CI records exact source and binary identities, collected test results, explicit
per-operation SDK assertions, and measured upload receipts. The contract
validator rejects missing, stale, mismatched and uncollected evidence. It does
not automatically promote a family to certified. See [SDK qualification](sdk-qualification.md)
and [large-upload qualification](large-upload-qualification.md).

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
| SES history retained every event payload | At most `limit + 1` candidates retained while scanning; filter-bound stable cursors survive deletion of the previous boundary. API defaults to 50 and caps at 500. Archive scan I/O remains linear. |
| Google .NET default gzip was not qualified | Locked Google.Cloud.Storage.V1 5.0.0/.NET 11.0.100 test observes default gzip, bucket CMEK metadata, resumable initiation/completion, complete byte readback, missing-object response and cleanup. Anonymous/local-bearer CI lanes; not Google OAuth. |
| Admin direct links and error recovery were incomplete | Dotted routes, open-mode entry, visible retry/home actions and one main landmark; real browser journeys run in CI. |
| Native artifacts omitted the UI | Native bundles include built UI, manifest, version/source identity, payload/archive checksums and instructions. Asset discovery supports an executable launched from another directory. |
| Distribution could bypass qualification | Distribution checks require successful CI and automatic full measured campaigns for the exact commit. Narrow manual campaigns cannot qualify release. Both Linux architectures build and smoke natively; no cross-built executable is treated as smoke-tested. |

## Remaining engineering gaps

These are explicit follow-up boundaries, not closed acceptance claims.

| Priority | Gap and impact | Completion test |
| --- | --- | --- |
| P1 | Process signals do not yet coordinate listener shutdown and request/session draining. Normal storage-writer teardown waits for blocking writers, but default SIGTERM handling is not a graceful drain contract. | Real process tests send SIGTERM during active upload, callback and SMTP sessions; verify admission stops, acknowledged work survives restart, a bounded deadline applies, and root ownership remains held until writers finish. |
| P1 | Ordinary mailbox listings deserialize full stored messages and attachments before pagination; mailbox summaries can scan messages repeatedly. Cost grows with captured payload volume. | Metadata-only summaries/index with crash-consistent publication; large attachment fixtures prove bounded retained memory and bounded payload reads for first/next pages and mailbox counts. |
| P1 | Aggregate connection, request and background-work admission is not comprehensively qualified. Per-request byte limits alone do not bound process memory under concurrency. | Saturate API/UI/SMTP and callback workloads; measure aggregate RSS, spool bytes, queued jobs and recovery; prove native overload errors and cleanup without lost acknowledged work. |
| P1 | Operation-specific acceptance remains incomplete. Family smoke tests cannot prove native auth, errors, conditions, paging, crash recovery and resources for every variant. | Review each operation's exact assertions and artifacts; close individual proof categories only after positive and adversarial cases pass on the recorded binary. Keep unsupported variants explicit. |
| P2 | SES page scans still read every event record. The retained result is bounded, but archive latency and disk reads scale with archive size. | A durable metadata index or directory layout supporting bounded lookup, tie ordering, interrupted publication and backward-compatible recovery; count reads under a large archive. |
| P2 | Some callback/admission controls still read environment settings at runtime. Startup configuration is not yet a single immutable typed snapshot for every subsystem. | Table-driven startup validation and tests proving runtime requests do not observe later environment changes; retain provider-prefixed vendor settings. |
| P2 | Operational diagnostics do not yet provide a complete status/latency/active-job view or health checks for every storage/capture subsystem. | Inject per-subsystem failures; verify actionable health status and bounded-cardinality diagnostics without secrets or message contents. |
| P2 | Packaging is prepared and locally exercised; hosted native builds and current-head qualification remain separate required evidence. There is no claim that a GitHub release has been published. | Successful exact-head Linux x86_64/arm64 bundle builds, smoke receipts, downloadable checksum verification and deliberate release publication/readback. |

## Intentional emulator limits

Cloud IAM/control planes, real email/SMS delivery, actual cloud KMS encryption,
SNS certificate/subscription infrastructure and unsupported provider operations
remain outside the implemented model. Adding them requires explicit product
scope and independent qualification. Do not describe stored CMEK metadata as
real encryption or local bearer convenience as Google OAuth.

Jev is used for bounded adversarial candidates. Its probabilities are review
signals, not acceptance proof; source inspection and executable regressions
decide whether a candidate is a defect.
