//! Response decompression tests.
//!
//! Real CDNs return compressed bodies whether you ask or not. The
//! Fetcher should:
//!   1. Send `Accept-Encoding: gzip, deflate, br` by default so servers
//!      do compress (saves bandwidth + matches real-browser shape).
//!   2. Decode `Content-Encoding: gzip|deflate|br` responses transparently.
//!   3. Strip the encoding-specific headers from the returned `Response`
//!      so callers don't double-decode or get confused by the wrong
//!      Content-Length.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use bouncy_fetch::{FetchRequest, Fetcher};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const PLAIN: &str = "compressed-payload-roundtrip-marker";

fn gzip_encode(s: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(s).unwrap();
    e.finish().unwrap()
}

fn deflate_encode(s: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(s).unwrap();
    e.finish().unwrap()
}

fn br_encode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
    writer.write_all(s).unwrap();
    writer.flush().unwrap();
    drop(writer);
    out
}

async fn spawn_compressed_server() -> (SocketAddr, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let last_accept_encoding: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let lae = last_accept_encoding.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let lae2 = lae.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let lae3 = lae2.clone();
                    async move {
                        // Stash whatever Accept-Encoding the client sent.
                        let ae = req
                            .headers()
                            .get("accept-encoding")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        *lae3.lock().unwrap() = ae;

                        let path = req.uri().path().to_string();
                        let (encoding, body): (&str, Vec<u8>) = match path.as_str() {
                            "/gzip" => ("gzip", gzip_encode(PLAIN.as_bytes())),
                            "/deflate" => ("deflate", deflate_encode(PLAIN.as_bytes())),
                            "/br" => ("br", br_encode(PLAIN.as_bytes())),
                            _ => ("identity", PLAIN.as_bytes().to_vec()),
                        };
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-encoding", encoding)
                                .header("content-length", body.len().to_string())
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    (addr, last_accept_encoding)
}

#[tokio::test]
async fn fetcher_sends_accept_encoding_by_default() {
    let (addr, lae) = spawn_compressed_server().await;
    let f = Fetcher::new().unwrap();
    let _ = f
        .request(FetchRequest::new(format!("http://{addr}/")))
        .await
        .unwrap();
    let ae = lae.lock().unwrap().clone();
    let ae = ae.expect("Accept-Encoding header missing");
    assert!(ae.contains("gzip"), "got: {ae}");
    assert!(ae.contains("deflate"), "got: {ae}");
    assert!(ae.contains("br"), "got: {ae}");
}

#[tokio::test]
async fn fetcher_decodes_gzip() {
    let (addr, _) = spawn_compressed_server().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/gzip")))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], PLAIN.as_bytes(), "body not decoded");
    // Encoding-specific headers must not survive into the returned Response.
    assert!(
        r.headers.get("content-encoding").is_none(),
        "Content-Encoding should be stripped after decode"
    );
}

#[tokio::test]
async fn fetcher_decodes_deflate() {
    let (addr, _) = spawn_compressed_server().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/deflate")))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], PLAIN.as_bytes());
}

#[tokio::test]
async fn fetcher_decodes_brotli() {
    let (addr, _) = spawn_compressed_server().await;
    let f = Fetcher::new().unwrap();
    let r = f
        .request(FetchRequest::new(format!("http://{addr}/br")))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], PLAIN.as_bytes());
}

#[tokio::test]
async fn user_supplied_accept_encoding_is_respected() {
    // If the caller explicitly sets Accept-Encoding (e.g. `identity`),
    // we don't override it. Decode logic still kicks in based on the
    // actual Content-Encoding the server returns.
    let (addr, lae) = spawn_compressed_server().await;
    let f = Fetcher::new().unwrap();
    let _ = f
        .request(FetchRequest::new(format!("http://{addr}/")).header("Accept-Encoding", "identity"))
        .await
        .unwrap();
    let ae = lae.lock().unwrap().clone();
    assert_eq!(ae.as_deref(), Some("identity"));
}
