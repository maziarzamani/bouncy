//! Tiny in-process fixture for tests + smoke runs.
//!
//! Spawns a hyper server on a random port that serves a couple of
//! HTML pages with a path-based router. We use the same shape as
//! the integration tests in `bouncy-browse`: a `tokio::spawn` that
//! lives until the server's TCP listener is dropped.
//!
//! This isn't a substitute for a real WebArena fixture — it just
//! lets the harness's smoke test run end-to-end without docker.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Spawn a hyper server with a path-based router. Returns the
/// bound address — drop the returned guard / spawned task to stop.
/// (We don't return a guard today — the test process exits at end.)
pub async fn spawn_router(routes: Vec<(&'static str, &'static str)>) -> SocketAddr {
    let routes = Arc::new(routes);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let routes = routes.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        let body: &'static str = routes
                            .iter()
                            .find(|(p, _)| *p == path)
                            .map(|(_, b)| *b)
                            .unwrap_or("<html><body>404</body></html>");
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/html")
                                .body(Full::new(Bytes::from_static(body.as_bytes())))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}
