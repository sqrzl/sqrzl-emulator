use bytes::Bytes;
use cntryl_stress::prelude::*;
use http_body_util::BodyExt;
use hyper::{Method, Request};
use sqrzl_emulator::body::Body;
use sqrzl_emulator::models::Object;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::runtime::{Builder, Runtime};
use uuid::Uuid;

const ADMIN_BUCKET: &str = "bench-objects";
const ADMIN_BATCH_OPS: u64 = 8;
static BUCKET_LISTING_STORAGE: OnceLock<Arc<dyn Storage>> = OnceLock::new();
static OBJECT_BROWSING_STORAGE: OnceLock<Arc<dyn Storage>> = OnceLock::new();

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("sqrzl_bench_tier3_admin_{}", Uuid::new_v4()))
}

fn admin_get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::from(Bytes::new()))
        .expect("admin request should build")
}

async fn admin_response_len(storage: Arc<dyn Storage>, uri: &str) -> usize {
    let response = sqrzl_emulator::api::admin::handle_request(storage, admin_get(uri))
        .await
        .expect("admin request should succeed");
    assert!(
        response.status().is_success(),
        "expected successful admin response for {uri}, got {}",
        response.status()
    );
    response
        .into_body()
        .collect()
        .await
        .expect("admin response body should collect")
        .to_bytes()
        .len()
}

fn measure_admin_response_len_workload(
    ctx: &mut StressContext,
    runtime: &Runtime,
    storage: &Arc<dyn Storage>,
    uri: &str,
) -> u64 {
    ctx.parameter("operations_per_batch", ADMIN_BATCH_OPS);
    let completed = ctx.measure_batch(ADMIN_BATCH_OPS, || {
        for _ in 0..ADMIN_BATCH_OPS {
            let len = runtime.block_on(admin_response_len(storage.clone(), uri));
            black_box(len);
        }
    });
    let _ = ctx.correctness().attempted(completed).completed(completed);
    completed
}

fn put_text(storage: &Arc<dyn Storage>, bucket: &str, key: String, payload: &[u8]) {
    storage
        .put_object(
            bucket,
            key.clone(),
            Object::new(key, payload.to_vec(), "text/plain".to_string()),
        )
        .expect("seed put should succeed");
}

fn bucket_listing_storage() -> Arc<dyn Storage> {
    BUCKET_LISTING_STORAGE
        .get_or_init(|| seed_bucket_listing_storage(500))
        .clone()
}

fn object_browsing_storage() -> Arc<dyn Storage> {
    OBJECT_BROWSING_STORAGE
        .get_or_init(seed_object_browsing_storage)
        .clone()
}

fn seed_bucket_listing_storage(bucket_count: usize) -> Arc<dyn Storage> {
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));

    for index in 0..bucket_count {
        storage
            .create_bucket(format!("bucket-{index:03}"))
            .expect("seed bucket create should succeed");
    }

    storage
}

fn seed_object_browsing_storage() -> Arc<dyn Storage> {
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
    storage
        .create_bucket(ADMIN_BUCKET.to_string())
        .expect("seed bucket create should succeed");

    let payload = vec![b'a'; 256];

    for index in 0..1_000usize {
        put_text(
            &storage,
            ADMIN_BUCKET,
            format!("flat/object-{index:04}.txt"),
            &payload,
        );
    }

    for dir_index in 0..100usize {
        for object_index in 0..10usize {
            put_text(
                &storage,
                ADMIN_BUCKET,
                format!("dir-{dir_index:03}/item-{object_index:03}.txt"),
                &payload,
            );
        }
    }

    for index in 0..1_000usize {
        put_text(
            &storage,
            ADMIN_BUCKET,
            format!("skew/a/blob-{index:04}.txt"),
            &payload,
        );
    }
    put_text(
        &storage,
        ADMIN_BUCKET,
        "skew/z/blob.txt".to_string(),
        &payload,
    );

    storage
        .enable_versioning(ADMIN_BUCKET)
        .expect("versioning should enable");
    for version in 0..40usize {
        put_text(
            &storage,
            ADMIN_BUCKET,
            "versioned-target.txt".to_string(),
            format!("target-version-{version:03}").as_bytes(),
        );
    }

    for key_index in 0..100usize {
        for version in 0..3usize {
            put_text(
                &storage,
                ADMIN_BUCKET,
                format!("versioned-other-{key_index:03}.txt"),
                format!("other-{key_index:03}-{version:03}").as_bytes(),
            );
        }
    }

    storage
}

#[stress_test(
    tier = 3,
    metadata(component = "admin_api", operation = "list_buckets", scenario = "page")
)]
fn admin_list_buckets_page(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = bucket_listing_storage();

    ctx.parameter("bucket_count", 500);
    ctx.parameter("page_size", 50);
    let completed =
        measure_admin_response_len_workload(ctx, &runtime, &storage, "/admin/v1/buckets?limit=50");
    black_box(completed);
}

#[stress_test(
    tier = 3,
    metadata(
        component = "admin_api",
        operation = "list_buckets",
        scenario = "search"
    )
)]
fn admin_list_buckets_search(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = bucket_listing_storage();

    ctx.parameter("bucket_count", 500);
    ctx.parameter("page_size", 50);
    ctx.parameter("search", "bucket-4");
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets?limit=50&search=bucket-4",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    mode = "fixed_duration",
    metadata(
        component = "admin_api",
        operation = "list_objects",
        scenario = "root_directory"
    )
)]
fn admin_list_objects_root_directory(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("object_count", 3_001);
    ctx.parameter("page_size", 50);
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects?limit=50",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    mode = "fixed_duration",
    metadata(
        component = "admin_api",
        operation = "list_objects",
        scenario = "nested_directory"
    )
)]
fn admin_list_objects_nested_directory(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("object_count", 10);
    ctx.parameter("page_size", 50);
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=dir-050/",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    mode = "fixed_duration",
    metadata(
        component = "admin_api",
        operation = "list_objects",
        scenario = "skewed_root_directory"
    )
)]
fn admin_list_objects_skewed_root_directory(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("directory_count", 2);
    ctx.parameter("page_size", 50);
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=skew/",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    metadata(
        component = "admin_api",
        operation = "list_objects",
        scenario = "search_flat_objects"
    )
)]
fn admin_list_objects_search_flat_objects(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("object_count", 1_000);
    ctx.parameter("page_size", 50);
    ctx.parameter("search", "object-09");
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=flat/&search=object-09",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    mode = "fixed_duration",
    metadata(
        component = "admin_api",
        operation = "object_detail",
        scenario = "metadata"
    )
)]
fn admin_object_detail_metadata(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("object_count", 1);
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects/flat%2Fobject-0500.txt",
    );
    black_box(completed);
}

#[stress_test(
    tier = 3,
    metadata(
        component = "admin_api",
        operation = "object_detail",
        scenario = "versions"
    )
)]
fn admin_object_detail_versions(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let storage = object_browsing_storage();

    ctx.parameter("version_count", 40);
    ctx.parameter("page_size", 25);
    let completed = measure_admin_response_len_workload(
        ctx,
        &runtime,
        &storage,
        "/admin/v1/buckets/bench-objects/objects/versioned-target.txt/versions?limit=25",
    );
    black_box(completed);
}

stress_main!();
