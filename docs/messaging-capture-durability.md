# Messaging capture publication and repeatability

The filesystem mail and SMS stores publish new captures in transactions. Mail
recipient copies, the `_all` copy, raw MIME sidecars, SMS canonical files,
conversation indexes, inline media, and provider repeatability records become
visible through one commit marker. Mail provider personalizations and ACS SMS
recipient batches use one transaction per request.

Before writing a destination, the store records a synced intent containing its
relative path and staging path. Each destination is staged, synced, and renamed.
The store then writes and syncs a commit marker. Readers take a snapshot of
committed transaction IDs for each list operation; individual gets reject a
message whose transaction has not committed. Filesystem store startup removes
all destinations belonging to incomplete transactions. A returned error before
the commit marker triggers the same rollback. If acknowledgement fails after
publication, the committed replay result remains the authority for a retry.

ACS Email claims a repeatability GUID and reserves the operation GUID under one
provider claim lock. ACS SMS claims a GUID per recipient within the provider
resource; the sender, recipient, message and options remain part of the request
fingerprint. GUID identity is case insensitive. Matching retries return the
original result, while changed fingerprints or first-sent values fail. Request
results and successful message captures commit together. Valid repeatable
operation errors persist without creating messages. Protocol, authentication,
and repeatability-header validation failures do not reserve a claim.

Successful replay records survive admin capture deletion, as do ACS Email
operation-ID reservations. Replaying a deleted capture returns its original
operation identity without recreating the capture. Email's existing five-minute
repeatability header check still applies before replay; SMS retains its existing
recipient field contract. Messages from older stores, which have no capture
transaction ID, remain readable, and legacy message metadata remains a lookup
fallback for earlier ACS operations.

New captures have a **64 MiB projected aggregate materialization limit**. The
borrowed admission pass counts rendered JSON, raw MIME and inline media,
retained payload/result copies, replay buffers and envelope reserves before
fan-out or filesystem mutation. SendGrid plans personalizations before cloning
shared content; ACS SMS admits each recipient before cloning its payload.
An over-limit request creates no capture, journal or repeatability reservation.
Below-limit batches retain the same atomic publication and replay behavior.

The adapters return provider-shaped HTTP 413 responses identifying the local
constraint; SMTP returns 552 and accepts a later valid transaction on the same
connection. SMTP also bounds necessarily oversized raw DATA before constructing
MIME/body copies. These are conservative local admission limits, separate from
the configured per-request cap and provider capacities. They do not establish
a measured process RSS bound. `tests/contract_capture_resources.rs` and selected
official email SDK tests cover rejected fan-out, no mutation and smaller retries.

The qualification boundary is process termination on a local filesystem with a
single owning emulator process. It does not establish correctness for two
processes opening the same persistence root, network filesystems, disk corruption,
or physical power loss. Callback attempts, delivery transitions, admin deletion,
and old persisted captures are outside this initial capture transaction. Custom
`MailStore` and `SmsStore` implementations may use the default rollback path;
crash publication is qualified only for the filesystem implementations.

`capture::tests` terminates child processes at 74 actual filesystem boundaries:
transaction preparation, durable intent, each staged file, each destination
rename, each directory synchronization, and commit publication. It checks
invisibility before reopening, rollback recovery, complete committed batches,
raw MIME/media content, and matching replay records. Returned-error injection
also checks cleanup independently. `tests/contract_messaging.rs` covers concurrent
retries behind start barriers, competing operation IDs, case insensitive GUIDs,
per-recipient claims, changed senders/bodies, restart, admin deletion, client-error
replay, and retry after a persistence failure.

Native repeatability references: [ACS repeatable requests](https://learn.microsoft.com/en-us/rest/api/communication/repeatable-requests)
and [ACS SMS Send](https://learn.microsoft.com/en-us/rest/api/communication/sms/sms/send?view=rest-communication-sms-2021-03-07).
