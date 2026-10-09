# GCS resumable cancellation

Sessions initiated through the GCS JSON API return HTTP 499 on successful
`DELETE`. XML-initiated sessions return HTTP 204. The initiation protocol is
persisted, so this distinction survives reopening the adapter and filesystem
store. Cancellation removes the session record and staged chunks while
preserving any existing object at the destination.

Cancelled XML sessions retain a small durable decision containing the session
identifier, bucket name and creation identity, and expiration time. Future status
queries and resume attempts return HTTP 204 without accepting bytes or creating
an object. The decision is saved before active state and chunks are removed;
reopening storage finishes that cleanup if interruption occurred between these
steps. Looking up a cancellation also finishes pending cleanup.

The decision expires seven days after the original session initiation, matching
the local session lifetime. Expired decisions are removed when storage opens or
when that session is requested. Files in an idle store remain until the next open
or lookup; there is no background sweep or fixed count limit. Decisions live
outside bucket object directories, so they do not prevent deleting an empty
bucket. Bucket deletion or recreation invalidates the decision; startup and
lookup remove it, and the old session URI returns HTTP 404.

Older persisted sessions did not record whether JSON or XML initiated them.
Their cancellation retains the existing HTTP 204 response, and later requests
return HTTP 404. New JSON sessions return HTTP 499 on cancellation and HTTP 404
afterward. Unknown session identifiers continue to return HTTP 404.

The native status contract is documented in Google's
[resumable upload cancellation reference](https://docs.cloud.google.com/storage/docs/performing-resumable-uploads#cancel-upload).
