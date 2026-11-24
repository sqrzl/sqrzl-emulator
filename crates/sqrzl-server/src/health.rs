use crate::body::Body;
use crate::server::ResponseBuilder;
use crate::storage::BucketStore;
use http::StatusCode;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    api_port: u16,
    ui_port: u16,
    auth_enforced: bool,
    auth_enforced_providers: Vec<&'static str>,
    admin_auth_enforced: bool,
    max_request_bytes: usize,
    storage_ready: bool,
    mail_ready: Option<bool>,
    sms_ready: Option<bool>,
    runtime: crate::runtime::RuntimeSnapshot,
    runtime_limits: crate::runtime::RuntimeLimits,
    enabled_providers: Vec<&'static str>,
}

pub fn response(
    storage: &(impl BucketStore + ?Sized),
    config: &crate::Config,
) -> hyper::Response<Body> {
    render(storage, config, None, None)
}

pub fn response_with_capture(
    storage: &(impl BucketStore + ?Sized),
    config: &crate::Config,
    mail: &dyn crate::mail::MailStore,
    sms: &dyn crate::sms::SmsStore,
) -> hyper::Response<Body> {
    render(
        storage,
        config,
        Some(mail.check_ready().is_ok()),
        Some(sms.check_ready().is_ok()),
    )
}

fn render(
    storage: &(impl BucketStore + ?Sized),
    config: &crate::Config,
    mail_ready: Option<bool>,
    sms_ready: Option<bool>,
) -> hyper::Response<Body> {
    let storage_ready = storage.check_ready().is_ok();
    let runtime = config.runtime.snapshot();
    let ready =
        storage_ready && mail_ready != Some(false) && sms_ready != Some(false) && !runtime.draining;

    let vendor = &config.vendor_credentials;
    let auth_enforced_providers = [
        ("s3-family", config.enforce_auth),
        ("azure-blob", config.azure_auth_enforced()),
        ("gcs", config.gcs_auth_enforced()),
        ("oci-object", config.oci_auth_enforced()),
        ("ses", config.enforce_auth),
        ("sns", config.enforce_auth),
        ("aws-sms-voice-v2", config.enforce_auth),
        (
            "twilio",
            vendor.twilio_account_sid.is_some() && vendor.twilio_auth_token.is_some(),
        ),
        ("sendgrid", vendor.sendgrid_api_key.is_some()),
        ("acs", vendor.acs_connection_string.is_some()),
    ]
    .into_iter()
    .filter_map(|(provider, enforced)| enforced.then_some(provider))
    .collect::<Vec<_>>();
    let body = HealthResponse {
        status: if ready { "ok" } else { "degraded" },
        version: env!("CARGO_PKG_VERSION"),
        api_port: config.api_port,
        ui_port: config.ui_port,
        auth_enforced: !auth_enforced_providers.is_empty(),
        auth_enforced_providers,
        admin_auth_enforced: config.admin_auth_enforced(),
        max_request_bytes: config.max_request_bytes,
        storage_ready,
        mail_ready,
        sms_ready,
        runtime,
        runtime_limits: config.runtime.limits,
        enabled_providers: vec![
            "s3-family",
            "azure-blob",
            "gcs",
            "oci-object",
            "smtp",
            "ses",
            "sns",
            "aws-sms-voice-v2",
            "twilio",
            "sendgrid",
            "acs",
        ],
    };

    match serde_json::to_vec(&body) {
        Ok(json) => ResponseBuilder::new(if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        })
        .content_type("application/json")
        .body(json)
        .build(),
        Err(err) => ResponseBuilder::new(StatusCode::INTERNAL_SERVER_ERROR)
            .content_type("text/plain; charset=utf-8")
            .body_str(&format!("failed to render health response: {err}"))
            .build(),
    }
}
