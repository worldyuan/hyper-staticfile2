/// 主要时分段读取文件字节流，同时会输出分段头
use futures_util::Stream;
use http_range::HttpRange;
use hyper::body::Bytes;
use std::{
    io::{Error as IoError, SeekFrom},
    pin::Pin,
    task::Poll,
    vec,
};

use crate::vfs::{FileAccess, TokioFileAccess};
use std::fmt::Write;

/// 根据游标读取文件字节流
/// 主要调用`file: FileAccess`的poll_read方法,poll_read内置了根据游标读取，每次读取实时更新`remaining`，直到`remaining`为0
pub struct FileBytesStream<F = TokioFileAccess> {
    file: F,
    remaining: u64,
}

impl<F> FileBytesStream<F> {
    pub fn new(file: F) -> Self {
        Self {
            file,
            remaining: u64::MAX,
        }
    }

    pub fn new_with_limit(file: F, limit: u64) -> Self {
        Self {
            file,
            remaining: limit,
        }
    }
}

impl<F: FileAccess> Stream for FileBytesStream<F> {
    type Item = Result<Bytes, IoError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let Self {
            ref mut file,
            ref mut remaining,
        } = *self;

        match Pin::new(file).poll_read(cx, *remaining as usize) {
            Poll::Ready(Ok(buf)) => {
                *remaining -= buf.len() as u64;
                if buf.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(buf)))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(PartialEq, Eq)]
enum FileSeekState {
    NeedSeek,
    Seeking,
    Reading,
}

/// 包装`FileBytesStream`，主要增加了游标的起始位置
pub struct FileBytesStreamRange<F = TokioFileAccess> {
    file_stream: FileBytesStream<F>,
    seek_state: FileSeekState,
    start_offset: u64,
}

impl<F> FileBytesStreamRange<F> {
    pub fn new(file: F, range: HttpRange) -> Self {
        Self {
            file_stream: FileBytesStream::new_with_limit(file, range.length),
            seek_state: FileSeekState::NeedSeek,
            start_offset: range.start,
        }
    }

    fn without_initial_range(file: F) -> Self {
        Self {
            file_stream: FileBytesStream::new_with_limit(file, 0),
            seek_state: FileSeekState::NeedSeek,
            start_offset: 0,
        }
    }
}

impl<F: FileAccess> Stream for FileBytesStreamRange<F> {
    type Item = Result<Bytes, IoError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut file_stream,
            ref mut seek_state,
            start_offset,
        } = *self;

        // 初始化指针
        if *seek_state == FileSeekState::NeedSeek {
            *seek_state = FileSeekState::Seeking;
            if let Err(e) =
                Pin::new(&mut file_stream.file).start_seek(SeekFrom::Start(start_offset))
            {
                return Poll::Ready(Some(Err(e)));
            }
        }

        // 设置指针偏移
        if *seek_state == FileSeekState::Seeking {
            match Pin::new(&mut file_stream.file).poll_complete(cx) {
                Poll::Ready(Ok(..)) => *seek_state = FileSeekState::Reading,
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }

        // 读取真正的文件内容，返回字节码
        Pin::new(file_stream).poll_next(cx)
    }
}

/// 读取一个文件的多段，并自动组装响应体`boundary`
pub struct FileBytesStreamMultiRange<F = TokioFileAccess> {
    file_range: FileBytesStreamRange<F>,
    range_iter: vec::IntoIter<HttpRange>,
    is_first_boundary: bool,
    completed: bool,
    boundary: String,
    content_type: String,
    file_length: u64,
}

impl<F> FileBytesStreamMultiRange<F> {
    pub fn new(file: F, ranges: Vec<HttpRange>, boundary: String, file_length: u64) -> Self {
        Self {
            file_range: FileBytesStreamRange::without_initial_range(file),
            range_iter: ranges.into_iter(),
            boundary,
            is_first_boundary: true,
            completed: false,
            content_type: String::new(),
            file_length,
        }
    }

    pub fn set_content_type(&mut self, content_type: impl Into<String>) {
        self.content_type = content_type.into();
    }

    pub fn compute_length(&self) -> u64 {
        let Self {
            ref range_iter,
            ref boundary,
            ref content_type,
            file_length,
            ..
        } = *self;
        let mut total_length = 0;
        let mut is_first = true;
        for range in range_iter.as_slice() {
            let header =
                render_multipart_header(boundary, content_type, *range, is_first, file_length);
            is_first = false;
            total_length += header.as_bytes().len() as u64;
            total_length += range.length;
        }

        let header = render_multipart_header_end(boundary);
        total_length += header.as_bytes().len() as u64;
        total_length
    }
}

fn render_multipart_header(
    boundary: &str,
    content_type: &str,
    range: HttpRange,
    is_first: bool,
    file_length: u64,
) -> String {
    let mut buf = String::with_capacity(128);
    if !is_first {
        buf.push_str("\r\n");
    }
    write!(
        &mut buf,
        "--{boundary}\r\nContent-Range: {}-{}/{file_length}\r\n",
        range.start,
        range.start + range.length - 1
    )
    .expect("buffer write failed");

    if !content_type.is_empty() {
        write!(&mut buf, "Content-Type: {content_type}\r\n").expect("buffer write failed");
    }

    buf.push_str("\r\n");
    buf
}

fn render_multipart_header_end(boundary: &str) -> String {
    format!("\r\n--{boundary}--\r\n")
}

impl<F: FileAccess> Stream for FileBytesStreamMultiRange<F> {
    type Item = Result<Bytes, IoError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut file_range,
            ref mut range_iter,
            ref mut is_first_boundary,
            ref mut completed,
            ref boundary,
            ref content_type,
            file_length,
        } = *self;

        if *completed {
            return Poll::Ready(None);
        }

        if file_range.file_stream.remaining == 0 {
            let range = match range_iter.next() {
                Some(r) => r,
                None => {
                    *completed = true;
                    let header = render_multipart_header_end(boundary);
                    return Poll::Ready(Some(Ok(header.into())));
                }
            };

            file_range.seek_state = FileSeekState::NeedSeek;
            file_range.start_offset = range.start;
            file_range.file_stream.remaining = range.length;

            let cur_is_first = *is_first_boundary;
            *is_first_boundary = false;

            let header =
                render_multipart_header(boundary, content_type, range, cur_is_first, file_length);
            return Poll::Ready(Some(Ok(header.into())));
        }

        Pin::new(file_range).poll_next(cx)
    }
}

// include_dir
