use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use sqrzl_emulator::models::object::compute_etag;
use sqrzl_emulator::utils::xml::{parse_acl_xml, tagging_xml};
use std::collections::HashMap;
use std::hint::black_box;

#[path = "support/criterion_config.rs"]
mod criterion_config;

fn bench_compute_etag(c: &mut Criterion) {
    let payload_sizes = [0usize, 64, 1024, 16_384];
    let mut group = c.benchmark_group("tier1_hotpath_etag");
    group.sampling_mode(SamplingMode::Flat);

    for size in payload_sizes {
        let payload = vec![b'a'; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::new("compute_etag", size), |b| {
            b.iter(|| black_box(compute_etag(black_box(&payload))));
        });
    }

    group.finish();
}

fn bench_render_tagging_xml(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier1_hotpath_tagging_xml");
    group.sampling_mode(SamplingMode::Flat);

    for tag_count in [1usize, 8, 32] {
        let mut tags = HashMap::with_capacity(tag_count);
        for index in 0..tag_count {
            tags.insert(
                format!("key{index:02}"),
                format!("value{index:02}_with_&_chars"),
            );
        }

        group.throughput(Throughput::Elements(tag_count as u64));
        group.bench_function(BenchmarkId::new("render_tagging_xml", tag_count), |b| {
            b.iter(|| black_box(tagging_xml(black_box(&tags))));
        });
    }

    group.finish();
}

fn bench_parse_acl_xml(c: &mut Criterion) {
    let acl_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>owner-id</ID>
        <DisplayName>Owner</DisplayName>
    </Owner>
    <AccessControlList>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser">
                <ID>owner-id</ID>
            </Grantee>
            <Permission>FULL_CONTROL</Permission>
        </Grant>
    </AccessControlList>
</AccessControlPolicy>"#;

    let mut group = c.benchmark_group("tier1_hotpath_acl_xml");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    group.bench_function("parse_acl_xml", |b| {
        b.iter(|| black_box(parse_acl_xml(black_box(acl_xml)).expect("acl xml should parse")));
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier1();
    targets = bench_compute_etag, bench_render_tagging_xml, bench_parse_acl_xml
}
criterion_main!(benches);
