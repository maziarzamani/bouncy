//! End-to-end page lifecycle: load → run_inline_scripts → wait_for_selector → dump_html.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

async fn spawn_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<Incoming>| async move {
                    let body = match req.uri().path() {
                        "/api/posts.json" => Bytes::from_static(
                            br#"[{"id":1,"title":"First"},{"id":2,"title":"Second"}]"#,
                        ),
                        "/stage-1.js" => Bytes::from_static(
                            br#"window.__buildItems = function(n){var o=[];for(var i=0;i<n;i++)o.push({id:i+1,label:'item-'+(i+1)});return o;};"#,
                        ),
                        "/stage-2.js" => Bytes::from_static(
                            br#"window.__items = window.__buildItems(10);"#,
                        ),
                        _ => Bytes::from_static(b"{}"),
                    };
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(body))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

fn build_runtime() -> Runtime {
    Runtime::new(
        tokio::runtime::Handle::current(),
        Arc::new(Fetcher::new().expect("build fetcher")),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_href_set_queues_navigation() {
    // `location.href = '...'` doesn't suspend the script — we collect
    // the request and the host re-enters the page lifecycle for it.
    // Last-write-wins matches browser behaviour for multiple sets in
    // one synchronous frame.
    let mut rt = build_runtime();
    rt.load(
        "<html><body><script>location.href = '/next';</script></body></html>",
        "http://test.local/start",
    )
    .unwrap();
    rt.run_inline_scripts().unwrap();
    let pending = rt.take_pending_nav();
    assert_eq!(
        pending.as_deref(),
        Some("http://test.local/next"),
        "expected nav to be queued + resolved, got {pending:?}"
    );
    // Drain leaves nothing pending the second time.
    assert_eq!(rt.take_pending_nav(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_assign_and_replace_also_queue() {
    // First nav wins: setting `location.href` / `assign` / `replace`
    // halts the running script via V8 termination, so the second
    // call never runs. This differs from a real browser (which keeps
    // the script going until the next task boundary, "last write
    // wins") but matches what scrape authors typically intend with
    // `location.replace('/x')`: stop, go there.
    let mut rt = build_runtime();
    rt.load(
        "<html><body><script>location.assign('/a'); location.replace('/b');</script></body></html>",
        "http://test.local/",
    )
    .unwrap();
    rt.run_inline_scripts().unwrap();
    assert_eq!(
        rt.take_pending_nav().as_deref(),
        Some("http://test.local/a"),
        "first nav wins; second call should not have run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn location_href_terminates_current_script() {
    // `location.href = '/x'` in a real browser keeps the script
    // running until the task ends, but the document is about to be
    // replaced — most callers write the assignment as the last line
    // of a frame on purpose. We've gone one step further and *halt*
    // the running script as soon as the nav fires (V8
    // terminate_execution), so dead code after the assignment doesn't
    // execute even though we're not really tearing down a frame.
    // Easier to reason about; matches what authors typically intend.
    let mut rt = build_runtime();
    rt.load("<html><body></body></html>", "http://t.local/start")
        .unwrap();
    let r = rt.eval(
        "globalThis.__after = 'before'; \
         location.href = '/next'; \
         globalThis.__after = 'after';",
    );
    assert!(
        r.is_err(),
        "expected termination error from location.href, got: {r:?}"
    );
    // State written *before* the nav assignment survives.
    let after = rt.eval("globalThis.__after").unwrap();
    assert_eq!(
        after, "before",
        "code after `location.href = ...` should not have run"
    );
    // And the host can still drain the queued nav as before.
    assert_eq!(
        rt.take_pending_nav().as_deref(),
        Some("http://t.local/next")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_inline_scripts_executes_synchronous_mutations() {
    let mut rt = build_runtime();
    rt.load(
        r#"<html><body>
            <h1 id="t">old</h1>
            <script>
                document.getElementById('t').textContent = 'new';
                document.body.dataset.ready = '1';
            </script>
        </body></html>"#,
        "http://test.local/",
    )
    .unwrap();
    rt.run_inline_scripts().unwrap();
    let html = rt.dump_html().unwrap();
    assert!(html.contains("new"), "got: {html}");
    assert!(html.contains("data-ready=\"1\""), "got: {html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_selector_succeeds() {
    let mut rt = build_runtime();
    rt.load(
        r#"<html><body>
            <h1 id="t">x</h1>
            <script>document.body.dataset.ready = '1';</script>
        </body></html>"#,
        "http://test.local/",
    )
    .unwrap();
    rt.run_inline_scripts().unwrap();
    let found = rt
        .wait_for_selector("[data-ready=\"1\"]", 100)
        .await
        .unwrap();
    assert!(found);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_selector_times_out() {
    let mut rt = build_runtime();
    rt.load("<html><body><h1>x</h1></body></html>", "http://test.local/")
        .unwrap();
    let found = rt.wait_for_selector("[data-never]", 50).await.unwrap();
    assert!(!found);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_xhr_fixture_runs_to_completion() {
    let addr = spawn_server().await;
    let mut rt = build_runtime();
    let html = r#"<html><body>
            <h1 id="title">old</h1>
            <ul id="posts"></ul>
            <div id="status">loading</div>
            <script>
            (async () => {
                const r = await fetch('/api/posts.json');
                const posts = await r.json();
                document.getElementById('title').textContent = 'updated';
                const ul = document.getElementById('posts');
                for (const p of posts) {
                    const li = document.createElement('li');
                    li.className = 'post';
                    li.dataset.id = p.id;
                    li.innerHTML = '<h2>' + p.title + '</h2>';
                    ul.appendChild(li);
                }
                document.getElementById('status').textContent = 'ready';
                document.body.dataset.ready = '1';
            })();
            </script>
        </body></html>"#;
    rt.load(html, &format!("http://{addr}/")).unwrap();
    rt.run_inline_scripts().unwrap();
    let found = rt
        .wait_for_selector("[data-ready=\"1\"]", 2000)
        .await
        .unwrap();
    assert!(found, "wait_for_selector timed out");
    let dump = rt.dump_html().unwrap();
    assert!(dump.contains("updated"), "title not mutated: {dump}");
    assert!(dump.contains("data-id=\"1\""), "post 1 missing: {dump}");
    assert!(dump.contains("data-id=\"2\""), "post 2 missing: {dump}");
    assert!(dump.contains("First"), "post title missing: {dump}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_script_src_injection_runs_and_fires_onload() {
    // Two stages chained via injectStage(...) that creates a <script src=...> element
    // and resolves a Promise on its onload.
    let addr = spawn_server().await;
    let mut rt = build_runtime();
    let html = r#"<html><body>
        <ul id="items"></ul>
        <script>
        function injectStage(src) {
            return new Promise((resolve, reject) => {
                const s = document.createElement('script');
                s.src = src;
                s.onload = resolve;
                s.onerror = () => reject('load: ' + src);
                document.head.appendChild(s);
            });
        }
        (async () => {
            await injectStage('/stage-1.js');
            await injectStage('/stage-2.js');
            const ul = document.getElementById('items');
            for (const it of (window.__items || [])) {
                const li = document.createElement('li');
                li.className = 'item';
                li.dataset.id = it.id;
                li.textContent = it.label;
                ul.appendChild(li);
            }
            document.body.dataset.ready = '1';
        })();
        </script>
    </body></html>"#;
    rt.load(html, &format!("http://{addr}/")).unwrap();
    rt.run_inline_scripts().unwrap();
    let found = rt
        .wait_for_selector("[data-ready=\"1\"]", 2000)
        .await
        .unwrap();
    assert!(found, "dynamic script lifecycle did not signal ready");
    let dump = rt.dump_html().unwrap();
    assert!(dump.contains("data-id=\"1\""), "item 1 missing: {dump}");
    assert!(dump.contains("data-id=\"10\""), "item 10 missing: {dump}");
    assert!(dump.contains("item-1"), "label item-1 missing: {dump}");
}
