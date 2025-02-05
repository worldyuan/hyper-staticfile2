use std::path::{Component, Path, PathBuf};

fn decode_percents(string: &str) -> String {
    percent_encoding::percent_decode_str(string)
        .decode_utf8_lossy()
        .into_owned()
}

/// 标准化路径，防止攻击
fn sanitize_path(path: &Path) -> PathBuf {
    path.components().fold(PathBuf::new(), |mut result, p| match p {
        Component::Normal(x) => {
            if Path::new(&x).components().all(|c| matches!(c, Component::Normal(_))) {
                result.push(x);
            }
            result
        }
        Component::ParentDir => {
            result.pop();
            result
        }
        _ => result,
    })
}

pub struct RequestedPath{
    pub sanitized: PathBuf,
    pub is_dir_request: bool,
}

impl RequestedPath{
    pub fn resolve(request_path: &str) -> Self {
        let is_dir_request = request_path.as_bytes().last() == Some(&b'/');
        let request_path = PathBuf::from(decode_percents(request_path));
        RequestedPath {
            sanitized: sanitize_path(&request_path),
            is_dir_request
        }
    }
}

