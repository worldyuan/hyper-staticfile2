/// 解析器，获取到请求路径、获取请求文件元信息、编码等
use std::future::Future;
use std::{ops::BitAnd, path::PathBuf, sync::Arc, time::SystemTime};

use futures_util::future::BoxFuture;
use http::{header, HeaderValue, Method, Request};
use mime_guess::{mime, Mime, MimeGuess};
use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;
use std::io::Result as IoResult;
use tokio::fs::File;

use crate::util::RequestedPath;
use crate::vfs::FileOpener;
use crate::vfs::{FileWithMetadata, TokioFileOpener};

/// 文件解析结果
#[derive(Debug)]
pub struct ResolvedFile<F = File> {
    pub handle: F,
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub content_type: Option<String>,
    pub encoding: Option<Encoding>,
}

impl<F> ResolvedFile<F> {
    pub fn new(
        file: FileWithMetadata<F>,
        path: PathBuf,
        content_type: Option<String>,
        encoding: Option<Encoding>,
    ) -> Self {
        Self {
            handle: file.handle,
            path,
            size: file.size,
            modified: file.modified,
            content_type,
            encoding,
        }
    }
}

/// 解析者
pub struct Resolver<O = TokioFileOpener> {
    /// 打开文件
    pub opener: Arc<O>,
    /// 允许的编码
    pub allowed_encodings: AcceptEncoding,
    /// 重写解析参数
    pub rewrite: Option<Arc<dyn (Fn(ResolveParams) -> BoxRewriteFuture) + Send + Sync>>,
}

/// 重写解析参数的Future
pub type BoxRewriteFuture = BoxFuture<'static, IoResult<ResolveParams>>;

/// 解析所需参数
#[derive(Debug, Clone)]
pub struct ResolveParams {
    pub path: PathBuf,
    pub is_dir_request: bool,
    pub accept_encoding: AcceptEncoding,
}

/// 解析最终结果
#[derive(Debug)]
pub enum ResolveResult<F = File> {
    MethodNotMatched,
    NotFound,
    PermissionDenied,
    IsDirectory { redirect_to: String },
    Found(ResolvedFile<F>),
}

/// 将打开io错误映射为解析错误类型
fn map_open_err<F>(err: IoError) -> IoResult<ResolveResult<F>> {
    match err.kind() {
        IoErrorKind::NotFound => Ok(ResolveResult::NotFound),
        IoErrorKind::PermissionDenied => Ok(ResolveResult::PermissionDenied),
        _ => Err(err),
    }
}

impl Resolver<TokioFileOpener> {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_opener(TokioFileOpener::new(root))
    }
}

impl<O: FileOpener> Resolver<O> {
    pub fn with_opener(opener: O) -> Self {
        Self {
            opener: Arc::new(opener),
            allowed_encodings: AcceptEncoding::none(),
            rewrite: None,
        }
    }

    pub fn set_rewrite<R, F>(&mut self, rewrite: F) -> &mut Self
    where
        R: Future<Output = IoResult<ResolveParams>> + Send + 'static,
        F: (Fn(ResolveParams) -> R) + Send + Sync + 'static,
    {
        self.rewrite = Some(Arc::new(move |params| Box::pin(rewrite(params))));
        self
    }

    /// 解析Request
    pub async fn resovle_request<B>(&self, req: &Request<B>) -> IoResult<ResolveResult<O::File>> {
        // 解析请求方法
        match *req.method() {
            Method::HEAD | Method::GET => {}
            _ => {
                return Ok(ResolveResult::MethodNotMatched);
            }
        }

        // 解析编码
        let accept_encoding = self.allowed_encodings
            & req
                .headers()
                .get(header::ACCEPT_ENCODING)
                .map(AcceptEncoding::from_header_value)
                .unwrap_or(AcceptEncoding::none());
        // 解析路径
        self.resolve_path(req.uri().path(), accept_encoding).await
    }

    /// 解析路径
    pub async fn resolve_path(
        &self,
        request_path: &str,
        accept_encoding: AcceptEncoding,
    ) -> IoResult<ResolveResult<O::File>> {
        let requested_path = RequestedPath::resolve(request_path);
        let ResolveParams {
            mut path,
            is_dir_request,
            accept_encoding,
        } = {
            let mut params = ResolveParams {
                path: requested_path.sanitized,
                is_dir_request: requested_path.is_dir_request,
                accept_encoding,
            };
            if let Some(ref rewrite) = self.rewrite {
                params = rewrite(params).await?;
            }
            params
        };
        let file = match self.opener.open(&path).await {
            Ok(pair) => pair,
            Err(err) => return map_open_err(err),
        };

        if is_dir_request && !file.is_dir {
            return Ok(ResolveResult::NotFound);
        }

        if !is_dir_request && file.is_dir {
            let mut target = String::with_capacity(path.as_os_str().len() + 2);
            target.push('/');
            for component in path.components() {
                target.push_str(&component.as_os_str().to_string_lossy());
                target.push('/');
            }
            return Ok(ResolveResult::IsDirectory {
                redirect_to: target,
            });
        }

        if !is_dir_request {
            return self.resolve_final(file, path, accept_encoding).await;
        }

        path.push("index.html");
        let file = match self.opener.open(&path).await {
            Ok(pair) => pair,
            Err(err) => return map_open_err(err),
        };

        if file.is_dir {
            return Ok(ResolveResult::NotFound);
        }

        self.resolve_final(file, path, accept_encoding).await
    }

    /// 解析最终结果
    async fn resolve_final(
        &self,
        file: FileWithMetadata<O::File>,
        path: PathBuf,
        accept_encoding: AcceptEncoding,
    ) -> IoResult<ResolveResult<O::File>> {
        let mimetype = MimeGuess::from_path(&path)
            .first()
            .map(|mimetype| set_charset(mimetype).to_string());

        if accept_encoding.zstd {
            let mut zstd_path = path.clone().into_os_string();
            zstd_path.push("zst");
            if let Ok(file) = self.opener.open(zstd_path.as_ref()).await {
                return Ok(ResolveResult::Found(ResolvedFile::new(
                    file,
                    zstd_path.into(),
                    mimetype,
                    Some(Encoding::Zstd),
                )));
            }
        }

        if accept_encoding.br {
            let mut br_path = path.clone().into_os_string();
            br_path.push(".br");
            if let Ok(file) = self.opener.open(br_path.as_ref()).await {
                return Ok(ResolveResult::Found(ResolvedFile::new(
                    file,
                    br_path.into(),
                    mimetype,
                    Some(Encoding::Br),
                )));
            }
        }
        if accept_encoding.gzip {
            let mut gzip_path = path.clone().into_os_string();
            gzip_path.push(".gz");
            if let Ok(file) = self.opener.open(gzip_path.as_ref()).await {
                return Ok(ResolveResult::Found(ResolvedFile::new(
                    file,
                    gzip_path.into(),
                    mimetype,
                    Some(Encoding::Gzip),
                )));
            }
        }

        Ok(ResolveResult::Found(ResolvedFile::new(
            file, path, mimetype, None,
        )))
    }
}

impl<O> Clone for Resolver<O> {
    fn clone(&self) -> Self {
        Self {
            opener: self.opener.clone(),
            allowed_encodings: self.allowed_encodings,
            rewrite: self.rewrite.clone(),
        }
    }
}

/// 编码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Gzip,
    Br,
    Zstd,
}

impl Encoding {
    pub fn to_header_value(&self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Encoding::Gzip => "gzip",
            Encoding::Br => "br",
            Encoding::Zstd => "zstd",
        })
    }
}

/// 编码状态
#[derive(Debug, Copy, Clone)]
pub struct AcceptEncoding {
    pub gzip: bool,
    pub br: bool,
    pub zstd: bool,
}

impl AcceptEncoding {
    pub const fn all() -> Self {
        Self {
            gzip: true,
            br: true,
            zstd: true,
        }
    }

    pub const fn none() -> Self {
        Self {
            gzip: false,
            br: false,
            zstd: false,
        }
    }

    pub fn from_header_value(value: &HeaderValue) -> Self {
        let mut res = Self::none();
        if let Ok(value) = value.to_str() {
            for enc in value.split(",") {
                match enc.split(";").next().unwrap().trim() {
                    "gzip" => res.gzip = true,
                    "br" => res.br = true,
                    "zstd" => res.zstd = true,
                    _ => {}
                }
            }
        }
        res
    }
}

impl BitAnd for AcceptEncoding {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self {
            gzip: self.gzip && rhs.gzip,
            br: self.br && rhs.br,
            zstd: self.zstd && rhs.zstd,
        }
    }
}

fn set_charset(mimetype: Mime) -> Mime {
    if mimetype == mime::APPLICATION_JAVASCRIPT {
        return mime::APPLICATION_JAVASCRIPT_UTF_8;
    }
    if mimetype == mime::TEXT_JAVASCRIPT {
        return "text/javascript; charset=utf-8".parse().unwrap();
    }
    mimetype
}
