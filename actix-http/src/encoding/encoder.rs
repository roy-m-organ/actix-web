//! Stream encoders.

use std::{
    error::Error as StdError,
    future::Future,
    io::{self, Write as _},
    pin::Pin,
    task::{Context, Poll},
};

use actix_rt::task::{spawn_blocking, JoinHandle};
use brotli2::write::BrotliEncoder;
use bytes::Bytes;
use derive_more::Display;
use flate2::write::{GzEncoder, ZlibEncoder};
use futures_core::ready;
use pin_project::pin_project;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::{
    body::{Body, BodySize, BoxAnyBody, MessageBody, ResponseBody},
    http::{
        header::{ContentEncoding, CONTENT_ENCODING},
        HeaderValue, StatusCode,
    },
    Error, ResponseHead,
};

use super::Writer;
use crate::error::BlockingError;

const MAX_CHUNK_SIZE_ENCODE_IN_PLACE: usize = 1024;

#[pin_project]
pub struct Encoder<B> {
    eof: bool,
    #[pin]
    body: EncoderBody<B>,
    encoder: Option<ContentEncoder>,
    fut: Option<JoinHandle<Result<ContentEncoder, io::Error>>>,
}

impl<B: MessageBody> Encoder<B> {
    pub fn response(
        encoding: ContentEncoding,
        head: &mut ResponseHead,
        body: ResponseBody<B>,
    ) -> ResponseBody<Encoder<B>> {
        let can_encode = !(head.headers().contains_key(&CONTENT_ENCODING)
            || head.status == StatusCode::SWITCHING_PROTOCOLS
            || head.status == StatusCode::NO_CONTENT
            || encoding == ContentEncoding::Identity
            || encoding == ContentEncoding::Auto);

        let body = match body {
            ResponseBody::Other(b) => match b {
                Body::None => return ResponseBody::Other(Body::None),
                Body::Empty => return ResponseBody::Other(Body::Empty),
                Body::Bytes(buf) => {
                    if can_encode {
                        EncoderBody::Bytes(buf)
                    } else {
                        return ResponseBody::Other(Body::Bytes(buf));
                    }
                }
                Body::Message(stream) => EncoderBody::BoxedStream(stream),
            },
            ResponseBody::Body(stream) => EncoderBody::Stream(stream),
        };

        if can_encode {
            // Modify response body only if encoder is not None
            if let Some(enc) = ContentEncoder::encoder(encoding) {
                update_head(encoding, head);
                head.no_chunking(false);
                return ResponseBody::Body(Encoder {
                    body,
                    eof: false,
                    fut: None,
                    encoder: Some(enc),
                });
            }
        }

        ResponseBody::Body(Encoder {
            body,
            eof: false,
            fut: None,
            encoder: None,
        })
    }
}

#[pin_project(project = EncoderBodyProj)]
enum EncoderBody<B> {
    Bytes(Bytes),
    Stream(#[pin] B),
    BoxedStream(BoxAnyBody),
}

impl<B> MessageBody for EncoderBody<B>
where
    B: MessageBody,
    B::Error: Into<Error>,
{
    type Error = EncoderError<B::Error>;

    fn size(&self) -> BodySize {
        match self {
            EncoderBody::Bytes(ref b) => b.size(),
            EncoderBody::Stream(ref b) => b.size(),
            EncoderBody::BoxedStream(ref b) => b.size(),
        }
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        match self.project() {
            EncoderBodyProj::Bytes(b) => {
                if b.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(std::mem::take(b))))
                }
            }
            // TODO: MSRV 1.51: poll_map_err
            EncoderBodyProj::Stream(b) => match ready!(b.poll_next(cx)) {
                Some(Err(err)) => Poll::Ready(Some(Err(EncoderError::Body(err)))),
                Some(Ok(val)) => Poll::Ready(Some(Ok(val))),
                None => Poll::Ready(None),
            },
            EncoderBodyProj::BoxedStream(ref mut b) => {
                match ready!(b.as_pin_mut().poll_next(cx)) {
                    Some(Err(err)) => {
                        Poll::Ready(Some(Err(EncoderError::Boxed(err.into()))))
                    }
                    Some(Ok(val)) => Poll::Ready(Some(Ok(val))),
                    None => Poll::Ready(None),
                }
            }
        }
    }
}

impl<B> MessageBody for Encoder<B>
where
    B: MessageBody,
    B::Error: Into<Error>,
{
    type Error = EncoderError<B::Error>;

    fn size(&self) -> BodySize {
        if self.encoder.is_none() {
            self.body.size()
        } else {
            BodySize::Stream
        }
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let mut this = self.project();
        loop {
            if *this.eof {
                return Poll::Ready(None);
            }

            if let Some(ref mut fut) = this.fut {
                let mut encoder = ready!(Pin::new(fut).poll(cx))
                    .map_err(|_| EncoderError::Blocking(BlockingError))?
                    .map_err(EncoderError::Io)?;

                let chunk = encoder.take();
                *this.encoder = Some(encoder);
                this.fut.take();

                if !chunk.is_empty() {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            let result = ready!(this.body.as_mut().poll_next(cx));

            match result {
                Some(Err(err)) => return Poll::Ready(Some(Err(err))),

                Some(Ok(chunk)) => {
                    if let Some(mut encoder) = this.encoder.take() {
                        if chunk.len() < MAX_CHUNK_SIZE_ENCODE_IN_PLACE {
                            encoder.write(&chunk).map_err(EncoderError::Io)?;
                            let chunk = encoder.take();
                            *this.encoder = Some(encoder);

                            if !chunk.is_empty() {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                        } else {
                            *this.fut = Some(spawn_blocking(move || {
                                encoder.write(&chunk)?;
                                Ok(encoder)
                            }));
                        }
                    } else {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                }

                None => {
                    if let Some(encoder) = this.encoder.take() {
                        let chunk = encoder.finish().map_err(EncoderError::Io)?;
                        if chunk.is_empty() {
                            return Poll::Ready(None);
                        } else {
                            *this.eof = true;
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    } else {
                        return Poll::Ready(None);
                    }
                }
            }
        }
    }
}

fn update_head(encoding: ContentEncoding, head: &mut ResponseHead) {
    head.headers_mut().insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );
}

enum ContentEncoder {
    Deflate(ZlibEncoder<Writer>),
    Gzip(GzEncoder<Writer>),
    Br(BrotliEncoder<Writer>),
    // We need explicit 'static lifetime here because ZstdEncoder need lifetime
    // argument, and we use `spawn_blocking` in `Encoder::poll_next` that require `FnOnce() -> R + Send + 'static`
    Zstd(ZstdEncoder<'static, Writer>),
}

impl ContentEncoder {
    fn encoder(encoding: ContentEncoding) -> Option<Self> {
        match encoding {
            ContentEncoding::Deflate => Some(ContentEncoder::Deflate(ZlibEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Gzip => Some(ContentEncoder::Gzip(GzEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Br => {
                Some(ContentEncoder::Br(BrotliEncoder::new(Writer::new(), 3)))
            }
            ContentEncoding::Zstd => {
                let encoder = ZstdEncoder::new(Writer::new(), 3).ok()?;
                Some(ContentEncoder::Zstd(encoder))
            }
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn take(&mut self) -> Bytes {
        match *self {
            ContentEncoder::Br(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Deflate(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Gzip(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Zstd(ref mut encoder) => encoder.get_mut().take(),
        }
    }

    fn finish(self) -> Result<Bytes, io::Error> {
        match self {
            ContentEncoder::Br(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Gzip(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Deflate(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Zstd(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), io::Error> {
        match *self {
            ContentEncoder::Br(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding br encoding: {}", err);
                    Err(err)
                }
            },
            ContentEncoder::Gzip(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding gzip encoding: {}", err);
                    Err(err)
                }
            },
            ContentEncoder::Deflate(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding deflate encoding: {}", err);
                    Err(err)
                }
            },
            ContentEncoder::Zstd(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding ztsd encoding: {}", err);
                    Err(err)
                }
            },
        }
    }
}

#[derive(Debug, Display)]
#[non_exhaustive]
pub enum EncoderError<E> {
    #[display(fmt = "body")]
    Body(E),

    #[display(fmt = "boxed")]
    Boxed(Error),

    #[display(fmt = "blocking")]
    Blocking(BlockingError),

    #[display(fmt = "io")]
    Io(io::Error),
}

impl<E: StdError> StdError for EncoderError<E> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        None
    }
}

impl<E: Into<Error>> From<EncoderError<E>> for Error {
    fn from(err: EncoderError<E>) -> Self {
        match err {
            EncoderError::Body(err) => err.into(),
            EncoderError::Boxed(err) => err,
            EncoderError::Blocking(err) => err.into(),
            EncoderError::Io(err) => err.into(),
        }
    }
}
