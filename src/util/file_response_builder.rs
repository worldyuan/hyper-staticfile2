use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{
    header, response::Builder as HttpResponseBuilder, HeaderMap, Method, Request, Response, Result,
    StatusCode,
};
use http_range::{HttpRange, HttpRangeParseError};
use rand::{rng, seq::IndexedRandom, thread_rng};

use crate::{body::Body, resolve::ResolvedFile, vfs::IntoFileAccess};

use super::{FileBytesStream, FileBytesStreamMultiRange, FileBytesStreamRange};

const MIN_VALID_MTIME: Duration = Duration::from_secs(2);
const BOUNDARY_LENGTH: usize = 60;
const BOUNDARY_CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Clone, Debug, Default)]
pub struct FileResponseBuilder {
    pub cache_headers: Option<u32>,
    pub is_head: bool,
    pub if_modified_since: Option<SystemTime>,
    pub range: Option<String>,
    pub if_range: Option<String>,
}

impl FileResponseBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request<B>(&mut self, req: Request<B>) -> &mut Self {
        self.request_parts(req.method(), req.headers());
        self
    }

    pub fn request_parts(&mut self, method: &Method, headers: &HeaderMap) -> &mut Self {
        self.request_method(method);
        self.request_heanders(headers);
        self
    }

    pub fn request_method(&mut self, method: &Method) -> &mut Self {
        self.is_head(*method == Method::HEAD);
        self
    }

    pub fn request_heanders(&mut self, headers: &HeaderMap) -> &mut Self {
        self.if_modified_since_header(headers.get(header::IF_MODIFIED_SINCE));
        self.range_header(headers.get(header::RANGE));
        self.if_range(headers.get(header::IF_RANGE));
        self
    }

    pub fn cache_headers(&mut self, value: Option<u32>) -> &mut Self {
        self.cache_headers = value;
        self
    }

    pub fn is_head(&mut self, value: bool) -> &mut Self {
        self.is_head = value;
        self
    }

    pub fn if_modified_since(&mut self, value: Option<SystemTime>) -> &mut Self {
        self.if_modified_since = value;
        self
    }

    pub fn if_modified_since_header(&mut self, value: Option<&header::HeaderValue>) -> &mut Self {
        self.if_modified_since = value
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok());
        self
    }

    pub fn if_range(&mut self, value: Option<&header::HeaderValue>) -> &mut Self {
        if let Some(s) = value.and_then(|s| s.to_str().ok()) {
            self.if_range = Some(s.to_string());
        }
        self
    }

    pub fn range_header(&mut self, value: Option<&header::HeaderValue>) -> &mut Self {
        self.range = value.and_then(|v| v.to_str().ok()).map(|v| v.to_string());
        self
    }

    pub fn build<F: IntoFileAccess>(
        &self,
        file: ResolvedFile<F>,
    ) -> Result<Response<Body<F::Output>>> {
        let mut res = HttpResponseBuilder::new();
        let modified = file.modified.filter(|v| {
            v.duration_since(UNIX_EPOCH)
                .ok()
                .filter(|v| v >= &MIN_VALID_MTIME)
                .is_some()
        });
        let mut range_cond_ok = self.if_range.is_none();
        if let Some(modified) = modified {
            if let Ok(modified_unix) = modified.duration_since(UNIX_EPOCH) {
                if let Some(Ok(is_unix)) =
                    self.if_modified_since.map(|v| v.duration_since(UNIX_EPOCH))
                {
                    return HttpResponseBuilder::new()
                        .status(StatusCode::NOT_MODIFIED)
                        .body(Body::Empty);
                }

                let etag = format!(
                    "w/\"{0:x}-{1:x}.{2:x}\"",
                    file.size,
                    modified_unix.as_secs(),
                    modified_unix.subsec_nanos()
                );

                if let Some(ref v) = self.if_range {
                    if *v == etag {
                        range_cond_ok = true;
                    }
                }

                res = res.header(header::ETAG, etag);
            }

            let last_modified_formatted = httpdate::fmt_http_date(modified);
            if let Some(ref v) = self.if_range {
                if *v == last_modified_formatted {
                    range_cond_ok = true;
                }
            }

            res = res
                .header(header::LAST_MODIFIED, last_modified_formatted)
                .header(header::ACCEPT_RANGES, "bytes");
        }

        if let Some(seconds) = self.cache_headers {
            res = res.header(
                header::CACHE_CONTROL,
                format!("public, max-age={}", seconds),
            );
        }

        if self.is_head {
            res = res.header(header::CONTENT_LENGTH, format!("{}", file.size));
            return res.status(StatusCode::OK).body(Body::Empty);
        }

        let ranges = self.range.as_ref().filter(|_| range_cond_ok).and_then(|r| {
            match HttpRange::parse(r, file.size) {
                Ok(r) => Some(Ok(r)),
                Err(HttpRangeParseError::NoOverlap) => Some(Err(())),
                Err(HttpRangeParseError::InvalidRange) => None,
            }
        });

        if let Some(ranges) = ranges {
            let ranges = match ranges {
                Ok(r) => r,
                Err(err) => {
                    return res
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .body(Body::Empty);
                }
            };

            if ranges.len() == 1 {
                let single_span = ranges[0];
                res = res
                    .header(
                        header::CONTENT_RANGE,
                        content_range_header(&single_span, file.size),
                    )
                    .header(header::CONTENT_LENGTH, format!("{}", single_span.length));

                let body_stream =
                    FileBytesStreamRange::new(file.handle.into_file_access(), single_span);
                return res
                    .status(StatusCode::PARTIAL_CONTENT)
                    .body(Body::Range(body_stream));
            } else if ranges.len() > 1 {
                let mut boundary_tmp = [0u8; BOUNDARY_LENGTH];
                let mut rng = rng();

                for v in boundary_tmp.iter_mut() {
                    *v = *BOUNDARY_CHARS.choose(&mut rng).unwrap();
                }

                let boundary = std::str::from_utf8(&boundary_tmp[..]).unwrap().to_string();
                res = res.header(
                    hyper::header::CONTENT_TYPE,
                    format!("multipart/byteranges; boundary={}", boundary),
                );
                let mut body_stream = FileBytesStreamMultiRange::new(
                    file.handle.into_file_access(),
                    ranges,
                    boundary,
                    file.size,
                );
                if let Some(content_type) = file.content_type.as_ref() {
                    body_stream.set_content_type(content_type);
                }

                res = res.header(
                    hyper::header::CONTENT_LENGTH,
                    format!("{}", body_stream.compute_length()),
                );

                return res
                    .status(StatusCode::PARTIAL_CONTENT)
                    .body(Body::MultiRange(body_stream));
            }
        }

        res = res.header(header::CONTENT_LENGTH, format!("{}", file.size));
        if let Some(content_type) = file.content_type {
            res = res.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(encoding) = file.encoding {
            res = res.header(header::CONTENT_ENCODING, encoding.to_header_value());
        }

        res.status(StatusCode::OK)
            .body(Body::Full(FileBytesStream::new_with_limit(
                file.handle.into_file_access(),
                file.size,
            )))
    }
}

fn content_range_header(r: &HttpRange, total_length: u64) -> String {
    format!(
        "bytes {}-{}/{}",
        r.start,
        r.start + r.length - 1,
        total_length
    )
}
