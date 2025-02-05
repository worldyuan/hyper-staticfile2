/// 这里是虚拟文件系统
/// 基础类：
/// FileWithMetadata，主要是文件的元信息和具体的文件（文件句柄或者文件字节码），可能会是文件或者文件夹
/// 主要接口：
/// FileAccess : 提供读取的future接口，返回字节码（主要针对文件）
/// FileOpener ： 提供打开`path`文件的future接口，返回文件元信息（针对文件和文件夹）
/// 实现类：
/// TokioFileFuture：包装一个Future，返回`FileWithMetadata`
/// 
/// TokioFileOpener: 实现FileOpener，返回`FileWithMetadata<File>`(`TokioFuture`)
///     它主要支持打开一个`root`目录，使用根目录`root`和`path`相结合
/// TokioFileAccess：实现FileAccess.包装tokio::fs，返回文件元信息（TokioFuture）
///     主要针对的是单个文件
use std::cmp::min;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Cursor, Error, ErrorKind};
use std::mem::MaybeUninit;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{future::Future, time::SystemTime};

use futures_util::future::{ready, Ready};
use hyper::body::Bytes;
use tokio::fs::{self, File};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio::task::{spawn_blocking, JoinHandle};

const TOKIO_READ_BUF_SIZE: usize = 8 * 1024;

/// 文件元信息
#[derive(Debug)]
pub struct FileWithMetadata<F = File> {
    /// 实际文件
    /// 可能是文件句柄或者是文件的实际内容
    pub handle: F,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
}

/// 打开文件
pub trait FileOpener: Send + Sync + 'static {
    type File: IntoFileAccess;
    type Future: Future<Output = Result<FileWithMetadata<Self::File>, Error>> + Send;
    fn open(&self, path: &Path) -> Self::Future;
}

/// 转为读取文件
pub trait IntoFileAccess: Send + Unpin + 'static {
    type Output: FileAccess;
    fn into_file_access(self) -> Self::Output;
}

/// 读取文件接口
/// 其中AsyncSeek主要为了确保多线程环境中可以随机访问
/// Unpin：主要是表示内存可以安全的移动
/// Send: 主要表示所有权可以在不同线程中安全的移动，它是Future必须的
/// AsyncSeek : 根据游标读取文件
pub trait FileAccess: AsyncSeek + Send + Unpin + 'static {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        len: usize,
    ) -> Poll<Result<Bytes, Error>>;
}

impl IntoFileAccess for File {
    type Output = TokioFileAccess;

    fn into_file_access(self) -> Self::Output {
        TokioFileAccess::new(self)
    }
}

pub struct TokioFileAccess {
    file: File,
    read_buf: Box<[MaybeUninit<u8>; TOKIO_READ_BUF_SIZE]>,
}

impl TokioFileAccess {
    pub fn new(file: File) -> Self {
        TokioFileAccess {
            file,
            read_buf: Box::new([MaybeUninit::uninit(); TOKIO_READ_BUF_SIZE]),
        }
    }
}

impl AsyncSeek for TokioFileAccess {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.file).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.file).poll_complete(cx)
    }
}

impl FileAccess for TokioFileAccess {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        len: usize,
    ) -> Poll<Result<Bytes, Error>> {
        let Self {
            ref mut file,
            ref mut read_buf,
        } = *self;
        let len = min(len, read_buf.len());
        let mut read_buf = ReadBuf::uninit(&mut read_buf[..len]);
        match Pin::new(file).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    Poll::Ready(Ok(Bytes::new()))
                } else {
                    Poll::Ready(Ok(Bytes::copy_from_slice(filled)))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct TokioFileOpener {
    pub root: PathBuf,
}

impl TokioFileOpener {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FileOpener for TokioFileOpener {
    type File = File;
    type Future = TokioFileFuture;
    fn open(&self, path: &Path) -> Self::Future {
        let mut full_path = self.root.clone();
        full_path.extend(path);

        let inner = spawn_blocking(move || {
            let mut opts = OpenOptions::new();
            opts.read(true);

            // On Windows, we need to set this flag to be able to open directories.
            #[cfg(windows)]
            opts.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);

            let handle = opts.open(full_path)?;
            let metadata = handle.metadata()?;
            Ok(FileWithMetadata {
                handle: File::from_std(handle),
                size: metadata.len(),
                modified: metadata.modified().ok(),
                is_dir: metadata.is_dir(),
            })
        });

        TokioFileFuture { inner }
    }
}

/// 包装文件的Future，返回文件的元信息
/// 文件元信息中包含文件句柄
pub struct TokioFileFuture {
    inner: JoinHandle<Result<FileWithMetadata<File>, Error>>,
}

impl Future for TokioFileFuture {
    type Output = Result<FileWithMetadata<File>, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.inner).poll(cx) {
            Poll::Ready(Ok(res)) => Poll::Ready(res),
            Poll::Ready(Err(e)) => {
                Poll::Ready(Err(Error::new(ErrorKind::Other, "background task failed")))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// 内存文件
type MemoryFileMap = HashMap<PathBuf, FileWithMetadata<Bytes>>;

impl IntoFileAccess for Cursor<Bytes> {
    type Output = Self;
    fn into_file_access(self) -> Self::Output {
        self
    }
}

impl FileAccess for Cursor<Bytes> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        len: usize,
    ) -> Poll<Result<Bytes, Error>> {
        let pos = self.position();
        // 这里的*self应该是获取Pin内部的值，也就是获取到了&mut Cursor<Byte>
        // 然后.get_ref()是Cursor内部的实现，是获取Cursor内部的值的引用，也就是&Bytes
        let slice = (*self).get_ref();
        if pos > slice.len() as u64 {
            return Poll::Ready(Ok(Bytes::new()));
        }

        let start = pos as usize;
        // 这里计算需要读取的字节数，防止数组越界
        let amt = min(slice.len() - start, len);
        let end = start + amt;
        Poll::Ready(Ok(slice.slice(start..end)))
    }
}

pub struct MemoryFs {
    files: MemoryFileMap,
}

impl Default for MemoryFs {
    fn default() -> Self {
        let mut files = MemoryFileMap::new();
        files.insert(
            PathBuf::new(),
            FileWithMetadata {
                handle: Bytes::new(),
                size: 0,
                modified: None,
                is_dir: true,
            },
        );

        Self { files }
    }
}

impl MemoryFs {
    pub async fn from_dir(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut fs = Self::default();
        let mut dirs = vec![(path.as_ref().to_path_buf(), PathBuf::new())];
        while let Some((dir, base)) = dirs.pop() {
            let mut iter = fs::read_dir(dir).await?;
            while let Some(entry) = iter.next_entry().await? {
                let metadata = entry.metadata().await?;
                let mut out_path = base.to_path_buf();
                out_path.push(entry.file_name());

                if metadata.is_dir() {
                    dirs.push((entry.path(), out_path));
                } else if metadata.is_file() {
                    let data = fs::read(entry.path()).await?;
                    fs.add(out_path, data.into(), metadata.modified().ok());
                }
            }
        }
        Ok(fs)
    }

    pub fn add(
        &mut self,
        path: impl Into<PathBuf>,
        data: Bytes,
        modified: Option<SystemTime>,
    ) -> &mut Self {
        let path = path.into();

        // 建立文件夹
        let mut components: Vec<_> = path.components().collect();
        // 获取文件的文件夹
        components.pop();
        let mut dir_path = PathBuf::new();
        // 遍历文件的全部文件夹
        for component in components {
            if let Component::Normal(x) = component {
                dir_path.push(x);
                self.files.insert(
                    dir_path.clone(),
                    FileWithMetadata {
                        handle: Bytes::new(),
                        size: 0,
                        modified: None,
                        is_dir: true,
                    },
                );
            }
        }

        // 添加文件
        let size = data.len() as u64;
        self.files.insert(
            path,
            FileWithMetadata {
                handle: data,
                size,
                modified,
                is_dir: false,
            },
        );

        self
    }
}

impl FileOpener for MemoryFs {
    type File = Cursor<Bytes>;
    type Future = Ready<Result<FileWithMetadata<Self::File>, Error>>;
    fn open(&self, path: &Path) -> Self::Future {
        ready(
            self.files
                .get(path)
                .map(|file| FileWithMetadata {
                    handle: Cursor::new(file.handle.clone()),
                    size: file.size,
                    modified: file.modified,
                    is_dir: file.is_dir,
                })
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "Not Found")),
        )
    }
}
