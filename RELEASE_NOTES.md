# Release notes

## Concurrent object reads

Within one emulator instance, object GET, HEAD, range, and version reads now
synchronize with writes and deletes. Successful payload responses keep their
bytes, size, ETag, range headers, and read conditions on one object generation,
including streamed replacements.

## Storage format v2

This release intentionally changes the on-disk blob layout. Bucket directory
identities are generated from logical names, so request-controlled bucket names
are never joined directly into filesystem paths.

There is no automatic migration. If `SQRZL_BLOBS_PATH` is nonempty and does not
contain `.sqrzl-storage-format-v2`, Sqrzl exits with reset instructions and
leaves every existing file untouched. Archive the old directory or clear it,
then restart.

Provider compatibility claims affected by the contract audit are temporarily
demoted to partial. Certification will be restored only after authenticated
official-SDK, negative-contract, pagination, restart-durability, and
provider-error gates pass.
