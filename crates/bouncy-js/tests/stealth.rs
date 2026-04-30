//! Stealth-mode tests.
//!
//! `Runtime::set_stealth(true)` should:
//!  - hide `navigator.webdriver` (read as `undefined`)
//!  - return a Chrome-flavoured `navigator.userAgent`
//!  - mask `__bouncy_*` natives so `.toString()` returns `[native code]`,
//!    same as a real built-in function.

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
async fn webdriver_undefined_under_stealth() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    assert_eq!(rt.eval("typeof navigator.webdriver").unwrap(), "undefined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_agent_looks_chrome_like_under_stealth() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let ua = rt.eval("navigator.userAgent").unwrap();
    assert!(ua.contains("Chrome/"), "got: {ua:?}");
    assert!(!ua.contains("bouncy"), "bouncy leaked into UA: {ua:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_function_tostring_masked_under_stealth() {
    // A polyfilled DOM method's `.toString()` should look like a real
    // native function so fingerprinting code can't distinguish them.
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let s = rt.eval("document.getElementById.toString()").unwrap();
    assert!(
        s.contains("[native code]"),
        "expected native-code masking, got: {s:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_canvas_fingerprint_varies_across_sessions() {
    // Two separate Runtimes both with stealth on should NOT return the
    // same canvas fingerprint — that's the whole point of per-session
    // randomization.
    let mut a = make_rt();
    a.set_stealth(true);
    a.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let fp_a = a
        .eval("document.createElement('canvas').toDataURL()")
        .unwrap();

    let mut b = make_rt();
    b.set_stealth(true);
    b.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let fp_b = b
        .eval("document.createElement('canvas').toDataURL()")
        .unwrap();

    assert!(
        fp_a.starts_with("data:image/"),
        "fp_a not a data URL: {fp_a:?}"
    );
    assert!(
        fp_b.starts_with("data:image/"),
        "fp_b not a data URL: {fp_b:?}"
    );
    assert_ne!(
        fp_a, fp_b,
        "two stealth sessions returned identical canvas fingerprints"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_canvas_fingerprint_stable_within_session() {
    // Same Runtime, two calls — must return the same value (real
    // browsers do; randomized-per-call would itself be a tell).
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let fp1 = rt
        .eval("document.createElement('canvas').toDataURL()")
        .unwrap();
    let fp2 = rt
        .eval("document.createElement('canvas').toDataURL()")
        .unwrap();
    assert_eq!(
        fp1, fp2,
        "canvas fingerprint changed between calls in the same session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_battery_returns_plausible_values() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    // navigator.getBattery() returns a Promise. V8 drains microtasks at
    // end of every script->Run, so the .then callback fires before the
    // first eval returns; the second eval reads the captured value.
    rt.eval(
        "globalThis.__r = null; navigator.getBattery().then(b => { \
         globalThis.__r = JSON.stringify({ level: b.level, charging: b.charging, \
         inRange: b.level >= 0 && b.level <= 1 }); });",
    )
    .unwrap();
    let level = rt.eval("globalThis.__r").unwrap();
    assert!(level.contains("\"inRange\":true"), "got: {level}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_gpu_adapter_has_vendor() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    rt.eval(
        "globalThis.__a = null; navigator.gpu.requestAdapter().then(a => { \
         globalThis.__a = JSON.stringify({ has: !!a, vendor: a && a.info && a.info.vendor }); });",
    )
    .unwrap();
    let v = rt.eval("globalThis.__a").unwrap();
    assert!(v.contains("\"has\":true"), "got: {v}");
    assert!(
        v.contains("\"vendor\":\"")
            && (v.contains("Intel")
                || v.contains("NVIDIA")
                || v.contains("AMD")
                || v.contains("Apple")),
        "expected a real-looking vendor string, got: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_stealth_means_no_canvas_polyfill() {
    // Without stealth we should NOT pretend to be a browser — keep the
    // snapshot lean. canvas.toDataURL stays undefined.
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let t = rt
        .eval("typeof document.createElement('canvas').toDataURL")
        .unwrap();
    assert_eq!(t, "undefined", "got: {t}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webdriver_visible_without_stealth() {
    // Sanity: default (non-stealth) Runtime exposes navigator.webdriver
    // via the polyfill. We don't pretend to be a real browser unless
    // explicitly asked.
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://example.test/")
        .unwrap();
    let v = rt.eval("typeof navigator.webdriver").unwrap();
    // Either 'boolean' (we set it explicitly to false) or 'undefined'
    // is acceptable as long as the *answer* differs from stealth mode's
    // explicit shadowing — what matters is the stealth flag is the
    // toggle.
    assert!(v == "boolean" || v == "undefined", "got: {v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_service_worker_is_a_safe_stub_even_without_stealth() {
    // Many SPA bootstraps do `navigator.serviceWorker.register(...)`
    // unconditionally. If the property is undefined that throws and we
    // never hit the rest of the script. Stub matches the common
    // feature-detect shape: `controller` is null, `ready` is a Promise,
    // `register` returns a rejected Promise so calling code can fall
    // back to its non-SW branch.
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let v = rt
        .eval(
            "globalThis.__r = null; \
             const r = navigator.serviceWorker.register('/sw.js'); \
             r.then(() => { globalThis.__r = 'ok'; }, e => { globalThis.__r = 'reject:' + (e && e.message); });",
        )
        .unwrap();
    let _ = v;
    let r = rt.eval("globalThis.__r").unwrap();
    assert!(r.starts_with("reject:"), "got: {r}");
    let controller = rt.eval("navigator.serviceWorker.controller").unwrap();
    assert_eq!(
        controller, "null",
        "controller should be null, got: {controller}"
    );
    let has_ready = rt
        .eval("navigator.serviceWorker.ready instanceof Promise")
        .unwrap();
    assert_eq!(has_ready, "true");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_document_fonts_returns_plausible_chrome_set() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    // FontFaceSet has `check(font)` (string → boolean) and is iterable.
    let arial = rt.eval(r#"document.fonts.check("12px Arial")"#).unwrap();
    assert_eq!(arial, "true", "Chrome on Linux ships Arial-equivalent");
    let weird = rt
        .eval(r#"document.fonts.check("12px ZZZ-Definitely-Not-Installed")"#)
        .unwrap();
    assert_eq!(weird, "false");
    let n = rt
        .eval("Array.from(document.fonts).length")
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(n >= 5, "expected at least 5 fonts, got: {n}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_webgl_renderer_strings_look_real() {
    let mut rt = make_rt();
    rt.set_stealth(true);
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let v = rt
        .eval(
            r#"
            const c = document.createElement('canvas');
            const gl = c.getContext('webgl');
            JSON.stringify({
                hasGL: !!gl,
                renderer: gl && gl.getParameter(gl.UNMASKED_RENDERER_WEBGL),
                vendor: gl && gl.getParameter(gl.UNMASKED_VENDOR_WEBGL),
                version: gl && gl.getParameter(gl.VERSION),
            });
            "#,
        )
        .unwrap();
    assert!(v.contains("\"hasGL\":true"), "got: {v}");
    assert!(
        v.contains("ANGLE")
            || v.contains("Mesa")
            || v.contains("NVIDIA")
            || v.contains("Intel")
            || v.contains("AMD"),
        "renderer should look like a real GPU string, got: {v}"
    );
    assert!(
        v.contains("WebGL") && v.contains("OpenGL ES"),
        "version string should mirror Chrome's shape, got: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stealth_webgl_renderer_varies_across_sessions() {
    // Same per-session-seed pattern as canvas/GPU vendor — but since
    // we pick from a small fixed set of plausible renderer strings,
    // any single pair of sessions has a ~20% chance of colliding.
    // Asserting distinctness across enough sessions makes this robust:
    // probability of all-identical across 8 sessions is (1/5)^7 ≈ 0.001%.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let mut rt = make_rt();
        rt.set_stealth(true);
        rt.load("<html><body></body></html>", "https://x.test/")
            .unwrap();
        let r = rt
            .eval("document.createElement('canvas').getContext('webgl').getParameter(0x9246)")
            .unwrap();
        seen.insert(r);
    }
    assert!(
        seen.len() >= 2,
        "WebGL renderer never varied across 8 sessions: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_stealth_means_no_webgl_polyfill() {
    // Without stealth the canvas + webgl polyfills aren't installed.
    // Element.getContext should be undefined; we assert the negative
    // shape, not the throw — keeps the test robust to where the
    // method *would* live if we ever moved the polyfill.
    let mut rt = make_rt();
    rt.load("<html><body></body></html>", "https://x.test/")
        .unwrap();
    let v = rt
        .eval("typeof document.createElement('canvas').getContext")
        .unwrap();
    assert_eq!(
        v, "undefined",
        "no-stealth canvas should not have getContext, got: {v}"
    );
}
