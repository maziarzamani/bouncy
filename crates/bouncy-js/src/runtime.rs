//! Browser-like runtime: fetch HTML, parse to DOM, run scripts in V8 against
//! the DOM, dump the post-script HTML.
//!
//! Holds one `OwnedIsolate` (booted from the bootstrap snapshot) plus a
//! persistent `Global<Context>`. Native bridge functions (`bridge::install`)
//! are wired into the context once, at construction time. Each `load(html)`
//! parses the document and stashes it in the context's slot, replacing the
//! previous page.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bouncy_dom::Document;
use bouncy_fetch::Fetcher;

use crate::bridge::{self, DomSlot, FetchSlot, NavSlot};
use crate::{init_v8, Error, SNAPSHOT};

pub struct Runtime {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    fetcher: Arc<Fetcher>,
    rt_handle: tokio::runtime::Handle,
    stealth: bool,
    /// Per-Runtime seed for stealth-mode fingerprint randomization.
    /// Generated once at construction; survives across `load()` calls
    /// so canvas / audio / GPU fingerprints are stable within a session
    /// but vary across sessions.
    stealth_seed: u32,
}

/// Generate a fresh u32 seed for a Runtime's stealth state. Mixes a
/// process-local atomic counter with the current wallclock so adjacent
/// Runtimes don't collide; not cryptographic, just enough entropy that
/// two sessions never share a fingerprint by accident.
fn next_stealth_seed() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ counter;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x as u32) ^ ((x >> 32) as u32)
}

/// Stealth patch: hides `navigator.webdriver`, swaps the UA for a
/// recent Chrome string, masks our polyfill methods + native bridge
/// callbacks so `.toString()` returns the canonical native shape, and
/// adds per-session randomization to canvas / WebGPU / battery so a
/// detector can't cluster sessions on those fingerprints. The seed is
/// stable within a Runtime (across multiple `load()`s) but varies
/// across Runtimes.
const STEALTH_PATCH: &str = r##"
(function () {
  const realFns = new WeakSet();
  function markPrototype(proto) {
    if (!proto) return;
    Object.getOwnPropertyNames(proto).forEach(name => {
      const desc = Object.getOwnPropertyDescriptor(proto, name);
      if (!desc) return;
      if (typeof desc.value === 'function') realFns.add(desc.value);
      if (typeof desc.get === 'function') realFns.add(desc.get);
      if (typeof desc.set === 'function') realFns.add(desc.set);
    });
  }
  // DOM polyfill methods
  markPrototype(globalThis.Node && globalThis.Node.prototype);
  markPrototype(globalThis.Element && globalThis.Element.prototype);
  markPrototype(globalThis.Document && globalThis.Document.prototype);
  // Native bridge globals
  Object.keys(globalThis)
    .filter(k => k.startsWith('__bouncy_'))
    .forEach(k => {
      if (typeof globalThis[k] === 'function') realFns.add(globalThis[k]);
    });

  const origToString = Function.prototype.toString;
  Function.prototype.toString = function () {
    if (realFns.has(this)) {
      return 'function ' + (this.name || '') + '() { [native code] }';
    }
    return origToString.call(this);
  };

  globalThis.navigator = {
    webdriver: undefined,
    userAgent:
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 " +
      "(KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
    platform: "Linux x86_64",
    language: "en-US",
    languages: ["en-US", "en"],
    appVersion:
      "5.0 (X11; Linux x86_64) AppleWebKit/537.36 " +
      "(KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
    vendor: "Google Inc.",
    hardwareConcurrency: 8,
    deviceMemory: 8,
    plugins: [],
    mimeTypes: [],
    serviceWorker: {
      controller: null,
      ready: new Promise(() => {}),
      register() { return Promise.reject(new Error("ServiceWorker not supported in bouncy")); },
      getRegistration() { return Promise.resolve(undefined); },
      getRegistrations() { return Promise.resolve([]); },
      addEventListener() {}, removeEventListener() {},
    },
  };

  // Per-session seed. Generated once per Runtime — even across multiple
  // `load()`s, so a detector that probes the same page twice gets the
  // same fingerprint (as a real browser would). Stable within a session,
  // varies across sessions.
  if (typeof globalThis.__bouncy_stealth_seed === 'undefined') {
    globalThis.__bouncy_stealth_seed =
      ((Math.random() * 4294967295) | 0) >>> 0;
  }
  const _seed = globalThis.__bouncy_stealth_seed;

  // Tiny LCG that's deterministic for a given seed.
  function _rng(s) {
    let x = s >>> 0;
    return function () {
      x = (Math.imul(x, 1664525) + 1013904223) >>> 0;
      return x;
    };
  }

  // Canvas fingerprint stub. Real Chrome rasterises text + patterns so
  // every request returns identical pixel data. Fingerprinters call
  // toDataURL on a canvas they drew specific shapes on, then hash the
  // result. We don't draw — we return a per-session stable string so
  // the hash is consistent across calls in this session but different
  // across sessions.
  const _b64 =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const _canvasFp = (function () {
    const r = _rng(_seed);
    let s = "data:image/png;base64,";
    for (let i = 0; i < 64; i++) s += _b64[r() & 63];
    return s;
  })();
  if (globalThis.Element && globalThis.Element.prototype) {
    Object.defineProperty(globalThis.Element.prototype, 'toDataURL', {
      value: function () { return _canvasFp; },
      configurable: true, writable: true,
    });
    Object.defineProperty(globalThis.Element.prototype, 'getContext', {
      value: function () {
        return {
          fillText() {}, fillRect() {}, beginPath() {}, moveTo() {},
          lineTo() {}, stroke() {}, fill() {}, arc() {},
          fillStyle: "#000", strokeStyle: "#000",
          getImageData() {
            return { data: new Uint8ClampedArray(0), width: 0, height: 0 };
          },
        };
      },
      configurable: true, writable: true,
    });
    realFns.add(globalThis.Element.prototype.toDataURL);
    realFns.add(globalThis.Element.prototype.getContext);
  }

  // navigator.getBattery — randomized per session so a fingerprinter
  // can't cluster on a static value.
  const _batteryLevel =
    Math.round(((((_seed >>> 8) & 0xff) / 255) * 0.5 + 0.5) * 100) / 100;
  globalThis.navigator.getBattery = function () {
    return Promise.resolve({
      charging: true,
      level: _batteryLevel,
      chargingTime: Infinity,
      dischargingTime: Infinity,
      addEventListener() {}, removeEventListener() {},
    });
  };
  realFns.add(globalThis.navigator.getBattery);

  // navigator.gpu.requestAdapter — vendor varies per session. WebGPU is
  // a fingerprint surface; we return one of a few plausible vendors so
  // the absence of WebGPU doesn't itself flag us.
  const _gpuVendors = ["Intel Inc.", "NVIDIA Corporation", "AMD", "Apple Inc."];
  const _gpuVendor = _gpuVendors[((_seed >>> 16) & 0xff) % _gpuVendors.length];
  globalThis.navigator.gpu = {
    requestAdapter() {
      return Promise.resolve({
        info: {
          vendor: _gpuVendor,
          architecture: "bouncy-gpu-r1",
          description: _gpuVendor + " (bouncy stealth)",
        },
        features: new Set(),
        limits: {},
        requestDevice() { return Promise.resolve({}); },
      });
    },
  };
  realFns.add(globalThis.navigator.gpu.requestAdapter);

  // AudioContext stub — varies per session. Many fingerprinters hash
  // the output of an OfflineAudioContext rendering a fixed oscillator
  // chain. We return a per-session stable stub.
  const _audioFp = (function () {
    const r = _rng(_seed ^ 0x55aa55aa);
    let s = "";
    for (let i = 0; i < 16; i++) s += (r() % 1000).toString(16);
    return s;
  })();
  globalThis.AudioContext = function () {
    return {
      sampleRate: 44100,
      state: "running",
      destination: {},
      createOscillator() {
        return {
          frequency: { value: 440 },
          connect() {}, disconnect() {}, start() {}, stop() {},
        };
      },
      createAnalyser() {
        const a = {
          fftSize: 2048,
          frequencyBinCount: 1024,
          getFloatFrequencyData(out) {
            const r = _rng(_seed ^ 0x12345678);
            for (let i = 0; i < out.length; i++) {
              out[i] = -100 + (r() & 0xff) / 255;
            }
          },
          getFloatTimeDomainData(out) {
            const r = _rng(_seed ^ 0x87654321);
            for (let i = 0; i < out.length; i++) {
              out[i] = (r() & 0xff) / 255 - 0.5;
            }
          },
          connect() {}, disconnect() {},
        };
        return a;
      },
      createGain() { return { gain: { value: 1 }, connect() {}, disconnect() {} }; },
      createBuffer() { return {}; },
      close() { return Promise.resolve(); },
      __bouncy_fp: _audioFp,
    };
  };
  globalThis.OfflineAudioContext = globalThis.AudioContext;
  realFns.add(globalThis.AudioContext);

  // document.fonts — FontFaceSet with check() + iterator. Returns a
  // Linux-Chrome-typical font list so fingerprinters that probe via
  // `document.fonts.check(font)` or enumerate `document.fonts` see a
  // real-looking signal. Two flavours of font-detect exist: name-based
  // (check) and width-measurement (we can't fake — no layout). The
  // name-based one is the cheaper detector path; that's what we cover.
  const _chromeLinuxFonts = new Set([
    "Arial", "Helvetica", "Times New Roman", "Times", "Courier New",
    "Courier", "Verdana", "Georgia", "Comic Sans MS", "Trebuchet MS",
    "Tahoma", "Impact", "Liberation Sans", "Liberation Serif",
    "Liberation Mono", "DejaVu Sans", "DejaVu Serif", "DejaVu Sans Mono",
    "Noto Sans", "Noto Serif", "Ubuntu", "monospace", "sans-serif", "serif",
  ]);
  const _fontEntries = Array.from(_chromeLinuxFonts).map((family) => ({
    family,
    style: "normal",
    weight: "400",
    stretch: "normal",
    unicodeRange: "U+0-10FFFF",
    variant: "normal",
    featureSettings: "normal",
    status: "loaded",
    load() { return Promise.resolve(this); },
  }));
  const _fontFaceSet = {
    size: _fontEntries.length,
    ready: Promise.resolve(undefined),
    status: "loaded",
    check(spec) {
      // `spec` looks like "12px Arial" — extract the font-family token
      // (everything after the last size-ish prefix). We're forgiving:
      // single-name match against the Chrome Linux set.
      const s = String(spec || "");
      const m = s.match(/(?:[\d.]+px\s+)?["']?([^"',]+)["']?/);
      if (!m) return false;
      const name = m[1].trim();
      return _chromeLinuxFonts.has(name);
    },
    forEach(cb) { _fontEntries.forEach(cb); },
    [Symbol.iterator]() { return _fontEntries[Symbol.iterator](); },
    add() {}, delete() {}, clear() {},
    addEventListener() {}, removeEventListener() {},
  };
  Object.defineProperty(globalThis.document, "fonts", {
    value: _fontFaceSet,
    configurable: true,
  });
  realFns.add(_fontFaceSet.check);

  // WebGL — full ANGLE/Mesa-style renderer + vendor strings,
  // randomized per session via _seed. Real fingerprinters call
  // gl.getParameter(UNMASKED_RENDERER_WEBGL) and hash the result; a
  // missing WebGL stack is itself a tell, so we fake the common
  // constants. We don't actually rasterise anything (no GPU); only
  // getParameter() returns useful data.
  const _glRenderers = [
    { vendor: "Google Inc. (NVIDIA)",
      renderer: "ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 Direct3D11 vs_5_0 ps_5_0, D3D11)" },
    { vendor: "Google Inc. (Intel)",
      renderer: "ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0, D3D11)" },
    { vendor: "Google Inc. (AMD)",
      renderer: "ANGLE (AMD, AMD Radeon RX 580 Direct3D11 vs_5_0 ps_5_0, D3D11)" },
    { vendor: "Mesa", renderer: "Mesa Intel(R) UHD Graphics 620 (KBL GT2)" },
    { vendor: "Mesa", renderer: "Mesa AMD Radeon Graphics (RADV NAVI23)" },
  ];
  const _glPick = _glRenderers[((_seed >>> 24) & 0xff) % _glRenderers.length];
  function _makeGLContext() {
    const PARAMS = {
      0x1F00: "WebKit",                                         // VENDOR
      0x1F01: _glPick.renderer,                                 // RENDERER
      0x1F02: "WebGL 1.0 (OpenGL ES 2.0 Chromium)",             // VERSION
      0x8B8C: "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)", // SHADING_LANGUAGE_VERSION
      0x9245: _glPick.vendor,                                   // UNMASKED_VENDOR_WEBGL
      0x9246: _glPick.renderer,                                 // UNMASKED_RENDERER_WEBGL
      0x0D33: 16384,                                            // MAX_TEXTURE_SIZE
      0x8869: 16,                                               // MAX_VERTEX_ATTRIBS
      0x8DFB: 16,                                               // MAX_FRAGMENT_UNIFORM_VECTORS
      0x8DFD: 30,                                               // MAX_VARYING_VECTORS
      0x8DFC: 4096,                                             // MAX_VERTEX_UNIFORM_VECTORS
    };
    return {
      // Constants surface so getParameter(gl.UNMASKED_RENDERER_WEBGL) works.
      VENDOR: 0x1F00, RENDERER: 0x1F01, VERSION: 0x1F02,
      SHADING_LANGUAGE_VERSION: 0x8B8C,
      UNMASKED_VENDOR_WEBGL: 0x9245, UNMASKED_RENDERER_WEBGL: 0x9246,
      MAX_TEXTURE_SIZE: 0x0D33, MAX_VERTEX_ATTRIBS: 0x8869,
      MAX_FRAGMENT_UNIFORM_VECTORS: 0x8DFB, MAX_VARYING_VECTORS: 0x8DFD,
      MAX_VERTEX_UNIFORM_VECTORS: 0x8DFC,
      // Behaviour.
      getParameter(p) { return Object.prototype.hasOwnProperty.call(PARAMS, p) ? PARAMS[p] : null; },
      getExtension(name) {
        if (name === "WEBGL_debug_renderer_info") {
          return { UNMASKED_VENDOR_WEBGL: 0x9245, UNMASKED_RENDERER_WEBGL: 0x9246 };
        }
        return null;
      },
      getSupportedExtensions() {
        return [
          "ANGLE_instanced_arrays", "EXT_blend_minmax", "EXT_color_buffer_half_float",
          "EXT_disjoint_timer_query", "EXT_float_blend", "EXT_frag_depth",
          "EXT_shader_texture_lod", "EXT_texture_compression_bptc",
          "EXT_texture_compression_rgtc", "EXT_texture_filter_anisotropic",
          "WEBKIT_EXT_texture_filter_anisotropic", "EXT_sRGB", "OES_element_index_uint",
          "OES_fbo_render_mipmap", "OES_standard_derivatives", "OES_texture_float",
          "OES_texture_float_linear", "OES_texture_half_float",
          "OES_texture_half_float_linear", "OES_vertex_array_object",
          "WEBGL_color_buffer_float", "WEBGL_compressed_texture_s3tc",
          "WEBGL_compressed_texture_s3tc_srgb", "WEBGL_debug_renderer_info",
          "WEBGL_debug_shaders", "WEBGL_depth_texture", "WEBGL_draw_buffers",
          "WEBGL_lose_context", "WEBGL_multi_draw",
        ];
      },
      // Common no-op methods so feature-detect-and-render code doesn't crash.
      createShader() { return {}; }, createProgram() { return {}; },
      createBuffer() { return {}; }, createTexture() { return {}; },
      bindBuffer() {}, bindTexture() {}, viewport() {}, clear() {}, clearColor() {},
      enable() {}, disable() {}, useProgram() {}, drawArrays() {}, drawElements() {},
      attachShader() {}, linkProgram() {}, shaderSource() {}, compileShader() {},
      getProgramParameter() { return true; }, getShaderParameter() { return true; },
      getUniformLocation() { return {}; }, getAttribLocation() { return 0; },
      uniform1f() {}, uniform2f() {}, uniform3f() {}, uniform4f() {},
      uniformMatrix4fv() {}, vertexAttribPointer() {}, enableVertexAttribArray() {},
      bufferData() {}, texImage2D() {}, texParameteri() {},
      canvas: null,
      drawingBufferWidth: 300, drawingBufferHeight: 150,
    };
  }
  // Wrap the existing canvas-stub `getContext` so 'webgl' / 'webgl2' /
  // 'experimental-webgl' return our fake GL; '2d' keeps the existing
  // behaviour from the canvas fingerprint stub above.
  if (globalThis.Element && globalThis.Element.prototype) {
    const _origGetContext = globalThis.Element.prototype.getContext;
    Object.defineProperty(globalThis.Element.prototype, 'getContext', {
      value: function (type) {
        if (type === "webgl" || type === "webgl2" || type === "experimental-webgl") {
          const ctx = _makeGLContext();
          ctx.canvas = this;
          return ctx;
        }
        return _origGetContext.call(this, type);
      },
      configurable: true, writable: true,
    });
    realFns.add(globalThis.Element.prototype.getContext);
  }
})();
"##;

impl Runtime {
    /// Clone the underlying Fetcher Arc — useful for callers that want to
    /// drive a request outside the V8 lifecycle (e.g. bouncy-cdp doing a
    /// `Page.navigate` fetch before re-loading the document).
    pub fn fetcher_clone(&self) -> Arc<Fetcher> {
        self.fetcher.clone()
    }

    /// Build a runtime tied to a Tokio runtime. The handle is used to drive
    /// async ops (currently `__bouncy_sync_fetch`) from inside synchronous V8
    /// callbacks via `block_in_place + block_on`. Caller's runtime SHOULD be
    /// `flavor = "multi_thread"`; otherwise `block_on` panics.
    pub fn new(rt_handle: tokio::runtime::Handle, fetcher: Arc<Fetcher>) -> Self {
        init_v8();
        let mut isolate = v8::Isolate::new(
            v8::CreateParams::default().snapshot_blob(v8::StartupData::from(SNAPSHOT)),
        );

        let context_global = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            {
                let mut ctx_scope = v8::ContextScope::new(scope, context);
                bridge::install(&mut ctx_scope);
            }
            v8::Global::new(scope, context)
        };

        Self {
            isolate,
            context: context_global,
            fetcher,
            rt_handle,
            stealth: false,
            stealth_seed: next_stealth_seed(),
        }
    }

    /// Enable stealth mode for subsequent `load()`s. Hides
    /// `navigator.webdriver`, swaps the UA for a Chrome string, and
    /// masks polyfill / bridge methods so `.toString()` returns the
    /// canonical `[native code]` shape.
    pub fn set_stealth(&mut self, on: bool) {
        self.stealth = on;
    }

    /// Replace the current page with `html`. `base_url` is used to resolve
    /// relative URLs in `fetch()` / `XMLHttpRequest` calls from the page.
    pub fn load(&mut self, html: &str, base_url: &str) -> Result<(), Error> {
        let doc = Document::parse(html).map_err(|e| Error::Compile(e.to_string()))?;
        let dom_slot: Rc<DomSlot> = Rc::new(RefCell::new(doc));
        let fetch_slot = Rc::new(FetchSlot {
            fetcher: self.fetcher.clone(),
            base_url: RefCell::new(base_url.to_string()),
            rt_handle: self.rt_handle.clone(),
            cookies: RefCell::new(std::collections::HashMap::new()),
        });

        {
            v8::scope!(let scope, &mut self.isolate);
            let context = v8::Local::new(scope, &self.context);
            // Context::set_slot is per-Context and persists across scope
            // exits — Scope::set_slot would be transient and only visible to
            // the same scope, which is *not* what we want here.
            context.set_slot::<DomSlot>(dom_slot);
            context.set_slot::<FetchSlot>(fetch_slot);
            // Reset the nav queue on every load — pending nav from the
            // previous page must not survive into the new one.
            let nav_slot: Rc<NavSlot> = Rc::new(RefCell::new(None));
            context.set_slot::<NavSlot>(nav_slot);
        }

        // Run the stealth patch AFTER the slots are wired and BEFORE
        // any user script touches navigator etc. Inject the seed first
        // so each load() in the same Runtime ends up with the same
        // fingerprint (real browsers don't randomize on navigation).
        if self.stealth {
            self.eval(&format!(
                "globalThis.__bouncy_stealth_seed = {};",
                self.stealth_seed
            ))?;
            self.eval(STEALTH_PATCH)?;
        }
        Ok(())
    }

    /// Compile + run `src` in the loaded page's context. Returns the result
    /// coerced to a string.
    pub fn eval(&mut self, src: &str) -> Result<String, Error> {
        v8::scope!(let scope, &mut self.isolate);
        // Defensive: a previous eval may have left the termination
        // flag set (e.g. another nav termination). Clear it before
        // we run anything else; otherwise the next script returns
        // None immediately with no exception and we can't tell why.
        scope.cancel_terminate_execution();
        let context = v8::Local::new(scope, &self.context);
        let mut ctx_scope = v8::ContextScope::new(scope, context);
        v8::tc_scope!(let tc, &mut ctx_scope);

        let code = v8::String::new(tc, src).ok_or(Error::Convert)?;
        let script = match v8::Script::compile(tc, code, None) {
            Some(s) => s,
            None => {
                let msg = match tc.message() {
                    Some(m) => m.get(tc).to_rust_string_lossy(tc),
                    None => "<compile error>".into(),
                };
                return Err(Error::Compile(msg));
            }
        };
        let result = match script.run(tc) {
            Some(v) => v,
            None => {
                // __bouncy_nav_to (the location.href setter) throws a
                // marker exception + sets the V8 kill flag to halt the
                // running script. Either signal lands here. Clean it
                // up so the next eval works, and return a recognisable
                // error so the host knows it was a queued nav, not a
                // user-code crash.
                let msg_text = tc
                    .message()
                    .map(|m| m.get(tc).to_rust_string_lossy(tc))
                    .unwrap_or_default();
                let nav_terminated =
                    tc.has_terminated() || msg_text.contains("bouncy:nav-terminated");
                if nav_terminated {
                    if tc.has_terminated() {
                        tc.cancel_terminate_execution();
                    }
                    return Err(Error::NavTerminated);
                }
                return Err(Error::Runtime(if msg_text.is_empty() {
                    "<runtime error>".into()
                } else {
                    msg_text
                }));
            }
        };
        let s = result
            .to_string(tc)
            .ok_or(Error::Convert)?
            .to_rust_string_lossy(tc);
        Ok(s)
    }

    /// Re-serialize the loaded DOM (post-mutation) back to HTML.
    pub fn dump_html(&mut self) -> Result<String, Error> {
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        match context.get_slot::<DomSlot>() {
            Some(slot) => Ok(slot.borrow().serialize()),
            None => Err(Error::Runtime(
                "no document loaded — call Runtime::load() first".into(),
            )),
        }
    }

    /// Collect all inline `<script>` elements (no `src` attribute) from the
    /// loaded document and eval each in document order. `<script src=>` is
    /// not handled in this version — the recipe (§3.3) has us land it as
    /// follow-up when a fixture asks for dynamic script injection.
    pub fn run_inline_scripts(&mut self) -> Result<(), Error> {
        let scripts = {
            v8::scope!(let scope, &mut self.isolate);
            let context = v8::Local::new(scope, &self.context);
            let slot = context
                .get_slot::<DomSlot>()
                .ok_or_else(|| Error::Runtime("no document loaded".into()))?;
            let doc = slot.borrow();
            doc.query_selector_all("script")
                .into_iter()
                .filter_map(|nid| {
                    if doc.get_attribute(nid, "src").is_some() {
                        return None;
                    }
                    // Scripts are deliberately skipped by text_content
                    // (visible-text semantics). Use raw_text_content here.
                    let src = doc.raw_text_content(nid);
                    if src.trim().is_empty() {
                        None
                    } else {
                        Some(src)
                    }
                })
                .collect::<Vec<_>>()
        };

        for src in scripts {
            // Inline scripts setting `location.href = '...'` raise a
            // clean Error::NavTerminated — that's an expected control-
            // flow signal, not a script failure. Swallow it; the
            // queued nav is captured in NavSlot for the host to drain.
            match self.eval(&src) {
                Ok(_) | Err(Error::NavTerminated) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Drain any pending navigation queued by `location.href = '...'` /
    /// `location.assign(...)` / `location.replace(...)` since the last
    /// call. Returns the resolved URL, or `None` if nothing is queued.
    pub fn take_pending_nav(&mut self) -> Option<String> {
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        let slot = context.get_slot::<NavSlot>()?;
        let mut g = slot.borrow_mut();
        g.take()
    }

    /// Find a single element matching `selector` against the loaded DOM.
    /// Returns the underlying `NodeId.raw()` so callers can refer to it
    /// across follow-up calls. Bypasses V8.
    pub fn query_selector(&mut self, selector: &str) -> Option<u32> {
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        let slot = context.get_slot::<DomSlot>()?;
        let nid = slot.borrow().query_selector(selector)?;
        Some(nid.raw())
    }

    /// Serialize the node identified by `raw_nid` (a value returned by
    /// `query_selector`) back to HTML, including its own start/end tag.
    /// Returns `None` if the id is unknown.
    pub fn outer_html(&mut self, raw_nid: u32) -> Option<String> {
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        let slot = context.get_slot::<DomSlot>()?;
        let html = slot.borrow().outer_html(bouncy_dom::NodeId(raw_nid));
        Some(html)
    }

    /// Block until `selector` matches an element, polling every 5 ms. If
    /// `timeout_ms` elapses first, returns `Ok(false)`. Returns `Ok(true)`
    /// the moment a match appears.
    pub async fn wait_for_selector(
        &mut self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<bool, Error> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let found: bool = {
                v8::scope!(let scope, &mut self.isolate);
                let context = v8::Local::new(scope, &self.context);
                let slot = context
                    .get_slot::<DomSlot>()
                    .ok_or_else(|| Error::Runtime("no document loaded".into()))?;
                let result = slot.borrow().query_selector(selector).is_some();
                result
            };
            if found {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}
