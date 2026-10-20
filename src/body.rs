use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
use hyper::body::{Frame, SizeHint};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct Body {
    kind: Kind,
}

enum Kind {
    Full(Option<Bytes>),
    Abort {
        expected: u64,
        done: bool,
    },
    Truncated {
        bytes: Option<Bytes>,
        expected: u64,
        done: bool,
    },
    Stream(UnsyncBoxBody<Bytes, io::Error>),
}

impl Body {
    #[must_use]
    pub fn abort(expected: u64) -> Self {
        Self {
            kind: Kind::Abort {
                expected,
                done: false,
            },
        }
    }

    #[must_use]
    pub fn truncated(bytes: Bytes, expected: u64) -> Self {
        Self {
            kind: Kind::Truncated {
                bytes: Some(bytes),
                expected,
                done: false,
            },
        }
    }

    #[must_use]
    pub(crate) fn aborts_connection(&self) -> bool {
        matches!(self.kind, Kind::Abort { .. })
    }

    /// Wrap a streaming HTTP body while preserving its frames for Hyper.
    pub(crate) fn from_stream<B>(body: B) -> Self
    where
        B: hyper::body::Body<Data = Bytes> + Send + 'static,
        B::Error: std::fmt::Display + Send + Sync + 'static,
    {
        Self {
            kind: Kind::Stream(
                body.map_err(|error| io::Error::other(error.to_string()))
                    .boxed_unsync(),
            ),
        }
    }

    pub(crate) fn full_bytes(&self) -> Option<&Bytes> {
        match &self.kind {
            Kind::Full(bytes) => bytes.as_ref(),
            _ => None,
        }
    }
}

impl Default for Body {
    fn default() -> Self {
        Self::from(Bytes::new())
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Self {
            kind: Kind::Full(Some(bytes)),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from(Bytes::from(bytes))
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Self::from(Bytes::from(value))
    }
}

impl From<&'static str> for Body {
    fn from(value: &'static str) -> Self {
        Self::from(Bytes::from_static(value.as_bytes()))
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match &mut self.kind {
            Kind::Full(bytes) => Poll::Ready(
                bytes
                    .take()
                    .and_then(|bytes| (!bytes.is_empty()).then(|| Ok(Frame::data(bytes)))),
            ),
            Kind::Abort { done, .. } => {
                if *done {
                    Poll::Ready(None)
                } else {
                    *done = true;
                    Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "Sqrzl response-loss failpoint",
                    ))))
                }
            }
            Kind::Truncated { bytes, done, .. } => {
                if let Some(bytes) = bytes.take() {
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                if *done {
                    Poll::Ready(None)
                } else {
                    *done = true;
                    Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Sqrzl truncated-response failpoint",
                    ))))
                }
            }
            Kind::Stream(body) => Pin::new(body).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Full(None) => true,
            Kind::Stream(body) => body.is_end_stream(),
            _ => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        let exact = match &self.kind {
            Kind::Full(bytes) => bytes.as_ref().map_or(0, |bytes| bytes.len() as u64),
            Kind::Abort { expected, done } | Kind::Truncated { expected, done, .. } => {
                u64::from(!done).saturating_mul(*expected)
            }
            Kind::Stream(body) => return body.size_hint(),
        };
        SizeHint::with_exact(exact)
    }
}

pub type RequestBody = hyper::body::Incoming;
