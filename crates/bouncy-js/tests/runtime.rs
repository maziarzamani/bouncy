//! End-to-end tests for the `Runtime` — V8 + DOM bridge against fixture HTML.

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
async fn js_can_read_document_title_via_native_callback() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let v = rt.eval("__bouncy_doc_title()").unwrap();
    assert_eq!(v, "Demo");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_can_get_element_and_read_attributes() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    let nid = rt.eval("__bouncy_doc_get_element_by_id('title')").unwrap();
    assert_ne!(nid, "-1", "expected positive node id");
    let tag = rt
        .eval("__bouncy_node_tag_name(__bouncy_doc_get_element_by_id('title'))")
        .unwrap();
    assert_eq!(tag, "h1");
    let txt = rt
        .eval("__bouncy_node_text_content(__bouncy_doc_get_element_by_id('title')).trim()")
        .unwrap();
    assert_eq!(txt, "Hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_mutations_round_trip_to_dump_html() {
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    rt.eval(
        r#"
        const t = __bouncy_doc_get_element_by_id('title');
        __bouncy_node_set_text_content(t, 'Updated');
        const ul = __bouncy_doc_get_element_by_id('items');
        const li = __bouncy_doc_create_element('li');
        __bouncy_node_set_attribute(li, 'class', 'item');
        __bouncy_node_set_attribute(li, 'data-id', '2');
        const txt = __bouncy_doc_create_text_node('B');
        __bouncy_node_append_child(li, txt);
        __bouncy_node_append_child(ul, li);
        __bouncy_node_set_attribute(__bouncy_doc_body(), 'data-ready', '1');
        "#,
    )
    .unwrap();

    let html = rt.dump_html().unwrap();
    assert!(html.contains("Updated"), "got: {html}");
    assert!(html.contains("data-id=\"2\""), "got: {html}");
    assert!(html.contains("data-ready=\"1\""), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dump_html_without_load_errors() {
    let mut rt = make_rt();
    let err = rt.dump_html().unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("no document"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_globals_still_visible_after_load() {
    // Bootstrap.js sets globalThis.__bouncy_snapshot_built — load() must not
    // wipe the global state.
    let mut rt = make_rt();
    rt.load(PAGE, "http://test.local/").unwrap();
    assert_eq!(
        rt.eval("globalThis.__bouncy_snapshot_built").unwrap(),
        "true"
    );
}
