use bytes::Bytes;
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
        _cx: &mut Context<'_>,
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
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(&self.kind, Kind::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        let exact = match &self.kind {
            Kind::Full(bytes) => bytes.as_ref().map_or(0, |bytes| bytes.len() as u64),
            Kind::Abort { expected, done } | Kind::Truncated { expected, done, .. } => {
                u64::from(!done).saturating_mul(*expected)
            }
        };
        SizeHint::with_exact(exact)
    }
}

pub type RequestBody = hyper::body::Incoming;
