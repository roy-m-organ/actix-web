//! `ResponseError` trait and foreign impls.

use std::{
    error::Error as StdError,
    fmt,
    io::{self, Write as _},
};

use actix_http::{body::AnyBody, header, Response, StatusCode};
use bytes::BytesMut;

use crate::{__downcast_dyn, __downcast_get_type_id};
use crate::{helpers, HttpResponse};

/// General purpose actix web error.
///
/// An actix web error is used to carry errors from `std::error`
/// through actix in a convenient way.  It can be created through
/// converting errors with `into()`.
///
/// Whenever it is created from an external object a response error is created
/// for it that can be used to create an HTTP response from it this means that
/// if you have access to an actix `Error` you can always get a
/// `ResponseError` reference from it.
pub struct Error {
    cause: Box<dyn ResponseError>,
}

impl Error {
    /// Returns the reference to the underlying `ResponseError`.
    pub fn as_response_error(&self) -> &dyn ResponseError {
        self.cause.as_ref()
    }

    /// Similar to `as_response_error` but downcasts.
    pub fn as_error<T: ResponseError + 'static>(&self) -> Option<&T> {
        <dyn ResponseError>::downcast_ref(self.cause.as_ref())
    }

    /// TODO
    pub fn error_response(&self) -> HttpResponse {
        self.cause.error_response()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &self.cause)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(val: std::convert::Infallible) -> Self {
        match val {}
    }
}

/// `Error` for any error that implements `ResponseError`
impl<T: ResponseError + 'static> From<T> for Error {
    fn from(err: T) -> Error {
        Error {
            cause: Box::new(err),
        }
    }
}

impl From<Error> for Response<AnyBody> {
    fn from(err: Error) -> Response<AnyBody> {
        err.error_response().into()
    }
}

/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////
/////////////////////

/// Errors that can generate responses.
// TODO: add std::error::Error bound when replacement for Box<dyn Error> is found
pub trait ResponseError: fmt::Debug + fmt::Display {
    /// Returns appropriate status code for error.
    ///
    /// A 500 Internal Server Error is used by default. If [error_response](Self::error_response) is
    /// also implemented and does not call `self.status_code()`, then this will not be used.
    fn status_code(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    /// Creates full response for error.
    ///
    /// By default, the generated response uses a 500 Internal Server Error status code, a
    /// `Content-Type` of `text/plain`, and the body is set to `Self`'s `Display` impl.
    fn error_response(&self) -> HttpResponse {
        let mut res = HttpResponse::new(self.status_code());

        let mut buf = BytesMut::new();
        let _ = write!(helpers::MutWriter(&mut buf), "{}", self);

        res.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );

        res.set_body(AnyBody::from(buf))
    }

    __downcast_get_type_id!();
}

__downcast_dyn!(ResponseError);

impl ResponseError for Box<dyn StdError + 'static> {}

#[cfg(feature = "openssl")]
impl ResponseError for actix_tls::accept::openssl::SslError {}

impl ResponseError for serde::de::value::Error {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl ResponseError for std::str::Utf8Error {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl ResponseError for std::io::Error {
    fn status_code(&self) -> StatusCode {
        // TODO: decide if these errors should consider not found or permission errors
        match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl ResponseError for actix_http::error::HttpError {}

impl ResponseError for actix_http::Error {
    fn status_code(&self) -> StatusCode {
        // TODO: map error kinds to status code better
        StatusCode::INTERNAL_SERVER_ERROR
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::new(self.status_code()).set_body(self.to_string().into())
    }
}

impl ResponseError for actix_http::header::InvalidHeaderValue {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl ResponseError for actix_http::error::ParseError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl ResponseError for actix_http::error::BlockingError {}

impl ResponseError for actix_http::error::PayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            actix_http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl ResponseError for actix_http::ws::ProtocolError {}

impl ResponseError for actix_http::error::ContentTypeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl ResponseError for actix_http::ws::HandshakeError {
    fn error_response(&self) -> HttpResponse {
        Response::from(self).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_casting() {
        use actix_http::error::{ContentTypeError, PayloadError};

        let err = PayloadError::Overflow;
        let resp_err: &dyn ResponseError = &err;

        let err = resp_err.downcast_ref::<PayloadError>().unwrap();
        assert_eq!(err.to_string(), "Payload reached size limit.");

        let not_err = resp_err.downcast_ref::<ContentTypeError>();
        assert!(not_err.is_none());
    }
}
