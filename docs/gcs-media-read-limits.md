# GCS media reads

The local GCS subset materializes at most 64 MiB for one media request. This
applies to the JSON `/download/storage/v1/...` route used by the official SDK,
the JSON object route with `alt=media`, and XML object GET. Full requests and
selected ranges above that limit return HTTP 501 before reading payload bytes
when the admitted object already exceeds the bound. JSON errors use
`notImplemented`; XML errors use `NotImplemented`.

Small ranges of larger objects remain supported. A range read returns metadata
and bytes from one storage generation, and its allocation is capped even if a
concurrent library writer replaces the object after metadata admission. JSON
generation and metageneration conditions are checked before reading bytes and
again against the returned generation. Responses never return a successful
truncated full object. Empty objects return an empty HTTP 200 response.

JSON metadata GET remains independent of payload size. Current generation
selectors are supported; historical GCS generation selection remains an
explicit HTTP 501 boundary. The shared S3 version APIs do not establish native
GCS history ownership. The existing single absolute or open-ended byte-range
subset is retained; invalid ranges use HTTP 416 with JSON
`requestedRangeNotSatisfiable` or XML `InvalidRange`.

The 64 MiB limit is an emulator resource policy, separate from Google's native
object-size limit. Streamed media, multipart and resumable uploads retain their
existing limits. The native range and conditional request contracts are
documented in Google's [Objects: get reference](https://docs.cloud.google.com/storage/docs/json_api/v1/objects/get)
and [JSON status codes](https://docs.cloud.google.com/storage/docs/json_api/v1/status-codes).
