// bouncy bootstrap.js
//
// Pre-evaluated at build time into a V8 startup snapshot. Anything we set
// here lives in the default context of every isolate the runtime mints —
// fresh `Context::new()` calls inherit it without re-parsing.
//
// All natives prefixed `__bouncy_*` are installed at runtime by
// `bouncy_js::bridge::install()` (FunctionTemplate, not string-dispatch).
// Method bodies referencing them resolve at call time, so it's fine that
// they don't exist at snapshot-creation time.
//
// Lazy-only: `MutationObserver`, `IntersectionObserver`, `ResizeObserver`,
// `WebGL*`, `Intl.*` are NOT in this snapshot. Adding them silently is a
// recipe violation (§3.3) — only land them when a real fixture asks for
// them.

(function (g) {
  "use strict";

  g.__bouncy_version = "0.0.0";
  g.__bouncy_snapshot_built = true;

  // Sentinel for "no node" (a `-1`-typed return from the bridge).
  const NO_NODE = -1;

  // Camel→kebab for dataset key translation: dataset.fooBar → data-foo-bar.
  const _camelToKebab = (s) =>
    s.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase());
  const _kebabToCamel = (s) =>
    s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());

  // Per-element wrapper cache — keeps `document.getElementById('x') ===
  // document.getElementById('x')` true within a session. Cleared by the
  // host runtime between page loads (it reinstalls bootstrap state).
  const _wrapCache = new Map();

  function _wrap(nid) {
    if (nid === NO_NODE || nid === null || nid === undefined) return null;
    let w = _wrapCache.get(nid);
    if (!w) {
      w = new Element(nid);
      _wrapCache.set(nid, w);
    }
    return w;
  }

  function _wrapList(nids) {
    const out = new Array(nids.length);
    for (let i = 0; i < nids.length; i++) out[i] = _wrap(nids[i]);
    return out;
  }

  // Walk an Event through the standard capture / target / bubble flow,
  // starting at `target` (a wrapper). The path is built bottom-up via
  // __bouncy_node_parent so it reflects the current tree, not what the
  // tree looked like when the listener registered.
  function _dispatchOnPath(target, ev) {
    if (!ev) return true;
    const path = [target];
    let cur = target;
    // Cap the climb at a sensible depth — don't loop on a malformed tree.
    for (let i = 0; i < 1000; i++) {
      const pid = __bouncy_node_parent(cur._nid);
      if (pid === null || pid === undefined || pid < 0) break;
      const w = _wrap(pid);
      if (!w) break;
      path.push(w);
      cur = w;
    }
    ev.target = target;
    ev._stopped = false;
    ev._stoppedImmediately = false;

    const _fireOn = (node, capturing) => {
      const arr = node._listeners && node._listeners[ev.type];
      if (!arr) return;
      ev.currentTarget = node;
      for (const l of arr.slice()) {
        if (ev._stoppedImmediately) return;
        if (capturing && !l.capture) continue;
        if (!capturing && l.capture) continue;
        try { l.fn.call(node, ev); } catch (_) {}
      }
    };

    // Capture: root → just-above-target. Stops on stopPropagation.
    ev.eventPhase = 1; // CAPTURING_PHASE
    for (let i = path.length - 1; i >= 1; i--) {
      _fireOn(path[i], true);
      if (ev._stopped) {
        ev.eventPhase = 0;
        ev.currentTarget = null;
        return !ev.defaultPrevented;
      }
    }

    // At target: both capture+bubble listeners fire (in registration order).
    ev.eventPhase = 2; // AT_TARGET
    {
      const arr = target._listeners && target._listeners[ev.type];
      if (arr) {
        ev.currentTarget = target;
        for (const l of arr.slice()) {
          if (ev._stoppedImmediately) break;
          try { l.fn.call(target, ev); } catch (_) {}
        }
      }
    }
    if (ev._stopped) {
      ev.eventPhase = 0;
      ev.currentTarget = null;
      return !ev.defaultPrevented;
    }

    // Bubble: just-above-target → root. Only if event opted in.
    if (ev.bubbles) {
      ev.eventPhase = 3; // BUBBLING_PHASE
      for (let i = 1; i < path.length; i++) {
        _fireOn(path[i], false);
        if (ev._stopped) break;
      }
    }

    ev.eventPhase = 0;
    ev.currentTarget = null;
    return !ev.defaultPrevented;
  }

  class Node {
    constructor(nid) {
      this._nid = nid;
    }
    get parentNode() {
      return _wrap(__bouncy_node_parent(this._nid));
    }
    get parentElement() {
      // Same as parentNode in our model — we only wrap elements.
      return _wrap(__bouncy_node_parent(this._nid));
    }
    get textContent() {
      return __bouncy_node_text_content(this._nid);
    }
    set textContent(v) {
      __bouncy_node_set_text_content(this._nid, String(v));
    }
    appendChild(child) {
      if (child && typeof child._nid === "number") {
        __bouncy_node_append_child(this._nid, child._nid);
        // Browser-style script-on-insert: when a <script src=...>
        // element is inserted into the document, fetch the URL, eval
        // the body in global scope, then dispatch load (or error).
        const _tag = __bouncy_node_tag_name(child._nid);
        if (_tag === "script") {
          const _src = __bouncy_node_get_attribute(child._nid, "src");
          if (_src) {
            let _err = null;
            try {
              const resp = __bouncy_sync_fetch(String(_src), "GET", "");
              if (resp.status >= 200 && resp.status < 300) {
                // Indirect eval ⇒ runs in global scope, not local.
                (0, eval)(resp.body);
              } else {
                _err = new Error("http " + resp.status);
              }
            } catch (e) {
              _err = e;
            }
            if (_err === null) {
              if (typeof child.onload === "function") child.onload();
            } else if (typeof child.onerror === "function") {
              child.onerror(_err);
            }
          }
        }
      }
      return child;
    }
    removeChild(child) {
      if (child && typeof child._nid === "number") {
        __bouncy_node_remove_child(this._nid, child._nid);
      }
      return child;
    }
  }

  class Element extends Node {
    get tagName() {
      const t = __bouncy_node_tag_name(this._nid);
      return t === null ? null : t.toUpperCase();
    }
    get nodeName() {
      return this.tagName;
    }
    getAttribute(name) {
      return __bouncy_node_get_attribute(this._nid, String(name));
    }
    setAttribute(name, value) {
      __bouncy_node_set_attribute(this._nid, String(name), String(value));
    }
    removeAttribute(name) {
      __bouncy_node_remove_attribute(this._nid, String(name));
    }
    hasAttribute(name) {
      return __bouncy_node_get_attribute(this._nid, String(name)) !== null;
    }
    get id() {
      return this.getAttribute("id") || "";
    }
    set id(v) {
      this.setAttribute("id", String(v));
    }
    get className() {
      return this.getAttribute("class") || "";
    }
    set className(v) {
      this.setAttribute("class", String(v));
    }
    // IDL-attribute reflections — browsers expose these as JS properties
    // that read/write the HTML attribute directly, not as plain own
    // properties on the element. Without them `s.src = '/x.js'` would
    // shadow the attribute and our appendChild's getAttribute would see
    // nothing.
    get src() { return this.getAttribute("src") || ""; }
    set src(v) { this.setAttribute("src", String(v)); }
    get href() { return this.getAttribute("href") || ""; }
    set href(v) { this.setAttribute("href", String(v)); }
    get value() { return this.getAttribute("value") || ""; }
    set value(v) { this.setAttribute("value", String(v)); }
    get type() { return this.getAttribute("type") || ""; }
    set type(v) { this.setAttribute("type", String(v)); }
    get name() { return this.getAttribute("name") || ""; }
    set name(v) { this.setAttribute("name", String(v)); }
    get innerHTML() {
      return __bouncy_node_inner_html(this._nid);
    }
    set innerHTML(v) {
      __bouncy_node_set_inner_html(this._nid, String(v));
    }
    get outerHTML() {
      return __bouncy_node_outer_html(this._nid);
    }
    get children() {
      const ids = __bouncy_node_children(this._nid);
      // Filter to element nodes — children DOM property excludes text /
      // comments. The bridge already returns only IDs that have tag_name,
      // but defense-in-depth.
      const out = [];
      for (const id of ids) {
        const w = _wrap(id);
        if (w && w.tagName !== null) out.push(w);
      }
      return out;
    }
    get childNodes() {
      return _wrapList(__bouncy_node_children(this._nid));
    }
    get firstChild() {
      const ids = __bouncy_node_children(this._nid);
      return ids.length > 0 ? _wrap(ids[0]) : null;
    }
    get lastChild() {
      const ids = __bouncy_node_children(this._nid);
      return ids.length > 0 ? _wrap(ids[ids.length - 1]) : null;
    }
    querySelector(selector) {
      return _wrap(__bouncy_node_query_selector(this._nid, String(selector)));
    }
    querySelectorAll(selector) {
      const ids = __bouncy_node_query_selector_all(this._nid, String(selector));
      return _wrapList(ids);
    }
    addEventListener(type, listener, options) {
      // Listeners live on the wrapper; the wrapper cache keeps the same
      // wrapper per NodeId, so a separate lookup of the same node still
      // sees registered listeners.
      const capture =
        options === true || (options && options.capture === true);
      this._listeners = this._listeners || {};
      (this._listeners[type] = this._listeners[type] || []).push({
        fn: listener,
        capture: !!capture,
      });
    }
    removeEventListener(type, listener, options) {
      if (!this._listeners || !this._listeners[type]) return;
      const capture =
        options === true || (options && options.capture === true);
      this._listeners[type] = this._listeners[type].filter(
        (l) => !(l.fn === listener && l.capture === !!capture),
      );
    }
    dispatchEvent(ev) {
      return _dispatchOnPath(this, ev);
    }
    click() {
      this.dispatchEvent(
        new g.MouseEvent("click", { bubbles: true, cancelable: true }),
      );
    }
    focus() {
      this.dispatchEvent(new g.FocusEvent("focus"));
    }
    blur() {
      this.dispatchEvent(new g.FocusEvent("blur"));
    }
    submit() {
      this.dispatchEvent(
        new g.Event("submit", { bubbles: true, cancelable: true }),
      );
    }
    attachShadow(opts) {
      const mode = (opts && opts.mode) || "open";
      const root = new ShadowRoot(this, mode);
      Object.defineProperty(this, "shadowRoot", {
        value: mode === "open" ? root : null,
        configurable: true,
      });
      return root;
    }
    // dataset proxy: foo.dataset.fooBar ↔ data-foo-bar attribute.
    get dataset() {
      const nid = this._nid;
      return new Proxy(
        {},
        {
          get(_, key) {
            return __bouncy_node_get_attribute(nid, "data-" + _camelToKebab(key));
          },
          set(_, key, value) {
            __bouncy_node_set_attribute(
              nid,
              "data-" + _camelToKebab(key),
              String(value),
            );
            return true;
          },
          has(_, key) {
            return (
              __bouncy_node_get_attribute(nid, "data-" + _camelToKebab(key)) !==
              null
            );
          },
          deleteProperty(_, key) {
            __bouncy_node_remove_attribute(
              nid,
              "data-" + _camelToKebab(key),
            );
            return true;
          },
        },
      );
    }
    // style — minimal stub that doesn't write through. Real style support
    // requires a CSSOM polyfill; we add it when a fixture needs it.
    get style() {
      return new Proxy(
        {},
        {
          get() { return ""; },
          set() { return true; },
        },
      );
    }
  }

  class Document extends Node {
    constructor() {
      super(0); // NodeId::DOCUMENT.raw() === 0
    }
    get title() {
      return __bouncy_doc_title();
    }
    get body() {
      return _wrap(__bouncy_doc_body());
    }
    get head() {
      return _wrap(__bouncy_doc_head());
    }
    get documentElement() {
      return _wrap(__bouncy_doc_html_root());
    }
    getElementById(id) {
      return _wrap(__bouncy_doc_get_element_by_id(String(id)));
    }
    createElement(tag) {
      return _wrap(__bouncy_doc_create_element(String(tag)));
    }
    createTextNode(text) {
      // Text nodes don't extend Element semantics; for the JS shim we
      // wrap them as a Node-like object so appendChild() works.
      const nid = __bouncy_doc_create_text_node(String(text));
      return new Node(nid);
    }
    // Common compatibility surfaces — left as empty stubs unless a
    // fixture needs them. Filling them later won't break the recipe
    // because none are in the snapshot's hot path right now.
    addEventListener() {}
    removeEventListener() {}
    dispatchEvent() { return true; }
  }

  // Exported globals.
  g.Node = Node;
  g.Element = Element;
  g.HTMLElement = Element;
  g.Document = Document;
  g.document = new Document();

  // ShadowRoot — minimal. Backed by a detached <div> we own so that
  // host.shadowRoot.querySelector works through the same DOM bridge.
  // Real shadow boundaries (event retargeting, slot resolution, CSS
  // encapsulation) are recipe-pending.
  class ShadowRoot {
    constructor(host, mode) {
      this.host = host;
      this.mode = String(mode);
      this._tree = g.document.createElement("div");
    }
    get innerHTML() {
      return __bouncy_node_inner_html(this._tree._nid);
    }
    set innerHTML(v) {
      __bouncy_node_set_inner_html(this._tree._nid, String(v));
    }
    querySelector(selector) {
      return _wrap(__bouncy_node_query_selector(this._tree._nid, String(selector)));
    }
    querySelectorAll(selector) {
      return _wrapList(
        __bouncy_node_query_selector_all(this._tree._nid, String(selector))
      );
    }
    appendChild(child) {
      return this._tree.appendChild(child);
    }
  }
  g.ShadowRoot = ShadowRoot;

  // Window-ish surfaces.
  g.window = g;
  g.self = g;

  // Default navigator. Stealth mode replaces it with a Chrome-flavoured
  // version that hides webdriver and lies about UA.
  g.navigator = {
    webdriver: false,
    userAgent: "bouncy/0.0.0",
    platform: "Linux x86_64",
    language: "en-US",
    languages: ["en-US"],
    appVersion: "5.0",
    vendor: "",
    // Service Worker stub — always-on (NOT stealth-gated). Many SPA
    // bootstraps unconditionally call `navigator.serviceWorker.register`
    // on load; with the property undefined that throws and the rest of
    // the page never runs. This stub makes feature-detect-and-fall-back
    // code work as the author intended: `controller` is null (no SW
    // controlling this page), `ready` is a Promise that never resolves
    // (a real "no controller" page acts the same), and `register`
    // returns a rejected Promise so callers hit their non-SW branch.
    serviceWorker: {
      controller: null,
      ready: new Promise(() => {}),
      register() {
        return Promise.reject(
          new Error("ServiceWorker not supported in bouncy"),
        );
      },
      getRegistration() { return Promise.resolve(undefined); },
      getRegistrations() { return Promise.resolve([]); },
      addEventListener() {},
      removeEventListener() {},
    },
  };

  // window.location — reads pull from the bridge's per-context base URL,
  // so they stay correct across `Runtime::load()` calls. Writes (href
  // setter, assign, replace) push onto a host-side nav queue that the
  // Runtime drains after the current eval — full mid-script suspension
  // is out of scope, last write wins.
  function _parseLocationFromHref(href) {
    // Hand-roll the parts so we don't drag a URL parser into the
    // snapshot. Fast path for the common shape `scheme://host/path?q#f`.
    let rest = String(href || "");
    let protocol = "";
    const schemeIdx = rest.indexOf("://");
    if (schemeIdx > 0) {
      protocol = rest.slice(0, schemeIdx) + ":";
      rest = rest.slice(schemeIdx + 3);
    }
    let host = "", pathname = "/", search = "", hash = "";
    let pathStart = rest.length;
    for (let i = 0; i < rest.length; i++) {
      const c = rest.charCodeAt(i);
      if (c === 0x2f /* / */ || c === 0x3f /* ? */ || c === 0x23 /* # */) {
        pathStart = i;
        break;
      }
    }
    host = rest.slice(0, pathStart);
    let pathPart = rest.slice(pathStart);
    if (pathPart.length === 0) pathPart = "/";
    const hashIdx = pathPart.indexOf("#");
    if (hashIdx >= 0) {
      hash = pathPart.slice(hashIdx);
      pathPart = pathPart.slice(0, hashIdx);
    }
    const qIdx = pathPart.indexOf("?");
    if (qIdx >= 0) {
      search = pathPart.slice(qIdx);
      pathPart = pathPart.slice(0, qIdx);
    }
    pathname = pathPart || "/";
    let hostname = host, port = "";
    const colonIdx = host.lastIndexOf(":");
    if (colonIdx >= 0 && /^\d+$/.test(host.slice(colonIdx + 1))) {
      hostname = host.slice(0, colonIdx);
      port = host.slice(colonIdx + 1);
    }
    const origin = protocol && host ? protocol + "//" + host : "";
    return { protocol, host, hostname, port, pathname, search, hash, origin };
  }
  Object.defineProperty(g, "location", {
    get() {
      const href = __bouncy_doc_url();
      const parts = _parseLocationFromHref(href);
      return {
        get href() { return href; },
        set href(v) { __bouncy_nav_to(String(v)); },
        toString() { return href; },
        protocol: parts.protocol,
        host: parts.host,
        hostname: parts.hostname,
        port: parts.port,
        pathname: parts.pathname,
        search: parts.search,
        hash: parts.hash,
        origin: parts.origin,
        // reload re-navigates to the current href.
        reload() { __bouncy_nav_to(href); },
        replace(url) { __bouncy_nav_to(String(url)); },
        assign(url) { __bouncy_nav_to(String(url)); },
      };
    },
    configurable: true,
  });
  // document.URL mirrors location.href.
  Object.defineProperty(g.document, "URL", {
    get() { return __bouncy_doc_url(); },
    configurable: true,
  });

  // Console — minimal. Real implementations would route to host stderr
  // via a native callback; for now drop output (the V8 default would
  // throw on `console.log`).
  if (!g.console) {
    g.console = {
      log: function () {},
      warn: function () {},
      error: function () {},
      info: function () {},
      debug: function () {},
    };
  }

  // Real Event with capture/bubble flow + stop / preventDefault. Used by
  // Element.dispatchEvent / .click / .submit / .focus / .blur.
  g.Event = class Event {
    constructor(type, opts) {
      this.type = type;
      this.bubbles = !!(opts && opts.bubbles);
      this.cancelable = !!(opts && opts.cancelable);
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this.eventPhase = 0;
      this._stopped = false;
      this._stoppedImmediately = false;
    }
    preventDefault() {
      if (this.cancelable) this.defaultPrevented = true;
    }
    stopPropagation() { this._stopped = true; }
    stopImmediatePropagation() {
      this._stopped = true;
      this._stoppedImmediately = true;
    }
  };
  g.Event.NONE = 0;
  g.Event.CAPTURING_PHASE = 1;
  g.Event.AT_TARGET = 2;
  g.Event.BUBBLING_PHASE = 3;
  g.CustomEvent = class CustomEvent extends g.Event {
    constructor(type, opts) {
      super(type, opts);
      this.detail = opts ? opts.detail : null;
    }
  };
  g.MouseEvent = class MouseEvent extends g.Event {
    constructor(type, opts) {
      super(type, opts);
      const o = opts || {};
      this.clientX = o.clientX || 0;
      this.clientY = o.clientY || 0;
      this.button = o.button || 0;
      this.buttons = o.buttons || 0;
      this.ctrlKey = !!o.ctrlKey;
      this.shiftKey = !!o.shiftKey;
      this.altKey = !!o.altKey;
      this.metaKey = !!o.metaKey;
    }
  };
  g.FocusEvent = class FocusEvent extends g.Event {
    constructor(type, opts) {
      super(type, opts);
      this.relatedTarget = (opts && opts.relatedTarget) || null;
    }
  };
  g.KeyboardEvent = class KeyboardEvent extends g.Event {
    constructor(type, opts) {
      super(type, opts);
      const o = opts || {};
      this.key = o.key || "";
      this.code = o.code || "";
      this.ctrlKey = !!o.ctrlKey;
      this.shiftKey = !!o.shiftKey;
      this.altKey = !!o.altKey;
      this.metaKey = !!o.metaKey;
    }
  };

  // ----- network ---------------------------------------------------------
  //
  // fetch() / XMLHttpRequest are wired to the SYNC bridge native
  // `__bouncy_sync_fetch(url, method, body)` — the host blocks on the
  // underlying tokio fetch and returns the bytes synchronously. We then
  // wrap the result in `Promise.resolve(...)` so JS code that does
  // `await fetch(url)` works unchanged: `await` on an already-resolved
  // Promise just runs the continuation in the next microtask.
  //
  // This sidesteps a full V8 Promise<->Tokio integration. If a real fixture
  // needs streaming or progress events we'll graduate to async ops + a
  // hand-pumped microtask loop; for now sync is enough for js-xhr.

  function _bridgeFetch(url, init) {
    const method = (init && init.method) || "GET";
    const body = (init && init.body && String(init.body)) || "";
    return __bouncy_sync_fetch(String(url), method, body);
  }

  g.fetch = function fetch(url, init) {
    let resp;
    try {
      resp = _bridgeFetch(url, init);
    } catch (e) {
      return Promise.reject(e);
    }
    const status = resp.status;
    const text = resp.body;
    return Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      url: String(url),
      headers: { get() { return null; } },
      text() { return Promise.resolve(text); },
      json() {
        try { return Promise.resolve(JSON.parse(text)); }
        catch (e) { return Promise.reject(e); }
      },
      clone() { return this; },
    });
  };

  // ----- localStorage / sessionStorage -----------------------------------
  function _makeStorage() {
    const map = new Map();
    return {
      get length() { return map.size; },
      key(i) { return Array.from(map.keys())[i] || null; },
      getItem(k) { return map.has(String(k)) ? map.get(String(k)) : null; },
      setItem(k, v) { map.set(String(k), String(v)); },
      removeItem(k) { map.delete(String(k)); },
      clear() { map.clear(); },
    };
  }
  g.localStorage = _makeStorage();
  g.sessionStorage = _makeStorage();

  // ----- IndexedDB (safe stub) ------------------------------------------
  //
  // We don't run a real IndexedDB store. This stub exists so libraries
  // that probe `typeof indexedDB !== 'undefined'` and immediately call
  // `indexedDB.open(...)` see an IDBOpenDBRequest-shaped object whose
  // `onerror` fires on the next microtask — that's the canonical "this
  // browser doesn't have IDB" signal callers fall back from.
  function _idbRequest() {
    const req = { onerror: null, onsuccess: null, onupgradeneeded: null,
                  result: null, error: { name: "NotSupportedError" } };
    Promise.resolve().then(() => {
      if (typeof req.onerror === "function") {
        req.onerror({ target: req, type: "error" });
      }
    });
    return req;
  }
  g.indexedDB = {
    open() { return _idbRequest(); },
    deleteDatabase() { return _idbRequest(); },
    databases() { return Promise.resolve([]); },
    cmp(a, b) { return a < b ? -1 : a > b ? 1 : 0; },
  };

  // ----- URL / URLSearchParams (minimal) ---------------------------------
  // V8 alone doesn't ship these; browsers and deno_core do. Hand-rolled
  // parser covers the shapes scrapers actually hit. Not WHATWG-perfect.
  class URLSearchParams {
    constructor(init) {
      this._p = [];
      if (init == null) return;
      if (typeof init === "string") {
        let s = init.charAt(0) === "?" ? init.slice(1) : init;
        if (!s) return;
        for (const part of s.split("&")) {
          const eq = part.indexOf("=");
          if (eq >= 0) {
            this._p.push([
              decodeURIComponent(part.slice(0, eq).replace(/\+/g, " ")),
              decodeURIComponent(part.slice(eq + 1).replace(/\+/g, " ")),
            ]);
          } else if (part.length) {
            this._p.push([decodeURIComponent(part.replace(/\+/g, " ")), ""]);
          }
        }
      } else if (Array.isArray(init)) {
        for (const e of init) this._p.push([String(e[0]), String(e[1])]);
      } else if (typeof init === "object") {
        for (const k of Object.keys(init)) this._p.push([k, String(init[k])]);
      }
    }
    get(k) { const e = this._p.find(p => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this._p.filter(p => p[0] === k).map(p => p[1]); }
    has(k) { return this._p.some(p => p[0] === k); }
    set(k, v) {
      this._p = this._p.filter(p => p[0] !== k);
      this._p.push([String(k), String(v)]);
    }
    append(k, v) { this._p.push([String(k), String(v)]); }
    delete(k) { this._p = this._p.filter(p => p[0] !== k); }
    toString() {
      return this._p
        .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
        .join("&");
    }
    forEach(cb) { for (const [k, v] of this._p) cb(v, k, this); }
    *[Symbol.iterator]() { for (const p of this._p) yield p; }
    *entries() { for (const p of this._p) yield p; }
    *keys() { for (const p of this._p) yield p[0]; }
    *values() { for (const p of this._p) yield p[1]; }
  }
  g.URLSearchParams = URLSearchParams;

  class URL {
    constructor(input, base) {
      let s = String(input);
      if (base && s.indexOf("://") < 0) {
        // Resolve relative against base.
        const baseParts = _parseLocationFromHref(String(base));
        if (s.charAt(0) === "/") {
          s = baseParts.protocol + "//" + baseParts.host + s;
        } else {
          const dir = baseParts.pathname.replace(/\/[^/]*$/, "/");
          s = baseParts.protocol + "//" + baseParts.host + dir + s;
        }
      }
      const parts = _parseLocationFromHref(s);
      this._href = s;
      this.protocol = parts.protocol;
      this.host = parts.host;
      this.hostname = parts.hostname;
      this.port = parts.port;
      this.pathname = parts.pathname;
      this.search = parts.search;
      this.hash = parts.hash;
      this.origin = parts.origin;
      this.username = "";
      this.password = "";
      this.searchParams = new URLSearchParams(parts.search);
    }
    get href() { return this._href; }
    set href(v) {
      const u = new URL(v);
      Object.assign(this, u);
    }
    toString() { return this._href; }
    toJSON() { return this._href; }
  }
  g.URL = URL;

  // ----- FormData --------------------------------------------------------
  class FormData {
    constructor() { this._p = []; }
    append(k, v) { this._p.push([String(k), String(v)]); }
    set(k, v) {
      this._p = this._p.filter(p => p[0] !== k);
      this._p.push([String(k), String(v)]);
    }
    get(k) { const e = this._p.find(p => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this._p.filter(p => p[0] === k).map(p => p[1]); }
    has(k) { return this._p.some(p => p[0] === k); }
    delete(k) { this._p = this._p.filter(p => p[0] !== k); }
    forEach(cb) { for (const [k, v] of this._p) cb(v, k, this); }
    *[Symbol.iterator]() { for (const p of this._p) yield p; }
    *entries() { for (const p of this._p) yield p; }
    *keys() { for (const p of this._p) yield p[0]; }
    *values() { for (const p of this._p) yield p[1]; }
  }
  g.FormData = FormData;

  // ----- history --------------------------------------------------------
  // Stack-based polyfill. pushState / replaceState don't actually
  // navigate (recipe-pending); they just track state + URL for scripts
  // that read history.state on continue.
  //
  // The stack is initialised lazily on first access so we don't try to
  // call the location getter (which hits a bridge native) at snapshot
  // creation time.
  let _historyStack = null;
  let _historyIndex = 0;
  function _historyTop() {
    if (_historyStack === null) {
      const url = (() => {
        try { return g.location ? g.location.href : ""; }
        catch (_) { return ""; }
      })();
      _historyStack = [{ state: null, url }];
      _historyIndex = 0;
    }
    return _historyStack;
  }
  g.history = {
    get length() { return _historyTop().length; },
    get state() { return _historyTop()[_historyIndex].state; },
    get scrollRestoration() { return "auto"; },
    set scrollRestoration(_v) {},
    pushState(state, _title, url) {
      const stack = _historyTop();
      stack.length = _historyIndex + 1;
      stack.push({ state, url: url == null ? stack[_historyIndex].url : String(url) });
      _historyIndex = stack.length - 1;
    },
    replaceState(state, _title, url) {
      const stack = _historyTop();
      stack[_historyIndex] = {
        state,
        url: url == null ? stack[_historyIndex].url : String(url),
      };
    },
    back() {
      _historyTop();
      if (_historyIndex > 0) _historyIndex--;
    },
    forward() {
      const stack = _historyTop();
      if (_historyIndex < stack.length - 1) _historyIndex++;
    },
    go(n) {
      const stack = _historyTop();
      const target = _historyIndex + (n | 0);
      if (target >= 0 && target < stack.length) _historyIndex = target;
    },
  };

  // ----- MutationObserver ------------------------------------------------
  // Tracks observers + a per-observer queue of records. Hooks into the
  // existing Node.appendChild / removeChild / Element.setAttribute to
  // notify listeners. Microtask delivery is faked via .takeRecords()
  // (sync drain); real async dispatch on the microtask queue would need
  // queueMicrotask, which most fixtures don't depend on.
  const _observers = []; // { target, options, callback, queue }

  function _notifyChildList(target, added, removed) {
    for (const o of _observers) {
      if (!o.options.childList) continue;
      if (o.target !== target && !o.options.subtree) continue;
      o.queue.push({
        type: "childList",
        target,
        addedNodes: added.slice(),
        removedNodes: removed.slice(),
        previousSibling: null,
        nextSibling: null,
      });
    }
  }
  function _notifyAttribute(target, attributeName, oldValue) {
    for (const o of _observers) {
      if (!o.options.attributes) continue;
      if (o.target !== target && !o.options.subtree) continue;
      o.queue.push({
        type: "attributes",
        target,
        attributeName,
        attributeNamespace: null,
        oldValue: o.options.attributeOldValue ? oldValue : null,
        addedNodes: [],
        removedNodes: [],
      });
    }
  }

  // Wrap Node.prototype.appendChild / removeChild to record mutations.
  const _origAppendChild = Node.prototype.appendChild;
  Node.prototype.appendChild = function (child) {
    const res = _origAppendChild.call(this, child);
    _notifyChildList(this, [child], []);
    return res;
  };
  const _origRemoveChild = Node.prototype.removeChild;
  Node.prototype.removeChild = function (child) {
    const res = _origRemoveChild.call(this, child);
    _notifyChildList(this, [], [child]);
    return res;
  };
  const _origSetAttribute = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (name, value) {
    const old = __bouncy_node_get_attribute(this._nid, String(name));
    _origSetAttribute.call(this, name, value);
    _notifyAttribute(this, String(name), old);
  };

  class MutationObserver {
    constructor(callback) {
      this._cb = callback;
      this._observed = [];
    }
    observe(target, options) {
      const entry = {
        target,
        options: options || {},
        callback: this._cb,
        queue: [],
      };
      _observers.push(entry);
      this._observed.push(entry);
    }
    disconnect() {
      for (const e of this._observed) {
        const i = _observers.indexOf(e);
        if (i >= 0) _observers.splice(i, 1);
      }
      this._observed.length = 0;
    }
    takeRecords() {
      const out = [];
      for (const e of this._observed) {
        for (const r of e.queue) out.push(r);
        e.queue.length = 0;
      }
      return out;
    }
  }
  g.MutationObserver = MutationObserver;

  g.XMLHttpRequest = class XMLHttpRequest {
    constructor() {
      this.readyState = 0;
      this.status = 0;
      this.statusText = "";
      this.responseText = "";
      this.response = null;
      this.responseType = "";
      this.onreadystatechange = null;
      this.onload = null;
      this.onerror = null;
      this.onabort = null;
      this._method = "GET";
      this._url = "";
      this._async = true;
    }
    open(method, url, async) {
      this._method = String(method || "GET");
      this._url = String(url || "");
      this._async = async !== false;
      this.readyState = 1;
      if (this.onreadystatechange) this.onreadystatechange();
    }
    setRequestHeader() { /* not yet plumbed through the bridge */ }
    send(body) {
      try {
        const resp = _bridgeFetch(this._url, {
          method: this._method,
          body: body == null ? "" : String(body),
        });
        this.status = resp.status;
        this.responseText = resp.body;
        this.response =
          this.responseType === "json" ? JSON.parse(resp.body) : resp.body;
        this.readyState = 4;
        if (this.onreadystatechange) this.onreadystatechange();
        if (this.onload) this.onload();
      } catch (e) {
        this.status = 0;
        this.readyState = 4;
        if (this.onreadystatechange) this.onreadystatechange();
        if (this.onerror) this.onerror(e);
      }
    }
    abort() {
      this.readyState = 0;
      if (this.onabort) this.onabort();
    }
  };
})(globalThis);
