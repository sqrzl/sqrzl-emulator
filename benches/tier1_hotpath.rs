use cntryl_stress::prelude::*;
use sqrzl_emulator::models::object::compute_etag;
use sqrzl_emulator::utils::xml::{parse_acl_xml, tagging_xml};
use std::collections::HashMap;

fn measure_compute_etag(ctx: &mut StressContext, payload_size_bytes: usize) {
    let payload = vec![b'a'; payload_size_bytes];
    ctx.parameter("payload_size_bytes", payload_size_bytes);
    ctx.measure_micro(|| black_box(compute_etag(black_box(payload.as_slice()))));
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "object",
        operation = "compute_etag",
        scenario = "empty_payload",
        validated_micro = "true"
    )
)]
fn compute_etag_empty_payload(ctx: &mut StressContext) {
    measure_compute_etag(ctx, 0);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "object",
        operation = "compute_etag",
        scenario = "64b_payload",
        validated_micro = "true"
    )
)]
fn compute_etag_64b_payload(ctx: &mut StressContext) {
    measure_compute_etag(ctx, 64);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "object",
        operation = "compute_etag",
        scenario = "1k_payload",
        validated_micro = "true"
    )
)]
fn compute_etag_1k_payload(ctx: &mut StressContext) {
    measure_compute_etag(ctx, 1024);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "object",
        operation = "compute_etag",
        scenario = "16k_payload",
        validated_micro = "true"
    )
)]
fn compute_etag_16k_payload(ctx: &mut StressContext) {
    measure_compute_etag(ctx, 16_384);
}

fn build_tags(tag_count: usize) -> HashMap<String, String> {
    let mut tags = HashMap::with_capacity(tag_count);
    for index in 0..tag_count {
        tags.insert(
            format!("key{index:02}"),
            format!("value{index:02}_with_&_chars"),
        );
    }
    tags
}

fn measure_render_tagging_xml(ctx: &mut StressContext, tag_count: usize) {
    let tags = build_tags(tag_count);
    ctx.parameter("tag_count", tag_count);
    ctx.measure_micro(|| black_box(tagging_xml(black_box(&tags))));
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "xml",
        operation = "render_tagging_xml",
        scenario = "single_tag",
        validated_micro = "true"
    )
)]
fn render_tagging_xml_single_tag(ctx: &mut StressContext) {
    measure_render_tagging_xml(ctx, 1);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "xml",
        operation = "render_tagging_xml",
        scenario = "eight_tags",
        validated_micro = "true"
    )
)]
fn render_tagging_xml_eight_tags(ctx: &mut StressContext) {
    measure_render_tagging_xml(ctx, 8);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "xml",
        operation = "render_tagging_xml",
        scenario = "thirty_two_tags",
        validated_micro = "true"
    )
)]
fn render_tagging_xml_thirty_two_tags(ctx: &mut StressContext) {
    measure_render_tagging_xml(ctx, 32);
}

#[stress_test(
    tier = 1,
    mode = "micro",
    metadata(
        component = "xml",
        operation = "parse_acl_xml",
        scenario = "single_grant",
        validated_micro = "true"
    )
)]
fn parse_acl_xml_single_grant(ctx: &mut StressContext) {
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

    ctx.parameter("grant_count", 1);
    ctx.measure_micro(|| {
        black_box(parse_acl_xml(black_box(acl_xml)).expect("acl xml should parse"))
    });
}

stress_main!();
