use http::{header, response::Builder as ResponseBuilder, Request, Response, StatusCode};
use hyper::service::service_fn;
use hyper_staticfile::{Body, Static};
use hyper_util::rt::TokioIo;
use std::{
    io::Error as IoError,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::net::TcpListener;

async fn handle_request<B>(req: Request<B>, static_: Static) -> Result<Response<Body>, IoError> {
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

#[tokio::main]
async fn main() {
    let static_ = Static::new(Path::new("target/doc/"));
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
