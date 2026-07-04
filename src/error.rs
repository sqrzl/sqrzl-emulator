use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Bucket already exists")]
    BucketAlreadyExists,

    #[error("Bucket not found")]
    BucketNotFound,

    #[error("Bucket not empty")]
    BucketNotEmpty,

    #[error("Key not found")]
    KeyNotFound,

    #[error("Invalid request")]
    InvalidRequest(String),

    #[error("Method not allowed")]
    MethodNotAllowed(String),

    #[error("Route not found")]
    RouteNotFound(String),

    #[error("Access denied")]
    AccessDenied,

    #[error("Invalid multipart upload ID")]
    InvalidUploadId,

    #[error("No such upload")]
    NoSuchUpload,

    #[error("Invalid part number")]
    InvalidPartNumber,

    #[error("Invalid part order")]
    InvalidPartOrder,

    #[error("Incomplete multipart upload")]
    IncompleteMultipartUpload,

    #[error("No such version")]
    NoSuchVersion,

    #[error("No such lifecycle configuration")]
    NoSuchLifecycleConfiguration,

    #[error("Invalid policy")]
    InvalidPolicy(String),

    #[error("Internal server error")]
    InternalError(String),

    #[error("Signature does not match")]
    SignatureDoesNotMatch,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    #[must_use]
    pub fn status_code(&self) -> http::StatusCode {
        match self {
            Error::BucketAlreadyExists | Error::BucketNotEmpty => http::StatusCode::CONFLICT,
            Error::BucketNotFound
            | Error::KeyNotFound
            | Error::RouteNotFound(_)
            | Error::InvalidUploadId
            | Error::NoSuchUpload
            | Error::NoSuchVersion
            | Error::NoSuchLifecycleConfiguration => http::StatusCode::NOT_FOUND,
            Error::InvalidRequest(_)
            | Error::InvalidPartNumber
            | Error::InvalidPartOrder
            | Error::IncompleteMultipartUpload
            | Error::InvalidPolicy(_) => http::StatusCode::BAD_REQUEST,
            Error::MethodNotAllowed(_) => http::StatusCode::METHOD_NOT_ALLOWED,
            Error::AccessDenied | Error::SignatureDoesNotMatch => http::StatusCode::FORBIDDEN,
            Error::InternalError(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Error::BucketAlreadyExists => "BucketAlreadyExists",
            Error::BucketNotFound => "NoSuchBucket",
            Error::BucketNotEmpty => "BucketNotEmpty",
            Error::KeyNotFound => "NoSuchKey",
            Error::InvalidRequest(_) => "InvalidRequest",
            Error::MethodNotAllowed(_) => "MethodNotAllowed",
            Error::RouteNotFound(_) => "NotFound",
            Error::AccessDenied => "AccessDenied",
            Error::InvalidUploadId | Error::NoSuchUpload => "NoSuchUpload",
            Error::InvalidPartNumber => "InvalidPartNumber",
            Error::InvalidPartOrder => "InvalidPartOrder",
            Error::IncompleteMultipartUpload => "IncompleteMultipartUpload",
            Error::NoSuchVersion => "NoSuchVersion",
            Error::NoSuchLifecycleConfiguration => "NoSuchLifecycleConfiguration",
            Error::InvalidPolicy(_) => "MalformedPolicy",
            Error::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            Error::InternalError(_) => "InternalError",
        }
    }
}

impl From<Error> for String {
    fn from(err: Error) -> Self {
        err.to_string()
    }
}
