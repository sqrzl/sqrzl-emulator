use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;

pub type Body = Full<Bytes>;
pub type RequestBody = Incoming;
