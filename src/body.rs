use futures_util::Stream;
use hyper::body::{Bytes, Frame};
use std::{io::Error as IoError, pin::Pin, task::{ready, Poll}};

use crate::{
    util::{FileBytesStream, FileBytesStreamMultiRange, FileBytesStreamRange},
    vfs::{FileAccess, TokioFileAccess},
};

pub enum Body<F = TokioFileAccess> {
    Empty,
    Full(FileBytesStream<F>),
    Range(FileBytesStreamRange<F>),
    MultiRange(FileBytesStreamMultiRange<F>),
}

impl<F: FileAccess> hyper::body::Body for Body<F> {
    type Data = Bytes;
    type Error = IoError;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let opt = ready!(match *self{
            Body::Empty => return Poll::Ready(None),
            Body::Full(ref mut stream) => Pin::new(stream).poll_next(cx),
            Body::Range(ref mut stream) => Pin::new(stream).poll_next(cx),
            Body::MultiRange(ref mut stream) => Pin::new(stream).poll_next(cx),
        }) ;
        Poll::Ready(opt.map(|res| res.map(Frame::data)))
    }
}
