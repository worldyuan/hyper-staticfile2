use http::{
    header, response::Builder as HttpResponseBuilder, HeaderMap, Method, Request, Response, Result, StatusCode, Uri
};

use crate::{body::Body, resolve::ResolveResult, util::FileResponseBuilder, vfs::IntoFileAccess};

#[derive(Clone, Debug, Default)]
pub struct ResponseBuilder<'a> {
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub file_response_builder: FileResponseBuilder,
}

impl<'a> ResponseBuilder<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request<B>(&mut self, req: &'a Request<B>) -> &mut Self {
        self.request_parts(req.method(), req.uri(), req.headers());
        self
    }

    pub fn request_parts(
        &mut self,
        method: &Method,
        uri: &'a Uri,
        headers: &'a HeaderMap,
    ) -> &mut Self {
        self.request_uri(uri);
        self.file_response_builder.request_parts(method, headers);
        self
    }

    pub fn request_uri(&mut self, uri: &'a Uri) -> &mut Self {
        self.path(uri.path());
        self.query(uri.query());
        self
    }

    pub fn cache_headers(&mut self, value: Option<u32>) -> &mut Self {
        self.file_response_builder.cache_headers(value);
        self
    }

    pub fn path(&mut self, value: &'a str) -> &mut Self {
        self.path = value;
        self
    }

    pub fn query(&mut self, value: Option<&'a str>) -> &mut Self {
        self.query = value;
        self
    }

    pub fn build<F: IntoFileAccess>(
        &self,
        result: ResolveResult<F>,
    ) -> Result<Response<Body<F::Output>>> {
        match result {
            ResolveResult::MethodNotMatched => HttpResponseBuilder::new()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::Empty),
            ResolveResult::NotFound => HttpResponseBuilder::new().status(StatusCode::NOT_FOUND).body(Body::Empty),
            ResolveResult::PermissionDenied => HttpResponseBuilder::new().status(StatusCode::FORBIDDEN).body(Body::Empty),
            ResolveResult::IsDirectory { redirect_to: mut target } => {
                if let Some(query) = self.query {
                    target.push('?');
                    target.push_str(query);
                }
                HttpResponseBuilder::new().status(StatusCode::MOVED_PERMANENTLY).header(header::LOCATION, target).body(Body::Empty)
            }
            ResolveResult::Found(file) => self.file_response_builder.build(file),
        }
    }
}
