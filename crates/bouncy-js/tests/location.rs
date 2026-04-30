//! Tests for `window.location` and `document.URL` getters.
//!
//! TDD-red against the current bootstrap (no `location` polyfill). After
//! adding the bridge native + JS proxy, all four go green.

use std::sync::Arc;

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;

fn make_rt() -> Runtime {
    Runtime::new(
        tokio::runtime::Handle::current(),
        Arc::new(Fetcher::new().expect("fetcher")),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_href_returns_full_url() {
    let mut rt = make_rt();
    rt.load(
        "<html><body></body></html>",
        "https://example.test/path/x?y=1#frag",
    )
    .unwrap();
    assert_eq!(
        rt.eval("location.href").unwrap(),
        "https://example.test/path/x?y=1#frag"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_parts_break_url_apart() {
    let mut rt = make_rt();
    rt.load(
        "<html><body></body></html>",
        "https://example.test:8443/p?q=1#frag",
    )
    .unwrap();
    assert_eq!(rt.eval("location.protocol").unwrap(), "https:");
    assert_eq!(rt.eval("location.host").unwrap(), "example.test:8443");
    assert_eq!(rt.eval("location.hostname").unwrap(), "example.test");
    assert_eq!(rt.eval("location.port").unwrap(), "8443");
    assert_eq!(rt.eval("location.pathname").unwrap(), "/p");
    assert_eq!(rt.eval("location.search").unwrap(), "?q=1");
    assert_eq!(rt.eval("location.hash").unwrap(), "#frag");
    assert_eq!(
        rt.eval("location.origin").unwrap(),
        "https://example.test:8443"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_url_matches_location_href() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let a = rt.eval("location.href").unwrap();
    let b = rt.eval("document.URL").unwrap();
    assert_eq!(a, b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_updates_after_load() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://a.test/")
        .unwrap();
    assert_eq!(rt.eval("location.host").unwrap(), "a.test");
    rt.load("<html><body></body></html>", "https://b.test:9999/x")
        .unwrap();
    assert_eq!(rt.eval("location.host").unwrap(), "b.test:9999");
    assert_eq!(rt.eval("location.pathname").unwrap(), "/x");
}
