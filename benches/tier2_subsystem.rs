use cntryl_stress::prelude::*;
use sqrzl_emulator::models::Object;
use sqrzl_emulator::storage::{BucketStore, FilesystemStorage, ObjectListingStore, ObjectStore};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};
use uuid::Uuid;

static PUT_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static GET_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static RANGE_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static FLAT_LIST_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static DIRECTORY_LIST_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static SKEWED_LIST_STORAGE: OnceLock<Arc<FilesystemStorage>> = OnceLock::new();
static PUT_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("sqrzl_bench_tier2_{}", Uuid::new_v4()))
}

fn new_storage_with_bucket() -> FilesystemStorage {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
    storage
}

fn put_storage() -> Arc<FilesystemStorage> {
    PUT_STORAGE
        .get_or_init(|| Arc::new(new_storage_with_bucket()))
        .clone()
}

fn get_storage() -> Arc<FilesystemStorage> {
    GET_STORAGE
        .get_or_init(|| {
            let storage = new_storage_with_bucket();
            storage
                .put_object(
                    "bench",
                    "item.txt".to_string(),
                    Object::new(
                        "item.txt".to_string(),
                        vec![b'a'; 1024],
                        "text/plain".to_string(),
                    ),
                )
                .expect("seed put should succeed");
            Arc::new(storage)
        })
        .clone()
}

fn range_storage() -> Arc<FilesystemStorage> {
    RANGE_STORAGE
        .get_or_init(|| {
            let storage = new_storage_with_bucket();
            storage
                .put_object(
                    "bench",
                    "item.txt".to_string(),
                    Object::new(
                        "item.txt".to_string(),
                        vec![b'a'; 4096],
                        "text/plain".to_string(),
                    ),
                )
                .expect("seed put should succeed");
            Arc::new(storage)
        })
        .clone()
}

fn flat_list_storage() -> Arc<FilesystemStorage> {
    FLAT_LIST_STORAGE
        .get_or_init(|| {
            let storage = new_storage_with_bucket();
            seed_flat_objects(&storage, 128, 512);
            Arc::new(storage)
        })
        .clone()
}

fn directory_list_storage() -> Arc<FilesystemStorage> {
    DIRECTORY_LIST_STORAGE
        .get_or_init(|| {
            let storage = new_storage_with_bucket();
            seed_directory_children(&storage);
            Arc::new(storage)
        })
        .clone()
}

fn skewed_list_storage() -> Arc<FilesystemStorage> {
    SKEWED_LIST_STORAGE
        .get_or_init(|| {
            let storage = new_storage_with_bucket();
            seed_skewed_directory_children(&storage);
            Arc::new(storage)
        })
        .clone()
}

#[stress_test(
    tier = 2,
    metadata(
        component = "storage",
        operation = "put_object",
        scenario = "1k_payload"
    )
)]
fn put_object_1k_payload(ctx: &mut StressContext) {
    let storage = put_storage();
    let payload_size = 1024usize;
    let payload = vec![b'a'; payload_size];
    let sequence = PUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let key = format!("item-{sequence}.txt");
    let object = Object::new(key.clone(), payload, "text/plain".to_string());

    ctx.parameter("payload_size_bytes", payload_size);
    ctx.measure(|| {
        storage
            .put_object("bench", key, object)
            .expect("put should succeed");
    });
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "get_object",
        scenario = "1k_payload"
    )
)]
fn get_object_1k_payload(ctx: &mut StressContext) {
    let storage = get_storage();

    ctx.parameter("payload_size_bytes", 1024);
    let _ = ctx.measure_workload(|| {
        let object = storage
            .get_object("bench", "item.txt")
            .expect("get should succeed");
        black_box(object);
    });
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "get_object_range",
        scenario = "64b_range"
    )
)]
fn get_object_64b_range(ctx: &mut StressContext) {
    let storage = range_storage();

    ctx.parameter("payload_size_bytes", 4096);
    ctx.parameter("range_start", 64);
    ctx.parameter("range_end", 128);
    let _ = ctx.measure_workload(|| {
        let object = storage
            .get_object_range("bench", "item.txt", 64, Some(128))
            .expect("range get should succeed");
        black_box(object);
    });
}

fn seed_flat_objects(storage: &FilesystemStorage, object_count: usize, payload_size: usize) {
    let payload = vec![b'a'; payload_size];
    for index in 0..object_count {
        storage
            .put_object(
                "bench",
                format!("item-{index:03}.txt"),
                Object::new(
                    format!("item-{index:03}.txt"),
                    payload.clone(),
                    "text/plain".to_string(),
                ),
            )
            .expect("seed put should succeed");
    }
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "list_objects",
        scenario = "flat_128"
    )
)]
fn list_objects_flat_128(ctx: &mut StressContext) {
    let storage = flat_list_storage();

    ctx.parameter("object_count", 128);
    ctx.parameter("page_size", 128);
    let _ = ctx.measure_workload(|| {
        let listing = storage
            .list_objects("bench", Some("item-"), None, None, Some(128))
            .expect("list should succeed");
        black_box(listing);
    });
}

fn seed_directory_children(storage: &FilesystemStorage) {
    let payload = vec![b'a'; 256];
    for dir_index in 0..100usize {
        for object_index in 0..10usize {
            let key = format!("dir-{dir_index:03}/item-{object_index:03}.txt");
            storage
                .put_object(
                    "bench",
                    key.clone(),
                    Object::new(key, payload.clone(), "text/plain".to_string()),
                )
                .expect("seed put should succeed");
        }
    }
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "list_directory_children",
        scenario = "root_children"
    )
)]
fn list_directory_root_children(ctx: &mut StressContext) {
    let storage = directory_list_storage();

    ctx.parameter("directory_count", 100);
    ctx.parameter("page_size", 100);
    let _ = ctx.measure_workload(|| {
        let listing = storage
            .list_objects("bench", Some(""), Some("/"), None, Some(100))
            .expect("directory list should succeed");
        black_box(listing);
    });
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "list_directory_children",
        scenario = "nested_children"
    )
)]
fn list_directory_nested_children(ctx: &mut StressContext) {
    let storage = directory_list_storage();

    ctx.parameter("object_count", 10);
    ctx.parameter("page_size", 10);
    let _ = ctx.measure_workload(|| {
        let listing = storage
            .list_objects("bench", Some("dir-050/"), Some("/"), None, Some(10))
            .expect("nested directory list should succeed");
        black_box(listing);
    });
}

fn seed_skewed_directory_children(storage: &FilesystemStorage) {
    let payload = vec![b'a'; 128];
    for index in 0..1_000usize {
        let key = format!("a/blob-{index:04}.txt");
        storage
            .put_object(
                "bench",
                key.clone(),
                Object::new(key, payload.clone(), "text/plain".to_string()),
            )
            .expect("seed put should succeed");
    }
    storage
        .put_object(
            "bench",
            "z/blob.txt".to_string(),
            Object::new("z/blob.txt".to_string(), payload, "text/plain".to_string()),
        )
        .expect("seed put should succeed");
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "list_skewed_directory_children",
        scenario = "root_children"
    )
)]
fn list_skewed_directory_root_children(ctx: &mut StressContext) {
    let storage = skewed_list_storage();

    ctx.parameter("directory_count", 2);
    ctx.parameter("page_size", 50);
    let _ = ctx.measure_workload(|| {
        let listing = storage
            .list_objects("bench", Some(""), Some("/"), None, Some(50))
            .expect("skewed root directory list should succeed");
        black_box(listing);
    });
}

#[stress_test(
    tier = 2,
    mode = "fixed_duration",
    metadata(
        component = "storage",
        operation = "list_skewed_directory_children",
        scenario = "large_prefix_page"
    )
)]
fn list_skewed_directory_large_prefix_page(ctx: &mut StressContext) {
    let storage = skewed_list_storage();

    ctx.parameter("object_count", 1_000);
    ctx.parameter("page_size", 50);
    let _ = ctx.measure_workload(|| {
        let listing = storage
            .list_objects("bench", Some("a/"), Some("/"), None, Some(50))
            .expect("large prefix directory list should succeed");
        black_box(listing);
    });
}

stress_main!();
