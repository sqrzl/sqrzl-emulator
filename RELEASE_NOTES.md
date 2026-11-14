# Release notes

## Configuration and admin reliability

Vendor settings now use `TWILIO_*`, `SENDGRID_API_KEY`, `ACS_CONNECTION_STRING`,
`AZURE_ENCRYPTION_SCOPES`, and `SES_CONFIGURATION_SETS`. Update deployments that
used the former SQRZL-prefixed vendor names. `SQRZL_*` remains reserved for emulator
controls. Incomplete generic or Twilio credentials, empty credentials and invalid
ACS connection strings fail startup. Lifecycle duration overflow is rejected.

SES simulations accept stable request IDs, persist intent before external delivery,
and retain pending outcomes across interruption without automatic callback resend.
Concurrent event destination updates no longer share temporary filenames.

The admin UI supports dotted deep-link reloads, open-mode entry, visible retry
actions and an unknown-route fallback. The generated client includes SES event
operations and is checked against OpenAPI in CI.

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
