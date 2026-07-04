use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use sqrzl_emulator::models::Object;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::hint::black_box;
use std::path::PathBuf;
use uuid::Uuid;

#[path = "support/criterion_config.rs"]
mod criterion_config;

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("sqrzl_bench_tier2_{}", Uuid::new_v4()))
}

fn bench_put_object(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");

    let payload_size = 1024usize;
    let payload = vec![b'a'; payload_size];
    let content_type = "text/plain".to_string();

    let mut group = c.benchmark_group("tier2_subsystem_put_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload_size as u64));
    group.bench_function(BenchmarkId::new("put_object", payload_size), |b| {
        b.iter_batched(
            || {
                Object::new(
                    "item.txt".to_string(),
                    payload.clone(),
                    content_type.clone(),
                )
            },
            |object| {
                storage
                    .put_object("bench", "item.txt".to_string(), object)
                    .expect("put should succeed");
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_get_object(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
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

    let mut group = c.benchmark_group("tier2_subsystem_get_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    group.bench_function("get_object", |b| {
        b.iter(|| {
            black_box(
                storage
                    .get_object("bench", "item.txt")
                    .expect("get should succeed"),
            )
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_get_object_range(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
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

    let mut group = c.benchmark_group("tier2_subsystem_get_object_range");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(64));
    group.bench_function("get_object_range", |b| {
        b.iter(|| {
            black_box(
                storage
                    .get_object_range("bench", "item.txt", 64, Some(128))
                    .expect("range get should succeed"),
            )
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_list_objects(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");

    let payload = vec![b'a'; 512];
    for index in 0..128usize {
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

    let mut group = c.benchmark_group("tier2_subsystem_list_objects");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(128));
    group.bench_function(BenchmarkId::new("list_objects", 128), |b| {
        b.iter(|| {
            black_box(
                storage
                    .list_objects("bench", Some("item-"), None, None, Some(128))
                    .expect("list should succeed"),
            )
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_list_directory_children(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");

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

    let mut group = c.benchmark_group("tier2_subsystem_list_directory_children");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(100));
    group.bench_function(BenchmarkId::new("root_children", 100), |b| {
        b.iter(|| {
            black_box(
                storage
                    .list_objects("bench", Some(""), Some("/"), None, Some(100))
                    .expect("directory list should succeed"),
            )
        })
    });
    group.bench_function(BenchmarkId::new("nested_children", 10), |b| {
        b.iter(|| {
            black_box(
                storage
                    .list_objects("bench", Some("dir-050/"), Some("/"), None, Some(10))
                    .expect("nested directory list should succeed"),
            )
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

fn bench_list_skewed_directory_children(c: &mut Criterion) {
    let base = temp_path();
    let storage = FilesystemStorage::new(&base);
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");

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

    let mut group = c.benchmark_group("tier2_subsystem_list_skewed_directory_children");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(2));
    group.bench_function(BenchmarkId::new("root_children", 2), |b| {
        b.iter(|| {
            black_box(
                storage
                    .list_objects("bench", Some(""), Some("/"), None, Some(50))
                    .expect("skewed root directory list should succeed"),
            )
        })
    });
    group.throughput(Throughput::Elements(50));
    group.bench_function(BenchmarkId::new("large_prefix_page", 50), |b| {
        b.iter(|| {
            black_box(
                storage
                    .list_objects("bench", Some("a/"), Some("/"), None, Some(50))
                    .expect("large prefix directory list should succeed"),
            )
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier2();
    targets = bench_put_object, bench_get_object, bench_get_object_range, bench_list_objects, bench_list_directory_children, bench_list_skewed_directory_children
}
criterion_main!(benches);
