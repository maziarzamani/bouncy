//! Web-API polyfill tests:
//!   - localStorage / sessionStorage (in-memory, per Runtime)
//!   - URL / URLSearchParams (V8-native, sanity check the snapshot
//!     doesn't break them)
//!   - FormData
//!   - History API (pushState/replaceState/back/forward + history.length / state)
//!
//! All TDD-red against the current bootstrap.

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
async fn local_storage_round_trips() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    rt.eval("localStorage.setItem('foo', 'bar')").unwrap();
    assert_eq!(rt.eval("localStorage.getItem('foo')").unwrap(), "bar");
    assert_eq!(rt.eval("localStorage.length").unwrap(), "1");
    rt.eval("localStorage.removeItem('foo')").unwrap();
    assert_eq!(rt.eval("localStorage.getItem('foo')").unwrap(), "null");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_storage_separate_from_local() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    rt.eval("localStorage.setItem('a', '1'); sessionStorage.setItem('a', '2')")
        .unwrap();
    assert_eq!(rt.eval("localStorage.getItem('a')").unwrap(), "1");
    assert_eq!(rt.eval("sessionStorage.getItem('a')").unwrap(), "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_and_url_search_params_work() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let host = rt.eval("new URL('https://x.test:9/p?q=1#h').host").unwrap();
    assert_eq!(host, "x.test:9");
    let search = rt.eval("new URLSearchParams('?a=1&b=2').get('b')").unwrap();
    assert_eq!(search, "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_data_appends_and_reads() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let v = rt
        .eval(
            r#"
            const fd = new FormData();
            fd.append('a', '1');
            fd.append('a', '2');
            fd.set('b', 'B');
            JSON.stringify({
                a_first: fd.get('a'),
                a_all: fd.getAll('a'),
                b: fd.get('b'),
                has_b: fd.has('b'),
                has_z: fd.has('z'),
            });
            "#,
        )
        .unwrap();
    assert!(v.contains("\"a_first\":\"1\""), "got: {v}");
    assert!(v.contains("[\"1\",\"2\"]"), "got: {v}");
    assert!(v.contains("\"b\":\"B\""), "got: {v}");
    assert!(v.contains("\"has_b\":true"), "got: {v}");
    assert!(v.contains("\"has_z\":false"), "got: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_push_replace_state() {
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/p")
        .unwrap();
    rt.eval("history.pushState({a:1}, '', '/q')").unwrap();
    assert_eq!(rt.eval("history.length").unwrap(), "2");
    assert_eq!(
        rt.eval("JSON.stringify(history.state)").unwrap(),
        r#"{"a":1}"#
    );
    rt.eval("history.replaceState({b:2}, '', '/r')").unwrap();
    assert_eq!(
        rt.eval("JSON.stringify(history.state)").unwrap(),
        r#"{"b":2}"#
    );
    // length stays the same — replaceState doesn't grow the stack.
    assert_eq!(rt.eval("history.length").unwrap(), "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutation_observer_records_append_child() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><ul id='u'></ul></body></html>",
        "https://example.test/",
    )
    .unwrap();
    rt.eval(
        r#"
        let __records;
        const ul = document.getElementById('u');
        const mo = new MutationObserver((list) => { __records = list; });
        mo.observe(ul, { childList: true });
        const li = document.createElement('li');
        ul.appendChild(li);
        // Force the queued microtask. takeRecords flushes pending records.
        __records = mo.takeRecords();
        "#,
    )
    .unwrap();
    let n = rt.eval("__records.length").unwrap();
    assert!(
        n.parse::<i32>().unwrap_or(0) >= 1,
        "expected >=1 mutation record, got {n}"
    );
    assert_eq!(rt.eval("__records[0].type").unwrap(), "childList");
    assert_eq!(rt.eval("__records[0].addedNodes.length").unwrap(), "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_bubbles_to_parent_listener() {
    // child.click() must reach a listener on the parent element via the
    // bubble phase. Today addEventListener stores listeners on the JS
    // wrapper, and each _wrap() call returns a fresh wrapper, so a
    // separate getElementById on the parent never sees the child's
    // dispatch.
    let mut rt = make_rt();
    rt.load(
        "<html><body><div id='p'><button id='c'>x</button></div></body></html>",
        "https://example.test/",
    )
    .unwrap();
    let v = rt
        .eval(
            r#"
            const log = [];
            const parent = document.getElementById('p');
            parent.addEventListener('click', (e) => log.push('parent:' + e.target.id));
            const child = document.getElementById('c');
            child.addEventListener('click', (e) => log.push('child:' + e.target.id));
            child.click();
            JSON.stringify(log);
            "#,
        )
        .unwrap();
    assert_eq!(v, r#"["child:c","parent:c"]"#, "got: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_listener_fires_before_target_and_bubble() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><div id='p'><button id='c'>x</button></div></body></html>",
        "https://example.test/",
    )
    .unwrap();
    let v = rt
        .eval(
            r#"
            const log = [];
            const parent = document.getElementById('p');
            const child = document.getElementById('c');
            parent.addEventListener('click', () => log.push('parent-capture'), true);
            parent.addEventListener('click', () => log.push('parent-bubble'));
            child.addEventListener('click', () => log.push('child-target'));
            child.click();
            JSON.stringify(log);
            "#,
        )
        .unwrap();
    assert_eq!(
        v, r#"["parent-capture","child-target","parent-bubble"]"#,
        "got: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_propagation_halts_bubbling() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><div id='p'><button id='c'>x</button></div></body></html>",
        "https://example.test/",
    )
    .unwrap();
    let v = rt
        .eval(
            r#"
            const log = [];
            document.getElementById('p').addEventListener('click', () => log.push('parent'));
            const child = document.getElementById('c');
            child.addEventListener('click', (e) => { e.stopPropagation(); log.push('child'); });
            child.click();
            JSON.stringify(log);
            "#,
        )
        .unwrap();
    assert_eq!(v, r#"["child"]"#, "got: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prevent_default_makes_dispatch_return_false() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><form id='f'></form></body></html>",
        "https://example.test/",
    )
    .unwrap();
    let v = rt
        .eval(
            r#"
            const f = document.getElementById('f');
            f.addEventListener('submit', (e) => e.preventDefault());
            const ev = new Event('submit', { bubbles: true, cancelable: true });
            JSON.stringify({ ret: f.dispatchEvent(ev), prevented: ev.defaultPrevented });
            "#,
        )
        .unwrap();
    assert!(v.contains("\"ret\":false"), "got: {v}");
    assert!(v.contains("\"prevented\":true"), "got: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_db_is_a_safe_stub() {
    // Many libraries do `if (typeof indexedDB !== 'undefined') { idb.open(...) }`.
    // We don't run a real IDB store; the stub exists so feature-detect
    // and fall-back code completes without crashing — open() returns
    // an object with the expected shape (onerror/onsuccess handlers),
    // and after a microtask onerror fires so the caller takes its
    // "no IDB available" branch instead of hanging on a Promise.
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let t = rt.eval("typeof indexedDB").unwrap();
    assert_eq!(t, "object", "indexedDB should be defined, got: {t}");
    let opn = rt.eval("typeof indexedDB.open").unwrap();
    assert_eq!(opn, "function");
    rt.eval(
        "globalThis.__idb = 'pending'; \
         const req = indexedDB.open('x'); \
         req.onerror = () => { globalThis.__idb = 'error'; }; \
         req.onsuccess = () => { globalThis.__idb = 'success'; };",
    )
    .unwrap();
    let v = rt.eval("globalThis.__idb").unwrap();
    assert_eq!(
        v, "error",
        "stub should fire onerror so callers fall back, got: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_root_attaches_and_query() {
    let mut rt = make_rt();
    rt.load(
        "<html><body><div id='host'></div></body></html>",
        "https://example.test/",
    )
    .unwrap();
    let v = rt
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<span class="inner">x</span>';
            JSON.stringify({
                hostShadow: host.shadowRoot === root,
                mode: root.mode,
                inner: root.querySelector('.inner') !== null,
            });
            "#,
        )
        .unwrap();
    assert!(v.contains("\"hostShadow\":true"), "got: {v}");
    assert!(v.contains("\"mode\":\"open\""), "got: {v}");
    assert!(v.contains("\"inner\":true"), "got: {v}");
}
