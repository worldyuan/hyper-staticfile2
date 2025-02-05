use std::future::Future;
use std::path::PathBuf;
use std::{io::Error as IoError, pin::Pin};

use http::{Request, Response};
use hyper::service::Service;

use crate::vfs::MemoryFs;
use crate::{
    vfs::{FileOpener, IntoFileAccess, TokioFileOpener},
    AcceptEncoding, Body, Resolver, ResponseBuilder,
};

pub struct Static<O = TokioFileOpener> {
    pub resolver: Resolver<O>,
    pub cache_headers: Option<u32>,
}

impl Static<TokioFileOpener> {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            resolver: Resolver::new(root),
            cache_headers: None,
        }
    }
}

impl Static<MemoryFs> {
    pub fn from_memory_fs(fs: impl Into<MemoryFs>) -> Self {
        Self {
            resolver: Resolver::from_memory_fs(fs),
            cache_headers: None,
        }
    }
}

impl<O: FileOpener> Static<O> {
    pub fn with_opener(opener: O) -> Self {
        Self {
            resolver: Resolver::with_opener(opener),
            cache_headers: None,
        }
    }

    pub fn cache_headers(&mut self, value: Option<u32>) -> &mut Self {
        self.cache_headers = value;
        self
    }

    pub fn allowed_encodings(&mut self, allowed_encodings: AcceptEncoding) -> &mut Self {
        self.resolver.allowed_encodings = allowed_encodings;
        self
    }

    pub async fn serve<B>(
        self,
        request: Request<B>,
    ) -> Result<Response<Body<<O::File as IntoFileAccess>::Output>>, IoError> {
        let Self {
            resolver,
            cache_headers,
        } = self;
        resolver.resovle_request(&request).await.map(|result| {
            ResponseBuilder::new()
                .request(&request)
                .cache_headers(cache_headers)
                .build(result)
                .expect("unable to build response")
        })
    }
}

impl<O> Clone for Static<O> {
    fn clone(&self) -> Self {
        Self {
            resolver: self.resolver.clone(),
            cache_headers: self.cache_headers,
        }
    }
}

impl<O, B> Service<Request<B>> for Static<O>
where
    O: FileOpener,
    B: Send + Sync + 'static,
{
    /// 返回文件对应的输出
    type Response = Response<Body<<O::File as IntoFileAccess>::Output>>;
    type Error = IoError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<B>) -> Self::Future {
        Box::pin(self.clone().serve(request))
    }
}
