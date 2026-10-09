# GCS resumable cancellation

Sessions initiated through the GCS JSON API return HTTP 499 on successful
`DELETE`. XML-initiated sessions return HTTP 204. The initiation protocol is
persisted, so this distinction survives reopening the adapter and filesystem
store. Cancellation removes the session record and staged chunks while
preserving any existing object at the destination.

Older persisted sessions did not record whether JSON or XML initiated them.
Their cancellation retains the existing HTTP 204 response. A new session
records its origin and receives the protocol-specific response.

Subsequent requests to a cancelled session receive HTTP 404 in the current local
model. Google documents a 4xx response for JSON sessions and HTTP 204 for future
XML queries or resume attempts; the XML post-cancellation response remains an
explicit compatibility gap.

The native status contract is documented in Google's
[resumable upload cancellation reference](https://docs.cloud.google.com/storage/docs/performing-resumable-uploads#cancel-upload).
