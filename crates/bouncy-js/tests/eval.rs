//! Phase 4 tests for bouncy-js: V8 isolate pool basics.

use bouncy_js::IsolatePool;

#[test]
fn eval_arithmetic_returns_string_repr() {
    let pool = IsolatePool::new(1);
    let mut guard = pool.checkout().expect("checkout");
    let v = guard.eval("1 + 2").unwrap();
    assert_eq!(v, "3");
}

#[test]
fn eval_string_literal() {
    let pool = IsolatePool::new(1);
    let mut guard = pool.checkout().expect("checkout");
    let v = guard.eval("'hello world'").unwrap();
    assert_eq!(v, "hello world");
}

#[test]
fn eval_compile_error() {
    let pool = IsolatePool::new(1);
    let mut guard = pool.checkout().expect("checkout");
    let err = guard.eval("function (").unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("syntax"),
        "got: {err}"
    );
}

#[test]
fn eval_runtime_error() {
    let pool = IsolatePool::new(1);
    let mut guard = pool.checkout().expect("checkout");
    let err = guard.eval("throw new Error('boom')").unwrap_err();
    assert!(format!("{err}").contains("boom"), "got: {err}");
}

#[test]
fn fresh_context_per_eval() {
    // Each eval gets a fresh Context — globals from one eval do NOT leak
    // into the next (we want page isolation).
    let pool = IsolatePool::new(1);
    let mut guard = pool.checkout().expect("checkout");
    guard.eval("globalThis.x = 42").unwrap();
    let v = guard.eval("typeof x").unwrap();
    assert_eq!(v, "undefined", "global state leaked across evals");
}

#[test]
fn snapshot_globals_visible_in_fresh_context() {
    // The bootstrap snapshot sets globalThis.__bouncy_snapshot_built = true.
    // Fresh contexts created via Context::new() must inherit it.
    let pool = IsolatePool::new(1);
    let mut g = pool.checkout().expect("checkout");
    assert_eq!(
        g.eval("globalThis.__bouncy_snapshot_built").unwrap(),
        "true"
    );
    assert_eq!(g.eval("globalThis.__bouncy_version").unwrap(), "0.0.0");
}

#[test]
fn pool_size_one_serial_reuse() {
    // A 1-isolate pool must let two sequential checkouts succeed.
    let pool = IsolatePool::new(1);
    {
        let mut g = pool.checkout().expect("first checkout");
        assert_eq!(g.eval("1+1").unwrap(), "2");
    }
    {
        let mut g = pool.checkout().expect("second checkout");
        assert_eq!(g.eval("2+2").unwrap(), "4");
    }
}
