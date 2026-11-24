use crate::body::Body;
use crate::hyper_compat::Compat;
use hyper::service::Service;
use hyper::Response;
use std::sync::Arc;

pub mod http;

pub use http::{Request as RequestExt, RequestParseError, ResponseBuilder};

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub async fn serve_h1_connection<S>(
    stream: tokio::net::TcpStream,
    service: S,
) -> Result<(), hyper::Error>
where
    S: Service<hyper::Request<hyper::body::Incoming>, Response = Response<Body>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
{
    serve_h1_connection_until(
        stream,
        service,
        crate::runtime::RuntimeControl::new(crate::runtime::RuntimeLimits::default()),
    )
    .await
}

/// Serves an HTTP connection until completion or coordinated graceful shutdown.
///
/// # Errors
/// Returns HTTP protocol and transport errors.
pub async fn serve_h1_connection_until<S>(
    stream: tokio::net::TcpStream,
    service: S,
    control: Arc<crate::runtime::RuntimeControl>,
) -> Result<(), hyper::Error>
where
    S: Service<hyper::Request<hyper::body::Incoming>, Response = Response<Body>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
{
    let connection =
        hyper::server::conn::http1::Builder::new().serve_connection(Compat::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => result,
        () = control.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    }
}
