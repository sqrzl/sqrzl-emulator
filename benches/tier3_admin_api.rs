use bytes::Bytes;
use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use http_body_util::BodyExt;
use hyper::{Method, Request};
use sqrzl_emulator::body::Body;
use sqrzl_emulator::models::Object;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Builder;
use uuid::Uuid;

#[path = "support/criterion_config.rs"]
mod criterion_config;

const ADMIN_BUCKET: &str = "bench-objects";

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

fn put_text(storage: &Arc<dyn Storage>, bucket: &str, key: String, payload: &[u8]) {
    storage
        .put_object(
            bucket,
            key.clone(),
            Object::new(key, payload.to_vec(), "text/plain".to_string()),
        )
        .expect("seed put should succeed");
}

fn seed_bucket_listing_storage(bucket_count: usize) -> (Arc<dyn Storage>, PathBuf) {
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));

    for index in 0..bucket_count {
        storage
            .create_bucket(format!("bucket-{index:03}"))
            .expect("seed bucket create should succeed");
    }

    (storage, base)
}

fn seed_object_browsing_storage() -> (Arc<dyn Storage>, PathBuf) {
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

    (storage, base)
}

fn bench_admin_list_buckets(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let (storage, base) = seed_bucket_listing_storage(500);

    let mut group = c.benchmark_group("tier3_admin_api_list_buckets");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(50));
    group.bench_function(BenchmarkId::new("page", 50), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets?limit=50",
            )))
        });
    });
    group.bench_function(BenchmarkId::new("search", 50), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets?limit=50&search=bucket-4",
            )))
        });
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_admin_list_objects(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let (storage, base) = seed_object_browsing_storage();

    let mut group = c.benchmark_group("tier3_admin_api_list_objects");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(50));
    group.bench_function(BenchmarkId::new("root_directory", 50), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects?limit=50",
            )))
        });
    });
    group.bench_function(BenchmarkId::new("nested_directory", 10), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=dir-050/",
            )))
        });
    });
    group.bench_function(BenchmarkId::new("skewed_root_directory", 2), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=skew/",
            )))
        });
    });
    group.bench_function(BenchmarkId::new("search_flat_objects", 50), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects?limit=50&prefix=flat/&search=object-09",
            )))
        });
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_admin_object_detail(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let (storage, base) = seed_object_browsing_storage();

    let mut group = c.benchmark_group("tier3_admin_api_object_detail");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    group.bench_function("metadata", |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects/flat%2Fobject-0500.txt",
            )))
        });
    });
    group.bench_function(BenchmarkId::new("versions", 25), |b| {
        b.iter(|| {
            black_box(runtime.block_on(admin_response_len(
                storage.clone(),
                "/admin/v1/buckets/bench-objects/objects/versioned-target.txt/versions?limit=25",
            )))
        });
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier3();
    targets = bench_admin_list_buckets, bench_admin_list_objects, bench_admin_object_detail
}
criterion_main!(benches);
