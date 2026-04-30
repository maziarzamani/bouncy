//! End-to-end tests for bootstrap.js polyfills running against the bridge.

use std::sync::Arc;

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;

fn make_rt() -> Runtime {
    let handle = tokio::runtime::Handle::current();
    let fetcher = Arc::new(Fetcher::new().expect("build fetcher"));
    Runtime::new(handle, fetcher)
}
const PAGE: &str = r#"<!doctype html>
<html><head><title>Demo</title></head>
<body>
  <h1 id="title">Hello</h1>
  <ul id="items">
    <li class="item" data-id="1">A</li>
  </ul>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_title_via_polyfill() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let v = rt.eval("document.title").unwrap();
    assert_eq!(v, "Demo");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_element_by_id_returns_element_wrapper() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let v = rt.eval("document.getElementById('title').tagName").unwrap();
    assert_eq!(v, "H1");
    let v = rt
        .eval("document.getElementById('title').textContent.trim()")
        .unwrap();
    assert_eq!(v, "Hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn class_name_round_trip() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let v = rt
        .eval("document.getElementById('items').children[0].className")
        .unwrap();
    assert_eq!(v, "item");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dataset_proxy_get_and_set() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let v = rt
        .eval("document.getElementById('items').children[0].dataset.id")
        .unwrap();
    assert_eq!(v, "1");
    rt.eval("document.body.dataset.ready = '1'").unwrap();
    let html = rt.dump_html().unwrap();
    assert!(html.contains("data-ready=\"1\""), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_append_via_polyfill() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    rt.eval(
        r#"
        const ul = document.getElementById('items');
        const li = document.createElement('li');
        li.className = 'item';
        li.dataset.id = '42';
        li.textContent = 'NEW';
        ul.appendChild(li);
        "#,
    )
    .unwrap();
    let html = rt.dump_html().unwrap();
    assert!(html.contains("data-id=\"42\""), "got: {html}");
    assert!(html.contains("NEW"), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inner_html_setter_round_trips() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    rt.eval(
        r#"
        document.getElementById('title').innerHTML = 'New <em>x</em>';
        "#,
    )
    .unwrap();
    let html = rt.dump_html().unwrap();
    assert!(html.contains("<em>x</em>"), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_runtime_per_load() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><h1 id='t'>A</h1></body></html>",
        "http://test.local/",
    )
    .unwrap();
    assert_eq!(
        rt.eval("document.getElementById('t').textContent").unwrap(),
        "A"
    );

    rt.load(
        "<html><body><h1 id='t'>B</h1></body></html>",
        "http://test.local/",
    )
    .unwrap();
    assert_eq!(
        rt.eval("document.getElementById('t').textContent").unwrap(),
        "B"
    );
}
