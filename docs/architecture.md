# Sqrzl Architecture

This document maps the current Sqrzl emulator architecture as Mermaid diagrams.
It is source-oriented: the nodes name the modules, contracts, ports, and runtime
paths that exist in this repository.

## System Context

```mermaid
flowchart LR
    subgraph Clients
        S3SDK[S3 SDKs and CLI tools]
        AzureSDK[Azure Blob SDKs]
        GCSSDK[Google Cloud Storage SDKs]
        OCISDK[OCI Object Storage SDKs]
        Browser[Browser admin UI]
        DevOps[Local dev, CI, Docker Compose]
    end

    subgraph Sqrzl["sqrzl-emulator process"]
        ApiPort["Provider API listener<br/>0.0.0.0:9000"]
        UiPort["UI and admin listener<br/>0.0.0.0:9001"]
        SharedStorage["Shared Storage trait object<br/>focused capability traits + FilesystemStorage"]
        Lifecycle["LifecycleExecutor<br/>background task"]
    end

    Disk[Filesystem blob root<br/>SQRZL_BLOBS_PATH]
    OpenApi[Admin OpenAPI contract<br/>public/openapi.yml]
    StaticUi[Built SPA assets<br/>./static or /app/ui/dist]

    S3SDK --> ApiPort
    AzureSDK --> ApiPort
    GCSSDK --> ApiPort
    OCISDK --> ApiPort
    Browser --> UiPort
    DevOps --> ApiPort
    DevOps --> UiPort

    ApiPort --> SharedStorage
    UiPort --> SharedStorage
    Lifecycle --> SharedStorage
    SharedStorage --> Disk
    UiPort --> StaticUi
    OpenApi --> Browser
```

## Runtime Composition

```mermaid
flowchart TB
    Main["src/main.rs"]
    Config["Config::from_env<br/>src/config.rs"]
    Logging["tracing subscriber<br/>text or json"]
    StartupBuckets["SQRZL_BUCKET_LIST validation<br/>ensure_startup_buckets"]
    Storage["Arc&lt;FilesystemStorage&gt;<br/>src/storage/filesystem.rs"]
    Lifecycle["LifecycleExecutor::start<br/>src/lifecycle.rs"]
    ProviderServer["Server::new(...).start<br/>src/server/mod.rs"]
    UiServer["start_ui_server<br/>src/api/server.rs"]

    Main --> Config
    Main --> Logging
    Main --> Storage
    Main --> StartupBuckets
    StartupBuckets --> Storage
    Main --> Lifecycle
    Main --> ProviderServer
    Main --> UiServer

    Config --> ProviderServer
    Config --> UiServer
    Config --> Lifecycle
    Storage --> ProviderServer
    Storage --> UiServer
    Storage --> Lifecycle
```

## Provider API Request Path

```mermaid
flowchart TD
    Request["HTTP request on API port"]
    Parse["Request::from_hyper_with_max_body<br/>src/server/http.rs"]
    BodyLimit{"Body exceeds<br/>SQRZL_MAX_REQUEST_BYTES?"}
    Health{"GET /healthz?"}
    Registry["AdapterRegistry::handle<br/>src/providers/mod.rs"]
    Azure{"AzureBlobAdapter.matches"}
    GCS{"GcsAdapter.matches"}
    OCI{"OciAdapter.matches"}
    S3["S3Adapter fallback"]

    AzureHandle["Azure Blob handler<br/>headers, SharedKey, block sessions"]
    GcsHandle["GCS handler<br/>JSON XML APIs, signed URLs, resumable sessions"]
    OciHandle["OCI handler<br/>/n namespace paths and Signature auth"]
    S3Handle["S3 handlers<br/>Router, bucket/object handlers"]

    ProviderAuth["Provider-specific auth<br/>SharedKey, GOOG1, OCI Signature"]
    S3Auth["S3 auth facade<br/>check_authorization"]
    S3SigV4["S3 auth internals<br/>sigv4 + context helpers"]
    ProviderStateHelper["Provider state helper<br/>providers::state JSON sidecars"]
    BlobBackend["BlobBackend facade<br/>namespace/blob vocabulary"]
    Services["S3 service helpers<br/>src/services/bucket.rs<br/>src/services/object.rs"]
    Storage["Storage trait<br/>src/storage/mod.rs"]
    Response["Provider-compatible response<br/>XML, JSON, headers"]
    TooLarge["AdapterRegistry::render_payload_too_large<br/>provider-shaped 413 response"]
    HealthResponse["health::response"]

    Request --> Parse
    Parse --> BodyLimit
    BodyLimit -- yes --> TooLarge
    BodyLimit -- no --> Health
    Health -- yes --> HealthResponse
    Health -- no --> Registry
    Registry --> Azure
    Azure -- match --> AzureHandle
    Azure -- no --> GCS
    GCS -- match --> GcsHandle
    GCS -- no --> OCI
    OCI -- match --> OciHandle
    OCI -- no --> S3
    S3 --> S3Handle

    AzureHandle --> ProviderAuth
    GcsHandle --> ProviderAuth
    OciHandle --> ProviderAuth
    AzureHandle --> ProviderStateHelper
    GcsHandle --> ProviderStateHelper
    S3Handle --> S3Auth --> S3SigV4
    ProviderAuth --> BlobBackend
    S3Auth --> Services
    ProviderStateHelper --> Storage
    BlobBackend --> Storage
    Services --> Storage
    Storage --> Response
```

## S3 Handler Breakdown

```mermaid
flowchart LR
    Req["Parsed Request"]
    Router["Router::route<br/>path-style and virtual-hosted-style"]
    Route{"RouteMatch"}
    ListBuckets["list_buckets"]
    BucketOps["bucket_get_or_list_objects<br/>bucket_put<br/>bucket_delete<br/>bucket_head<br/>bucket_post"]
    ObjectOps["object_get<br/>object_put<br/>object_delete<br/>object_head<br/>object_post"]
    Authz["check_authorization<br/>SigV4, AuthInfo, ACL, bucket policy"]
    CORS["CORS helpers"]
    LifecycleCheck["check_object_expiration<br/>eager object expiration"]
    BucketService["services::bucket"]
    ObjectService["services::object"]
    Xml["utils::xml<br/>S3 XML responses"]
    Store["dyn Storage"]

    Req --> Router --> Route
    Route --> ListBuckets
    Route --> BucketOps
    Route --> ObjectOps
    ListBuckets --> Authz
    BucketOps --> Authz
    ObjectOps --> Authz
    BucketOps --> CORS
    ObjectOps --> CORS
    ObjectOps --> LifecycleCheck
    ListBuckets --> BucketService
    BucketOps --> BucketService
    ObjectOps --> ObjectService
    BucketService --> Store
    ObjectService --> Store
    ListBuckets --> Xml
    BucketOps --> Xml
    ObjectOps --> Xml
```

## Admin UI And Admin API

```mermaid
flowchart TB
    Browser["Browser"]
    Askr["Askr SPA<br/>ui/src/main.tsx"]
    Routes["Route manifest and auth guards<br/>ui/src/pages/_routes.tsx"]
    Features["Feature query modules<br/>auth, buckets, objects"]
    GeneratedClient["Generated OpenAPI adapter<br/>ui/src/adapters/api.g.ts"]
    FetchClient["@fgrzl/fetch<br/>credentials: same-origin"]

    UiListener["UI listener :9001<br/>src/api/server.rs"]
    Static["Static asset serving<br/>./static or /app/ui/dist"]
    SessionRoutes["/admin/v1/auth/login<br/>/logout<br/>/session"]
    AdminRoutes["/admin/v1/buckets...<br/>/objects...<br/>/versioning, ACL, policy, lifecycle"]
    SessionManager["AdminSessionManager<br/>HttpOnly JWT cookie"]
    AdminHandler["api::admin::handle_request facade"]
    AdminInternals["private admin modules<br/>pagination + DTO mapping"]
    Services["services::bucket<br/>services::object"]
    Storage["dyn Storage"]
    OpenApi["public/openapi.yml"]

    OpenApi --> GeneratedClient
    Browser --> Askr --> Routes --> Features --> GeneratedClient --> FetchClient
    FetchClient --> UiListener
    UiListener --> Static
    UiListener --> SessionRoutes
    UiListener --> AdminRoutes
    SessionRoutes --> SessionManager
    AdminRoutes --> SessionManager
    AdminRoutes --> AdminHandler --> AdminInternals
    AdminHandler --> Services
    Services --> Storage
```

## Admin API Surface

```mermaid
flowchart LR
    AdminRoot["/admin/v1"]
    Auth["auth<br/>login, logout, session"]
    Buckets["buckets<br/>list, create, get, delete"]
    BucketControls["bucket controls<br/>versioning, ACL, policy, lifecycle"]
    Multipart["multipart uploads<br/>list, get, abort"]
    Objects["objects<br/>list, metadata, upload, download, delete"]
    ObjectControls["object controls<br/>versions, tags, ACL"]
    AdminHandler["api::admin::handle_request facade"]
    AdminInternals["private modules<br/>pagination + DTO mapping"]
    Services["services::bucket<br/>services::object"]
    JsonModels["api::models<br/>JSON response DTOs"]
    Storage["dyn Storage"]

    AdminRoot --> Auth
    AdminRoot --> Buckets
    Buckets --> BucketControls
    Buckets --> Multipart
    Buckets --> Objects
    Objects --> ObjectControls
    Auth --> JsonModels
    Buckets --> JsonModels
    BucketControls --> JsonModels
    Multipart --> JsonModels
    Objects --> JsonModels
    ObjectControls --> JsonModels
    Auth --> AdminHandler
    Buckets --> AdminHandler
    BucketControls --> AdminHandler
    Multipart --> AdminHandler
    Objects --> AdminHandler
    ObjectControls --> AdminHandler
    AdminHandler --> AdminInternals
    AdminHandler --> Services
    Services --> Storage
```

## Storage Backend And Disk Layout

```mermaid
flowchart TB
    StorageAggregate["Storage aggregate trait<br/>stable dyn Storage facade"]
    CapabilityTraits["Focused capability traits<br/>BucketStore, ObjectStore, ObjectListingStore<br/>MultipartStore, VersionStore, TagStore<br/>AclStore, LifecycleStore, PolicyStore, ProviderStateStore"]
    BlobBackend["BlobBackend adapter trait<br/>namespace/blob vocabulary for Azure, GCS, OCI"]
    FS["FilesystemStorage"]
    Indexed["IndexedStorage wrapper<br/>BTreeSet keys + delimiter delegation"]
    Index["LockFreeIndex<br/>object keys and immediate directory children"]
    ObjectLocks["Per-object Mutex registry"]
    UploadCache["Multipart uploads cache"]
    AtomicWrite["atomic_write temp file then rename"]

    Root["SQRZL_BLOBS_PATH"]
    BucketDir["bucket directory"]
    BucketControl["bucket sidecars<br/>.bucket.meta.json<br/>.versioning-enabled<br/>.lifecycle.json<br/>.policy.json<br/>bucket.acl.json"]
    ObjectDir["hashed object_id directory"]
    ObjectBlob["object.blob"]
    ObjectMeta["object.meta.json"]
    Versions["versions/{version_id}<br/>object.blob + object.meta.json"]
    Multipart[".multipart/{upload_id}<br/>upload.json + part files"]
    ProviderState[".provider-state/{provider}<br/>restart-safe sidecars"]

    StorageAggregate --> CapabilityTraits --> FS
    CapabilityTraits --> Indexed
    BlobBackend --> StorageAggregate
    FS --> Index
    FS --> ObjectLocks
    FS --> UploadCache
    FS --> AtomicWrite
    FS --> Root
    Root --> BucketDir
    BucketDir --> BucketControl
    BucketDir --> ObjectDir
    ObjectDir --> ObjectBlob
    ObjectDir --> ObjectMeta
    ObjectDir --> Versions
    BucketDir --> Multipart
    Root --> ProviderState
```

## Auth And Authorization

```mermaid
flowchart TD
    Config["Config<br/>SQRZL_ACCESS_KEY_ID<br/>SQRZL_SECRET_ACCESS_KEY<br/>SQRZL_ADMIN_AUTH_DISABLED"]
    ProviderRequest["Provider API request"]
    AdminRequest["Admin UI/API request"]

    ProviderAuth{"Provider auth disabled?"}
    ProviderSpecific["Provider-specific verification<br/>S3 SigV4/v2/presigned<br/>Azure SharedKey<br/>GCS GOOG1 or signed URL<br/>OCI Signature"]
    AuthInfo["AuthInfo principal"]
    PolicyAcl["S3 authorization facade<br/>bucket policy + ACL + owner fallback"]
    AuthModules["private S3 auth modules<br/>sigv4 + context"]
    ProviderAllow["Continue provider operation"]
    ProviderDeny["Provider-compatible 401 or 403"]

    AdminBypass{"Admin auth disabled<br/>or provider auth disabled?"}
    AdminPath{"Admin request type"}
    Login["POST /admin/v1/auth/login<br/>validate configured credentials"]
    Cookie["HttpOnly sqrzl_admin_session<br/>HS256 JWT, 8 hour TTL"]
    SessionCheck["Session cookie validation"]
    AdminAllow["Continue admin operation"]
    AdminDeny["JSON 401 Unauthorized"]

    Config --> ProviderAuth
    Config --> AdminBypass
    ProviderRequest --> ProviderAuth
    ProviderAuth -- yes --> ProviderAllow
    ProviderAuth -- no --> ProviderSpecific
    ProviderSpecific --> AuthInfo
    AuthInfo --> PolicyAcl --> AuthModules
    PolicyAcl -- allow --> ProviderAllow
    PolicyAcl -- deny --> ProviderDeny

    AdminRequest --> AdminBypass
    AdminBypass -- yes --> AdminAllow
    AdminBypass -- no --> AdminPath
    AdminPath -- login --> Login
    AdminPath -- protected route --> SessionCheck
    AdminPath -- session check --> SessionCheck
    Login --> Cookie
    Cookie --> SessionCheck
    SessionCheck -- valid --> AdminAllow
    SessionCheck -- missing or invalid --> AdminDeny
```

## Lifecycle Enforcement

```mermaid
flowchart LR
    Config["SQRZL_LIFECYCLE_HOURS"]
    Executor["LifecycleExecutor background loop"]
    ListBuckets["Storage::list_buckets"]
    BucketLifecycle["Storage::get_bucket_lifecycle"]
    Rules["Enabled lifecycle rules<br/>filters, expiration, transitions, noncurrent versions"]
    Current["Current object actions<br/>delete or storage class transition"]
    Noncurrent["Noncurrent version expiration"]
    Storage["Storage updates"]
    Eager["S3 object GET eager check<br/>check_object_expiration"]

    Config --> Executor
    Executor --> ListBuckets --> BucketLifecycle --> Rules
    Rules --> Current --> Storage
    Rules --> Noncurrent --> Storage
    Eager --> BucketLifecycle
    Eager --> Current
```

## Deployment Shape

```mermaid
flowchart TB
    Cargo["cargo build --bin sqrzl-emulator"]
    Dockerfile["Dockerfile"]
    Compose["compose.yml"]
    Container["sqrzl container"]
    Ports["9000 provider API<br/>9001 UI/admin"]
    Volume["sqrzl-blobs volume<br/>/app/blobs"]
    Env["Runtime env<br/>credentials, ports, blobs path, request limit, log format"]

    Cargo --> Dockerfile
    Dockerfile --> Container
    Compose --> Container
    Env --> Container
    Container --> Ports
    Container --> Volume
```

## Verification Architecture

```mermaid
flowchart LR
    Unit["Rust unit tests<br/>module-level storage, auth, routing, helpers"]
    E2E["Rust e2e and interop tests<br/>tests/e2e_*.rs<br/>tests/interop_*.rs"]
    SDK["Python SDK certification<br/>sdk-tests/test_*_sdk.py"]
    Benches["cntryl-stress tiers and artifacts<br/>tier1 hotpath<br/>tier2 subsystem<br/>tier3 system/admin<br/>tier4 provider integration"]
    LiveServer["LiveServer harness<br/>ephemeral ports and temp storage"]
    Emulator["sqrzl-emulator runtime"]
    Matrix["compatibility-matrix.json<br/>support-certification.md"]

    Unit --> Emulator
    E2E --> LiveServer --> Emulator
    SDK --> Emulator
    Benches --> LiveServer
    Benches --> Emulator
    SDK --> Matrix
    E2E --> Matrix
```
