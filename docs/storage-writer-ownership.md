# Storage writer ownership

One emulator process owns one `SQRZL_BLOBS_PATH`. Startup claims an exclusive OS
lock on `.sqrzl-writer.lock` before opening storage, recovering records, or
starting listeners. A second process using the same root exits with an actionable
error. The guard outlives the complete Tokio runtime, including blocking writers
still finishing after listener failure. Different roots can run concurrently. Terminating the owner releases the
lock; its file remains so subsequent owners use the same inode. Never delete that
file while the emulator is running. Unmarked legacy data is rejected before the
lock file is created.

This is a cooperative local-filesystem ownership contract. External programs that
modify files without claiming the lock, network filesystem lock behavior, and
multiple concurrent writer processes are outside the supported subset.

## Protection decisions

The provider adapter registry, storage admin API, and background lifecycle passes
share a storage operation gate. A handler owns it from state observation through
publication. Provider failure injection before dispatch and after publication is
outside the gate. Receiving/spooling request bytes is also outside it; bucket and
object existence are checked after dispatch acquires ownership. Operations are
serialized for correctness; this is not a throughput or concurrent cloud-service
capacity qualification.

The same gate covers lease, retention and hold activation, deletes, copy and
multipart completion, bucket changes, and admin mutations. The indexed storage
wrapper forwards its underlying store's gate and authoritative metadata listings;
it never authorizes protection or existence from a stale private key cache. Lifecycle expiration skips active
leases, holds, retention, and incomplete protection metadata. Admin content
replacement/deletion and version purge reject protected current or historical
data with JSON `AccessDenied`; admin writes to GCS/Azure protected bucket modes
are also rejected. There is no admin force-delete or lease-ID override.

An active Azure object lease or WORM policy reserves its shared bucket namespace
against S3/GCS/OCI mutations, which return their native conflict envelopes. Those
front doors do not implement Azure lease authorization or version ownership.
Releasing the protection through the Azure front door restores ordinary shared
namespace access. The underlying filesystem store proves absence of Azure object
protection during its complete startup metadata pass, after publication recovery.
Incomplete or unreadable current metadata fails startup. An empty new bucket can
also establish absence, without clearing a concurrent protection admission.

If creation stops before publishing `.bucket.name`, startup removes the
uncommitted bucket directory only when it is empty or contains solely regular
bucket-name temporary files with recognized UUID names. Recovery syncs the root
after removal, so creation can be retried. A missing name alongside a published
identity, object, history, upload, symlink, or unknown entry still fails startup
and preserves the directory's contents.

Ordinary S3/GCS requests use this proof to avoid scanning unrelated objects.
Before publishing any current protection metadata, replacing it through metadata
CAS, or promoting a protected historical version, the store invalidates the proof.
Invalidation survives failed or ambiguous writes, object deletion, bucket reuse,
and protection release for the lifetime of that store. These buckets use the
existing paginated metadata check, which interprets active, expired, released and
malformed attributes without loading payloads. Reopening reconstructs the proof
from recovered current metadata. Wrappers forward this authority to their inner
store. Native Azure mutations continue to enforce their lease IDs and retention
rules.

## Library embedding

Acquire `StorageRootWriter` before opening any filesystem store and retain it
until all listeners and background writers stop. Use `AdapterRegistry::handle`,
the admin handler, and `LifecycleExecutor::run_once` for coordinated operations.
Callers composing raw storage methods or directly invoking an individual adapter
must hold `storage.operation_gate()` over the complete decision and mutation.
Low-level storage methods intentionally do not reacquire the gate; per-object
locks continue to protect coherent reads and conditional storage writes.

## Qualification

`storage_root_ownership` launches real emulator subprocesses and verifies
same-root rejection, distinct-root concurrency, lock release after termination,
and unchanged legacy data. `operation_coordination` pauses a decision owner,
queues native/admin/lifecycle mutations, commits protection, and checks the
waiting operation observes it. It also verifies native conflicts preserve leased
bytes across S3/GCS/OCI, protected admin mutations preserve data, and bucket
deletion cannot consume an active multipart upload.

Run these gates with:

```sh
cargo test --test storage_root_ownership --test operation_coordination
cargo test --lib storage::filesystem::protection_tests
cargo test --lib storage::filesystem::bucket_recovery_tests
```

The protection regression counts complete S3, GCS XML and GCS JSON object/range
requests through `IndexedStorage`, checks exact status and bytes, and repeats
after reopening with unrelated objects present. Controls cover real Azure
activation/release, malformed and expired attributes, ambiguous publications,
failed metadata CAS, history promotion after reopening, incomplete startup,
late bucket initialization, and process termination around publication decisions.
These checks establish bounded metadata work and protection correctness. They
do not establish the cause or resolution of downstream full-window watchdog
failures; unchanged downstream qualification remains separate evidence.

The bucket recovery campaign terminates real creator processes after durable
directory creation, before the name rename, after that rename, and after the
identity sidecar is published. Repeated reopening preserves surviving bytes and
Azure protection, recovers uncommitted buckets for retry, and retains committed
empty buckets. Missing-name controls preserve populated or ambiguous directories.
