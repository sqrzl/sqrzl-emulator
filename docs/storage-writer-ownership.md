# Storage writer ownership

One emulator process owns one `SQRZL_BLOBS_PATH`. Startup claims an exclusive OS
lock on `.sqrzl-writer.lock` before opening storage, recovering records, or
starting listeners. A second process using the same root exits with an actionable
error. Different roots can run concurrently. Terminating the owner releases the
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
wrapper forwards its underlying store's gate. Lifecycle expiration skips active
leases, holds, retention, and incomplete protection metadata. Admin content
replacement/deletion and version purge reject protected current or historical
data with JSON `AccessDenied`; admin writes to GCS/Azure protected bucket modes
are also rejected. There is no admin force-delete or lease-ID override.

An active Azure object lease or WORM policy reserves its shared bucket namespace
against S3/GCS/OCI mutations, which return their native conflict envelopes. Those
front doors do not implement Azure lease authorization or version ownership.
Releasing the protection through the Azure front door restores ordinary shared
namespace access. Namespace checks page through object metadata without loading
payload bytes. Native Azure mutations continue to enforce their lease IDs and
retention rules.

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
```
