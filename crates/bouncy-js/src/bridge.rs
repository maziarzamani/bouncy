//! Native `FunctionTemplate` DOM bridge.
//!
//! Each DOM method we expose to JS lands as a direct `v8::FunctionTemplate`
//! callback — no string-dispatch op, no JSON-stringified results. NodeIds
//! are passed to JS as `v8::Number`. The bridge looks up the per-context
//! `Document` via `Context::get_slot::<DomSlot>()`.
//!
//! See `RECIPE.md` §3.3 for the four perf principles this implements.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use bouncy_dom::{Document, NodeId};
use bouncy_fetch::Fetcher;

/// Per-context state slot. Holds the in-flight DOM tree.
pub type DomSlot = RefCell<Document>;

/// Per-context navigation queue. `location.href = '...'` and friends push
/// onto this slot; the host runtime drains it after the current eval ends
/// and re-enters the page lifecycle. Last write wins — multiple sets in
/// one script collapse to a single nav, matching browser behaviour.
pub type NavSlot = RefCell<Option<String>>;

/// Per-context state slot for sync HTTP — fetcher, base URL for resolving
/// relative URLs, and a Tokio handle to drive the async fetch from inside
/// a synchronous V8 callback (we call `block_in_place` + `block_on`).
pub struct FetchSlot {
    pub fetcher: Arc<Fetcher>,
    pub base_url: RefCell<String>,
    pub rt_handle: tokio::runtime::Handle,
    /// Per-origin cookies harvested from `Set-Cookie` response headers.
    /// Keyed by `scheme://host[:port]` so cookies set by host A don't
    /// leak into requests against host B. Domain / path / expiration
    /// matching is intentionally NOT implemented — within a single
    /// scrape session this approximation works.
    pub cookies: RefCell<HashMap<String, HashMap<String, String>>>,
}

fn origin_for(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    })
}

fn parse_set_cookie(raw: &str) -> Option<(String, String)> {
    let first = raw.split(';').next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some((name, value))
    }
}

fn build_cookie_header(jar: &HashMap<String, String>) -> Option<String> {
    if jar.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.sort();
    Some(parts.join("; "))
}

const NULL_NODE: f64 = -1.0;

fn dom_for(scope: &mut v8::PinScope) -> Option<Rc<DomSlot>> {
    let context = scope.get_current_context();
    context.get_slot::<DomSlot>()
}

fn fetch_for(scope: &mut v8::PinScope) -> Option<Rc<FetchSlot>> {
    let context = scope.get_current_context();
    context.get_slot::<FetchSlot>()
}

fn nav_for(scope: &mut v8::PinScope) -> Option<Rc<NavSlot>> {
    let context = scope.get_current_context();
    context.get_slot::<NavSlot>()
}

fn opt_id_to_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    id: Option<NodeId>,
) -> v8::Local<'s, v8::Value> {
    match id {
        Some(nid) => v8::Number::new(scope, nid.raw() as f64).into(),
        None => v8::Number::new(scope, NULL_NODE).into(),
    }
}

fn arg_node_id(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    idx: i32,
) -> Option<NodeId> {
    let v = args.get(idx);
    let n = v.uint32_value(scope).unwrap_or(u32::MAX);
    if n == u32::MAX {
        None
    } else {
        Some(NodeId(n))
    }
}

fn arg_string(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, idx: i32) -> String {
    args.get(idx).to_rust_string_lossy(scope)
}

// ------------- callbacks ----------------------------------------------------

fn cb_doc_root(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    rv.set(v8::Number::new(scope, NodeId::DOCUMENT.raw() as f64).into());
}

fn cb_doc_body(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = dom_for(scope).and_then(|d| d.borrow().body());
    let v = opt_id_to_value(scope, id);
    rv.set(v);
}

fn cb_doc_head(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = dom_for(scope).and_then(|d| d.borrow().head());
    let v = opt_id_to_value(scope, id);
    rv.set(v);
}

fn cb_doc_html_root(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = dom_for(scope).and_then(|d| d.borrow().html_root());
    let v = opt_id_to_value(scope, id);
    rv.set(v);
}

fn cb_doc_title(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let title = dom_for(scope)
        .and_then(|d| d.borrow().title())
        .unwrap_or_default();
    let s = v8::String::new(scope, &title).unwrap();
    rv.set(s.into());
}

fn cb_doc_url(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let url = fetch_for(scope)
        .map(|s| s.base_url.borrow().clone())
        .unwrap_or_default();
    let s = v8::String::new(scope, &url).unwrap();
    rv.set(s.into());
}

fn cb_doc_get_element_by_id(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id_str = arg_string(scope, &args, 0);
    let id = dom_for(scope).and_then(|d| d.borrow().get_element_by_id(&id_str));
    let v = opt_id_to_value(scope, id);
    rv.set(v);
}

fn cb_doc_create_element(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let tag = arg_string(scope, &args, 0);
    let id = dom_for(scope)
        .map(|d| d.borrow_mut().create_element(&tag))
        .unwrap_or(NodeId::DOCUMENT);
    rv.set(v8::Number::new(scope, id.raw() as f64).into());
}

fn cb_doc_create_text_node(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let text = arg_string(scope, &args, 0);
    let id = dom_for(scope)
        .map(|d| d.borrow_mut().create_text_node(&text))
        .unwrap_or(NodeId::DOCUMENT);
    rv.set(v8::Number::new(scope, id.raw() as f64).into());
}

fn cb_node_tag_name(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let tag = dom_for(scope).and_then(|d| d.borrow().tag_name(id));
    match tag {
        Some(t) => {
            let s = v8::String::new(scope, &t).unwrap();
            rv.set(s.into());
        }
        None => rv.set_null(),
    }
}

fn cb_node_get_attribute(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let name = arg_string(scope, &args, 1);
    let val = dom_for(scope).and_then(|d| d.borrow().get_attribute(id, &name));
    match val {
        Some(v) => {
            let s = v8::String::new(scope, &v).unwrap();
            rv.set(s.into());
        }
        None => rv.set_null(),
    }
}

fn cb_node_set_attribute(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let name = arg_string(scope, &args, 1);
    let value = arg_string(scope, &args, 2);
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().set_attribute(id, &name, &value);
    }
}

fn cb_node_remove_attribute(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let name = arg_string(scope, &args, 1);
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().remove_attribute(id, &name);
    }
}

fn cb_node_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let text = dom_for(scope)
        .map(|d| d.borrow().text_content(id))
        .unwrap_or_default();
    let s = v8::String::new(scope, &text).unwrap();
    rv.set(s.into());
}

fn cb_node_set_text_content(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let text = arg_string(scope, &args, 1);
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().set_text_content(id, &text);
    }
}

fn cb_node_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let html = dom_for(scope)
        .map(|d| d.borrow().inner_html(id))
        .unwrap_or_default();
    let s = v8::String::new(scope, &html).unwrap();
    rv.set(s.into());
}

fn cb_node_set_inner_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let html = arg_string(scope, &args, 1);
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().set_inner_html(id, &html);
    }
}

fn cb_node_outer_html(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let html = dom_for(scope)
        .map(|d| d.borrow().outer_html(id))
        .unwrap_or_default();
    let s = v8::String::new(scope, &html).unwrap();
    rv.set(s.into());
}

fn cb_node_append_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let parent = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let child = match arg_node_id(scope, &args, 1) {
        Some(i) => i,
        None => return,
    };
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().append_child(parent, child);
    }
}

fn cb_node_remove_child(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let parent = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let child = match arg_node_id(scope, &args, 1) {
        Some(i) => i,
        None => return,
    };
    if let Some(d) = dom_for(scope) {
        let _ = d.borrow_mut().remove_child(parent, child);
    }
}

fn cb_node_children(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let kids = dom_for(scope)
        .map(|d| d.borrow().children(id))
        .unwrap_or_default();
    let arr = v8::Array::new(scope, kids.len() as i32);
    for (i, k) in kids.iter().enumerate() {
        let n = v8::Number::new(scope, k.raw() as f64);
        arr.set_index(scope, i as u32, n.into());
    }
    rv.set(arr.into());
}

fn cb_node_parent(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let parent = dom_for(scope).and_then(|d| d.borrow().parent(id));
    let v = opt_id_to_value(scope, parent);
    rv.set(v);
}

fn cb_node_query_selector(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let sel = arg_string(scope, &args, 1);
    let found = dom_for(scope).and_then(|d| d.borrow().query_selector_within(id, &sel));
    let v = opt_id_to_value(scope, found);
    rv.set(v);
}

fn cb_node_query_selector_all(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let id = match arg_node_id(scope, &args, 0) {
        Some(i) => i,
        None => return,
    };
    let sel = arg_string(scope, &args, 1);
    let ids = dom_for(scope)
        .map(|d| d.borrow().query_selector_all_within(id, &sel))
        .unwrap_or_default();
    let arr = v8::Array::new(scope, ids.len() as i32);
    for (i, k) in ids.iter().enumerate() {
        let n = v8::Number::new(scope, k.raw() as f64);
        arr.set_index(scope, i as u32, n.into());
    }
    rv.set(arr.into());
}

/// Install the native bridge functions on the current context's global.
/// Each method becomes a `globalThis.__bouncy_<name>` callable; bootstrap.js
/// wraps these into ergonomic JS classes.
///
/// Has to be unrolled (rather than driven by an array of fn-pointers) so
/// each `FunctionTemplate::new` call sees the *function item* type, not a
/// generic fn-pointer — `MapFnTo` requires the `UnitType` zero-sized
/// invariant that only fn items carry.
pub fn install(scope: &mut v8::PinScope) {
    macro_rules! register {
        ($name:literal, $cb:ident) => {{
            let context = scope.get_current_context();
            let global = context.global(scope);
            let key = v8::String::new(scope, $name).unwrap();
            let tmpl = v8::FunctionTemplate::new(scope, $cb);
            let fun = tmpl.get_function(scope).unwrap();
            global.set(scope, key.into(), fun.into());
        }};
    }

    register!("__bouncy_doc_root", cb_doc_root);
    register!("__bouncy_doc_body", cb_doc_body);
    register!("__bouncy_doc_head", cb_doc_head);
    register!("__bouncy_doc_html_root", cb_doc_html_root);
    register!("__bouncy_doc_title", cb_doc_title);
    register!("__bouncy_doc_url", cb_doc_url);
    register!("__bouncy_doc_get_element_by_id", cb_doc_get_element_by_id);
    register!("__bouncy_doc_create_element", cb_doc_create_element);
    register!("__bouncy_doc_create_text_node", cb_doc_create_text_node);
    register!("__bouncy_node_tag_name", cb_node_tag_name);
    register!("__bouncy_node_get_attribute", cb_node_get_attribute);
    register!("__bouncy_node_set_attribute", cb_node_set_attribute);
    register!("__bouncy_node_remove_attribute", cb_node_remove_attribute);
    register!("__bouncy_node_text_content", cb_node_text_content);
    register!("__bouncy_node_set_text_content", cb_node_set_text_content);
    register!("__bouncy_node_inner_html", cb_node_inner_html);
    register!("__bouncy_node_set_inner_html", cb_node_set_inner_html);
    register!("__bouncy_node_outer_html", cb_node_outer_html);
    register!("__bouncy_node_append_child", cb_node_append_child);
    register!("__bouncy_node_remove_child", cb_node_remove_child);
    register!("__bouncy_node_children", cb_node_children);
    register!("__bouncy_node_parent", cb_node_parent);
    register!("__bouncy_node_query_selector", cb_node_query_selector);
    register!(
        "__bouncy_node_query_selector_all",
        cb_node_query_selector_all
    );
    register!("__bouncy_sync_fetch", cb_sync_fetch);
    register!("__bouncy_nav_to", cb_nav_to);
}

/// Queue a navigation: `location.href = url` / `location.assign(url)` /
/// `location.replace(url)` all funnel here. The host runtime drains the
/// slot after the current eval finishes.
fn cb_nav_to(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let url = arg_string(scope, &args, 0);
    if url.is_empty() {
        return;
    }
    // Resolve against the current base URL (so `location.href = '/x'`
    // navigates to a path on the current origin, not a bare `/x`).
    let resolved = if let Some(slot) = fetch_for(scope) {
        match resolve_url(&slot.base_url.borrow(), &url) {
            Ok(u) => u,
            Err(_) => url,
        }
    } else {
        url
    };
    if let Some(slot) = nav_for(scope) {
        *slot.borrow_mut() = Some(resolved);
    }
    // Halt the currently-running script. We need *both* parts:
    //
    // - throw_exception unwinds the stack immediately so trailing
    //   statements after `location.href = '/x'` don't run (V8's
    //   plain `terminate_execution` is interrupt-checked, and
    //   sequential property assignments don't hit a check point —
    //   the test caught this).
    // - terminate_execution sets the kill flag so even a user-level
    //   `try { location.href = '/x' } catch {}` can't paper over it
    //   and keep going; the next interrupt point throws a fresh
    //   uncatchable termination.
    //
    // The host's eval() recognises the marker exception and returns
    // a clean Err, then drains the queued nav.
    let msg = v8::String::new(scope, "bouncy:nav-terminated").unwrap();
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc);
    scope.terminate_execution();
}

// ---- network --------------------------------------------------------------

fn cb_sync_fetch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let url = arg_string(scope, &args, 0);
    let method = arg_string(scope, &args, 1);
    let body = arg_string(scope, &args, 2);

    let slot = match fetch_for(scope) {
        Some(s) => s,
        None => {
            throw(scope, "fetch unavailable: no FetchSlot in context");
            return;
        }
    };

    let resolved = match resolve_url(&slot.base_url.borrow(), &url) {
        Ok(u) => u,
        Err(e) => {
            throw(scope, &format!("fetch: bad url {url:?}: {e}"));
            return;
        }
    };

    // Build the request. Add a Cookie header if we've stashed any for
    // this origin from a previous Set-Cookie.
    let origin = origin_for(&resolved).unwrap_or_default();
    let cookie_header = if !origin.is_empty() {
        let jar = slot.cookies.borrow();
        jar.get(&origin).and_then(build_cookie_header)
    } else {
        None
    };

    let mut req = bouncy_fetch::FetchRequest::new(&resolved).method(if method.is_empty() {
        "GET"
    } else {
        &method
    });
    if !body.is_empty() {
        req = req.body_str(body);
    }
    if let Some(ch) = cookie_header {
        req = req.header("Cookie", ch);
    }

    let fetcher = slot.fetcher.clone();
    let rt_handle = slot.rt_handle.clone();
    let result = tokio::task::block_in_place(|| {
        rt_handle.block_on(async move { fetcher.request(req).await })
    });

    match result {
        Ok(resp) => {
            // Harvest Set-Cookie from the response into the per-origin
            // jar before we hand the body to JS.
            if !origin.is_empty() {
                let mut jar = slot.cookies.borrow_mut();
                let entry = jar.entry(origin).or_default();
                for sc in resp.headers.get_all("set-cookie") {
                    if let Ok(s) = sc.to_str() {
                        if let Some((name, value)) = parse_set_cookie(s) {
                            entry.insert(name, value);
                        }
                    }
                }
            }

            let obj = v8::Object::new(scope);
            let status_key = v8::String::new(scope, "status").unwrap();
            let status_val = v8::Number::new(scope, resp.status as f64);
            obj.set(scope, status_key.into(), status_val.into());

            let body_str = String::from_utf8_lossy(&resp.body).into_owned();
            let body_key = v8::String::new(scope, "body").unwrap();
            let body_val = v8::String::new(scope, &body_str).unwrap();
            obj.set(scope, body_key.into(), body_val.into());

            rv.set(obj.into());
        }
        Err(e) => throw(scope, &format!("fetch error: {e}")),
    }
}

fn throw(scope: &mut v8::PinScope, msg: &str) {
    if let Some(s) = v8::String::new(scope, msg) {
        let exc = v8::Exception::error(scope, s);
        scope.throw_exception(exc);
    }
}

fn resolve_url(base: &str, target: &str) -> Result<String, String> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(target.to_string());
    }
    if base.is_empty() {
        return Err("relative url with no base".into());
    }
    let parsed = url::Url::parse(base).map_err(|e| e.to_string())?;
    let joined = parsed.join(target).map_err(|e| e.to_string())?;
    Ok(joined.to_string())
}
