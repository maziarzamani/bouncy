//! Phase 1 microbench: bouncy-fetch vs reqwest, both with hot connection pools.
//!
//! Spins one in-process hyper server on 127.0.0.1:0, lets each client warm
//! up a single TCP+keep-alive connection, then benchmarks the steady-state
//! GET path. The recipe gate: bouncy-fetch must beat reqwest by ≥10% on p50.

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use bouncy_fetch::Fetcher;

async fn spawn_server(payload_len: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let payload = Bytes::from(vec![b'x'; payload_len]);

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let payload = payload.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| {
                    let payload = payload.clone();
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/plain")
                                .body(Full::new(payload))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    addr
}

fn build_clients() -> (Fetcher, reqwest::Client) {
    let fetcher = Fetcher::new().expect("build fetcher");
    let reqwest_client = reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .build()
        .expect("build reqwest");
    (fetcher, reqwest_client)
}

async fn warm_up(fetcher: &Fetcher, rc: &reqwest::Client, url: &str) {
    for _ in 0..8 {
        let _ = fetcher.get(url).await.unwrap();
    }
    for _ in 0..8 {
        let _ = rc.get(url).send().await.unwrap().bytes().await.unwrap();
    }
}

fn bench_pooled_get(c: &mut Criterion, name: &str, payload_len: usize) {
    let rt = Runtime::new().unwrap();
    let addr = rt.block_on(spawn_server(payload_len));
    let url = format!("http://{}/x", addr);
    let (fetcher, rc) = build_clients();
    rt.block_on(warm_up(&fetcher, &rc, &url));

    let mut group = c.benchmark_group(name);
    group.throughput(criterion::Throughput::Bytes(payload_len as u64));

    group.bench_function("bouncy_fetch", |b| {
        b.to_async(&rt).iter(|| async {
            let r = fetcher.get(&url).await.unwrap();
            assert_eq!(r.status, 200);
            r
        });
    });

    group.bench_function("reqwest", |b| {
        b.to_async(&rt).iter(|| async {
            let r = rc.get(&url).send().await.unwrap();
            let status = r.status().as_u16();
            let body = r.bytes().await.unwrap();
            assert_eq!(status, 200);
            body
        });
    });

    group.finish();
}

fn bench_concurrent_16(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let addr = rt.block_on(spawn_server(4 * 1024));
    let url = format!("http://{}/x", addr);
    let (fetcher, rc) = build_clients();
    rt.block_on(warm_up(&fetcher, &rc, &url));

    let mut group = c.benchmark_group("concurrent_16x_4kb");
    group.throughput(criterion::Throughput::Bytes((4 * 1024 * 16) as u64));

    group.bench_function("bouncy_fetch", |b| {
        b.to_async(&rt).iter(|| async {
            let mut futs = Vec::with_capacity(16);
            for _ in 0..16 {
                futs.push(fetcher.get(&url));
            }
            let results = futures_util::future::join_all(futs).await;
            for r in results {
                assert_eq!(r.unwrap().status, 200);
            }
        });
    });

    group.bench_function("reqwest", |b| {
        b.to_async(&rt).iter(|| async {
            let mut futs = Vec::with_capacity(16);
            for _ in 0..16 {
                futs.push(async { rc.get(&url).send().await.unwrap().bytes().await.unwrap() });
            }
            let _ = futures_util::future::join_all(futs).await;
        });
    });

    group.finish();
}

fn bench_4kb(c: &mut Criterion) {
    bench_pooled_get(c, "pooled_get_4kb", 4 * 1024);
}

fn bench_tiny(c: &mut Criterion) {
    bench_pooled_get(c, "pooled_get_tiny", 32);
}

criterion_group!(benches, bench_tiny, bench_4kb, bench_concurrent_16);
criterion_main!(benches);
