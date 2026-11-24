mod handlers;
mod http;

pub(crate) use handlers::handle_request as handle_s3_request;
pub use http::{RouteMatch, Router};
#[cfg(test)]
pub(crate) use sqrzl_emulator_core::server::http::SpooledPayload;
pub(crate) use sqrzl_emulator_core::server::{RequestExt, ResponseBuilder};
