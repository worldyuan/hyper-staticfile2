use http::{header, response::Builder as ResponseBuilder, Request, Response, StatusCode};
use hyper::{body::Bytes, service::service_fn};
use hyper_staticfile::{vfs::MemoryFs, Body, Static};
use hyper_util::rt::TokioIo;
use include_dir::include_dir;
use std::{
    io::{Cursor, Error as IoError},
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::net::TcpListener;

async fn handle_request<B>(req: Request<B>, static_: Static<MemoryFs>) -> Result<Response<Body<Cursor<Bytes>>>, IoError> {
    if req.uri().path() == "/" {
        let res = ResponseBuilder::new()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, "/hyper_staticfile/")
            .body(Body::Empty)
            .expect("unable to build response");
        Ok(res)
    } else {
        static_.clone().serve(req).await
    }
}

static EMBED_FS: &'static include_dir::Dir = &include_dir!("target/doc");

#[tokio::main]
async fn main() {
    let static_ = Static::from_memory_fs(EMBED_FS);
    let addr: SocketAddr = ([127, 0, 0, 1], 3000).into();
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to create TCP connection");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .expect("Failed to accept TCP Connection");
        let static_ = static_.clone();
        tokio::spawn(async move {
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| handle_request(req, static_.clone())),
                )
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
