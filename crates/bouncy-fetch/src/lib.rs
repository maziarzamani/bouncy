//! HTTP client for bouncy.
//!
//! Thin wrapper over `hyper-util`'s pooled client with a `hyper-rustls`
//! connector. Trades reqwest's convenience for ~30% smaller dep tree, no
//! native-tls, and a tighter hot path.
//!
//! Surface:
//!   - `Fetcher::new()` / `Fetcher::builder()`
//!   - `Fetcher::get(url)`              shortcut for GET
//!   - `Fetcher::request(FetchRequest)` arbitrary method + headers + body
//!
//! Connection pooling and ALPN h1/h2 are on by default.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid uri: {0}")]
    Uri(#[from] http::uri::InvalidUri),

    #[error("unsupported scheme {0:?} (bouncy-fetch only speaks http/https)")]
    Scheme(String),

    #[error("request build error: {0}")]
    Build(#[from] http::Error),

    #[error("client error: {0}")]
    Client(#[from] hyper_util::client::legacy::Error),

    #[error("body error: {0}")]
    Body(#[from] hyper::Error),

    #[error("tls config: {0}")]
    Tls(String),

    #[error("invalid header name {0:?}")]
    HeaderName(String),

    #[error("invalid header value for {0:?}")]
    HeaderValue(String),

    #[error("invalid method {0:?}")]
    InvalidMethod(String),

    #[error("request timed out after {0:?}")]
    Timeout(Duration),

    #[error("too many redirects (cap: {0})")]
    TooManyRedirects(u32),

    #[error("redirect Location {0:?} could not be resolved against {1}")]
    BadRedirectLocation(String, String),
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Bytes,
    pub headers: HeaderMap,
}

/// Per-origin cookie store, shared across requests within a Fetcher.
/// Keyed by `scheme://host[:port]` — domain / path / expiration matching
/// is intentionally simplified. Cheap to clone (Arc-backed).
#[derive(Clone, Default)]
pub struct CookieJar {
    inner: Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a cookie for `origin`. Existing cookie with the same name
    /// is replaced.
    pub fn set(&self, origin: &str, name: &str, value: &str) {
        let mut g = self.inner.lock().unwrap();
        g.entry(origin.to_string())
            .or_default()
            .insert(name.to_string(), value.to_string());
    }

    pub fn get(&self, origin: &str, name: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        g.get(origin).and_then(|m| m.get(name).cloned())
    }

    /// Header value for an outgoing request, or None if no cookies.
    pub fn cookie_header(&self, origin: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        let m = g.get(origin)?;
        if m.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        Some(parts.join("; "))
    }

    /// Parse + record a single `Set-Cookie` header value.
    pub fn record_set_cookie(&self, origin: &str, raw: &str) {
        let first = raw.split(';').next().unwrap_or("").trim();
        if let Some((name, value)) = first.split_once('=') {
            let name = name.trim();
            if !name.is_empty() {
                self.set(origin, name, value.trim());
            }
        }
    }

    pub fn to_json(&self) -> String {
        let g = self.inner.lock().unwrap();
        serde_json::to_string(&*g).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let inner: HashMap<String, HashMap<String, String>> = serde_json::from_str(s)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }
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

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn is_known_encoding(s: &str) -> bool {
    matches!(s, "gzip" | "x-gzip" | "deflate" | "br")
}

fn decode_gzip(input: &[u8]) -> Result<Bytes, Error> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(input);
    let mut out = Vec::with_capacity(input.len() * 2);
    decoder
        .read_to_end(&mut out)
        .map_err(|e| Error::Tls(format!("gzip decode: {e}")))?;
    Ok(Bytes::from(out))
}

fn decode_deflate(input: &[u8]) -> Result<Bytes, Error> {
    use std::io::Read;
    // RFC 7230 says deflate = zlib (with header). Some servers ship
    // raw DEFLATE (no zlib wrapper) anyway. Try zlib first, fall back
    // to raw on header parse failure — same compatibility shim curl /
    // browsers have used for ~20 years.
    let mut zlib = flate2::read::ZlibDecoder::new(input);
    let mut out = Vec::with_capacity(input.len() * 2);
    if zlib.read_to_end(&mut out).is_ok() {
        return Ok(Bytes::from(out));
    }
    out.clear();
    let mut raw = flate2::read::DeflateDecoder::new(input);
    raw.read_to_end(&mut out)
        .map_err(|e| Error::Tls(format!("deflate decode: {e}")))?;
    Ok(Bytes::from(out))
}

fn decode_brotli(input: &[u8]) -> Result<Bytes, Error> {
    let mut out = Vec::with_capacity(input.len() * 4);
    let mut reader = brotli::Decompressor::new(input, 4096);
    use std::io::Read;
    reader
        .read_to_end(&mut out)
        .map_err(|e| Error::Tls(format!("brotli decode: {e}")))?;
    Ok(Bytes::from(out))
}

/// Resolve a Location header against the request's current URL. Handles
/// absolute (`https://other.test/`), origin-relative (`/foo`), and
/// fully-relative (`bar`, `../baz`) targets.
fn resolve_url(base: &str, location: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    base.join(location).ok().map(|u| u.to_string())
}

/// Apply RFC-aligned method/body rewriting for a redirect:
///
/// - 301/302/303: drop body, downgrade non-{GET,HEAD} method to GET
///   (matches every browser; technically 301/302 SHOULD preserve, but
///   reality won decades ago).
/// - 307/308: keep method, keep body.
///
/// Also drops `Content-Length` / `Content-Type` headers if we cleared
/// the body — they'd lie to the next hop.
fn rewrite_for_redirect(mut req: FetchRequest, status: u16, next_url: String) -> FetchRequest {
    req.url = next_url;
    if matches!(status, 301..=303) {
        let upper = req.method.to_uppercase();
        if upper != "GET" && upper != "HEAD" {
            req.method = "GET".into();
        }
        if !req.body.is_empty() {
            req.body = Bytes::new();
            req.headers.retain(|(n, _)| {
                let n = n.to_ascii_lowercase();
                n != "content-length" && n != "content-type"
            });
        }
    }
    req
}

/// Build the rustls `ClientConfig` shared by direct + proxied clients.
/// Always seeds native roots; appends extra trust anchors from the
/// supplied PEM files, if any. Empty file or no certs found is an error
/// — silently trusting nothing extra would be a footgun.
fn build_tls_config(extra_ca_files: &[PathBuf]) -> Result<rustls::ClientConfig, Error> {
    let mut roots = rustls::RootCertStore::empty();

    // Native roots first — keep behaviour parity with `with_native_roots`.
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        // Ignore individual cert parse errors (rustls's own helper does
        // the same): the broader handshake will fail loudly if we end up
        // with an empty store.
        let _ = roots.add(cert);
    }

    // Layer the user-provided PEM files on top.
    for path in extra_ca_files {
        use rustls::pki_types::pem::PemObject;
        let mut added = 0usize;
        let iter = rustls::pki_types::CertificateDer::pem_file_iter(path)
            .map_err(|e| Error::Tls(format!("ca-file open {}: {e}", path.display())))?;
        for cert_res in iter {
            let cert = cert_res
                .map_err(|e| Error::Tls(format!("ca-file parse {}: {e}", path.display())))?;
            roots
                .add(cert)
                .map_err(|e| Error::Tls(format!("ca-file add {}: {e}", path.display())))?;
            added += 1;
        }
        if added == 0 {
            return Err(Error::Tls(format!(
                "ca-file {} contained no certificates",
                path.display()
            )));
        }
    }

    if roots.is_empty() {
        return Err(Error::Tls(
            "trust store is empty (no native or user-supplied roots)".into(),
        ));
    }

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Built-in tracker / analytics hosts blocked by `TrackerBlocklist::default_set()`.
/// Intentionally tight — we'd rather miss a few than block legitimate
/// scraping targets.
const DEFAULT_TRACKERS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-tag.com",
    "connect.facebook.net",
    "scorecardresearch.com",
    "mixpanel.com",
    "segment.com",
    "segment.io",
    "hotjar.com",
    "fullstory.com",
    "amplitude.com",
];

/// Blocklist that short-circuits HTTP requests to known tracker hosts.
/// Match is "host equals entry, or host ends with `.entry`" — so an entry
/// of `google-analytics.com` blocks `www.google-analytics.com` too.
/// Cheap to clone (Arc-backed).
#[derive(Clone, Default)]
pub struct TrackerBlocklist {
    hosts: Arc<HashSet<String>>,
}

impl TrackerBlocklist {
    /// Empty blocklist.
    pub fn new() -> Self {
        Self::default()
    }

    /// bouncy's small built-in list of common ad / analytics hosts.
    pub fn default_set() -> Self {
        Self::from_hosts(DEFAULT_TRACKERS.iter().copied())
    }

    /// Build a blocklist from any iterable of host strings (no scheme,
    /// no path — just the bare host). Empty entries are ignored; entries
    /// are lower-cased.
    pub fn from_hosts(hs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut set = HashSet::new();
        for h in hs {
            let s = h.into().trim().to_ascii_lowercase();
            if !s.is_empty() {
                set.insert(s);
            }
        }
        Self {
            hosts: Arc::new(set),
        }
    }

    /// Iterate every host entry in the list. Order is unspecified.
    pub fn hosts_iter(&self) -> impl Iterator<Item = String> + '_ {
        self.hosts.iter().cloned()
    }

    /// True if `url`'s host (or `host:port` for non-default ports) matches
    /// an entry in the list, either exactly or as a subdomain suffix.
    pub fn blocks(&self, url: &str) -> bool {
        if self.hosts.is_empty() {
            return false;
        }
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let host_port = match parsed.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        };
        for entry in self.hosts.iter() {
            // Exact match — covers both bare host and host:port.
            if *entry == host || *entry == host_port {
                return true;
            }
            // Subdomain match: ".tracker.com" covers a.tracker.com.
            if host.len() > entry.len() && host.ends_with(entry) {
                let prefix_end = host.len() - entry.len();
                if host.as_bytes()[prefix_end - 1] == b'.' {
                    return true;
                }
            }
        }
        false
    }
}

/// Mutable builder for an HTTP request — method, URL, headers, body. Body
/// is owned (we always need to clone it once into hyper's body type) but
/// kept lazy until `Fetcher::request` consumes the builder.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl FetchRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub fn method(mut self, m: impl Into<String>) -> Self {
        self.method = m.into();
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body_str(mut self, body: impl Into<String>) -> Self {
        self.body = Bytes::from(body.into());
        self
    }

    pub fn body_bytes(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }
}

/// Concrete client type changes when a proxy is configured (the connector
/// tree wraps an extra Tunnel layer), and we'd rather not make `Fetcher`
/// generic — use a small enum and dispatch on it once per request.
#[allow(clippy::large_enum_variant)]
enum InnerClient {
    Direct(Client<HttpsConnector<HttpConnector>, Full<Bytes>>),
    Proxied(Client<HttpsConnector<Tunnel<HttpConnector>>, Full<Bytes>>),
}

pub struct Fetcher {
    client: InnerClient,
    request_timeout: Option<Duration>,
    cookie_jar: Option<CookieJar>,
    tracker_blocklist: Option<TrackerBlocklist>,
    /// Maximum redirect hops to follow. 0 disables following entirely
    /// (the 3xx response surfaces to the caller).
    max_redirects: u32,
}

impl Fetcher {
    pub fn new() -> Result<Self, Error> {
        Self::builder().build()
    }

    pub fn builder() -> FetcherBuilder {
        FetcherBuilder::default()
    }
}

pub struct FetcherBuilder {
    pool_idle_timeout: Duration,
    pool_max_idle_per_host: usize,
    /// HTTP CONNECT proxy URL (e.g., `http://proxy.example.com:3128`).
    /// HTTPS proxies aren't supported yet.
    proxy: Option<String>,
    /// Per-request total timeout — wraps the whole `request()` call,
    /// not just the connect or the first byte. None disables it.
    request_timeout: Option<Duration>,
    /// Optional shared cookie jar — when set, the Fetcher attaches a
    /// `Cookie` header to outgoing requests and harvests `Set-Cookie`
    /// from responses.
    cookie_jar: Option<CookieJar>,
    /// Optional tracker blocklist — short-circuits requests whose host
    /// matches an entry, returning a synthetic 204 with empty body.
    tracker_blocklist: Option<TrackerBlocklist>,
    /// Extra trust roots loaded from one or more PEM files. Added on
    /// top of the system's native root store.
    extra_ca_files: Vec<PathBuf>,
    /// Maximum redirect hops. 0 disables following.
    max_redirects: u32,
}

impl Default for FetcherBuilder {
    fn default() -> Self {
        Self {
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 16,
            proxy: None,
            request_timeout: None,
            cookie_jar: None,
            tracker_blocklist: None,
            extra_ca_files: Vec::new(),
            max_redirects: 10,
        }
    }
}

impl FetcherBuilder {
    pub fn pool_idle_timeout(mut self, d: Duration) -> Self {
        self.pool_idle_timeout = d;
        self
    }

    pub fn pool_max_idle_per_host(mut self, n: usize) -> Self {
        self.pool_max_idle_per_host = n;
        self
    }

    /// Route every outbound connection through an HTTP CONNECT proxy.
    /// Accepts `http://host:port` or bare `host:port`.
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Total per-request timeout. Wraps the whole `request()` call —
    /// connect + send + read body. None disables (default).
    pub fn request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = Some(d);
        self
    }

    /// Attach a shared cookie jar. Outgoing requests get a `Cookie`
    /// header populated for the request's origin; responses' `Set-Cookie`
    /// headers feed back into the jar.
    pub fn cookie_jar(mut self, jar: CookieJar) -> Self {
        self.cookie_jar = Some(jar);
        self
    }

    /// Block requests whose host matches the supplied tracker list.
    /// Blocked requests synthesise a 204 / empty body without opening
    /// a TCP connection.
    pub fn tracker_blocklist(mut self, list: TrackerBlocklist) -> Self {
        self.tracker_blocklist = Some(list);
        self
    }

    /// Trust extra CA certificates from a PEM file in addition to the
    /// system's native root store. Repeatable — call once per file.
    /// Useful for self-signed test servers and corporate MITM proxies.
    pub fn ca_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.extra_ca_files.push(path.into());
        self
    }

    /// Maximum redirect hops. 0 disables following — the 3xx response
    /// surfaces to the caller. Default: 10.
    pub fn max_redirects(mut self, n: u32) -> Self {
        self.max_redirects = n;
        self
    }

    pub fn build(self) -> Result<Fetcher, Error> {
        // TCP_NODELAY: hyper-util defaults to false. With Nagle on, the
        // request->response pipeline pays an extra ~40 ms RTT on cold
        // small requests. We always want low-latency.
        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        http.enforce_http(false);

        // Build a single rustls ClientConfig (used by both proxy + direct
        // paths). When extra CAs are provided we own the root store and
        // start from native roots, then layer in the user's PEM(s).
        let tls_config = build_tls_config(&self.extra_ca_files)?;

        let inner = match self.proxy {
            None => {
                let connector = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_tls_config(tls_config)
                    .https_or_http()
                    .enable_http1()
                    .enable_http2()
                    .wrap_connector(http);
                let client = Client::builder(TokioExecutor::new())
                    .pool_idle_timeout(self.pool_idle_timeout)
                    .pool_max_idle_per_host(self.pool_max_idle_per_host)
                    .build(connector);
                InnerClient::Direct(client)
            }
            Some(p) => {
                let proxy_uri: Uri = if p.contains("://") {
                    p.parse().map_err(Error::Uri)?
                } else {
                    format!("http://{p}").parse().map_err(Error::Uri)?
                };
                let tunnel = Tunnel::new(proxy_uri, http);
                let connector = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_tls_config(tls_config)
                    .https_or_http()
                    .enable_http1()
                    .enable_http2()
                    .wrap_connector(tunnel);
                let client = Client::builder(TokioExecutor::new())
                    .pool_idle_timeout(self.pool_idle_timeout)
                    .pool_max_idle_per_host(self.pool_max_idle_per_host)
                    .build(connector);
                InnerClient::Proxied(client)
            }
        };

        Ok(Fetcher {
            client: inner,
            request_timeout: self.request_timeout,
            cookie_jar: self.cookie_jar,
            tracker_blocklist: self.tracker_blocklist,
            max_redirects: self.max_redirects,
        })
    }
}

impl Fetcher {
    /// Shortcut for `request(FetchRequest::new(url))` (i.e., GET, no body,
    /// no extra headers). Returns a `Response`; for the headers, use the
    /// `request` API.
    pub async fn get(&self, url: &str) -> Result<Response, Error> {
        self.request(FetchRequest::new(url)).await
    }

    /// Drive an arbitrary HTTP request, following up to `max_redirects`
    /// 3xx hops automatically. Currently HTTP/1.1 + h2 only; only
    /// http/https schemes are accepted.
    pub async fn request(&self, mut req: FetchRequest) -> Result<Response, Error> {
        // Tracker blocklist: short-circuit before opening a connection.
        // Done outside the redirect loop because a blocked URL never
        // gets a redirect anyway.
        if let Some(bl) = self.tracker_blocklist.as_ref() {
            if bl.blocks(&req.url) {
                return Ok(Response {
                    status: 204,
                    body: Bytes::new(),
                    headers: HeaderMap::new(),
                });
            }
        }

        let mut hops: u32 = 0;
        loop {
            let resp = self.send_one(&req).await?;
            // Not a redirect, or following is disabled — return as-is.
            if !is_redirect(resp.status) || self.max_redirects == 0 {
                return Ok(resp);
            }
            if hops >= self.max_redirects {
                return Err(Error::TooManyRedirects(self.max_redirects));
            }
            let Some(location) = resp
                .headers
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
            else {
                // 3xx without a usable Location — treat as terminal.
                return Ok(resp);
            };
            let next_url = resolve_url(&req.url, &location)
                .ok_or_else(|| Error::BadRedirectLocation(location.clone(), req.url.clone()))?;
            req = rewrite_for_redirect(req, resp.status, next_url);
            hops += 1;
        }
    }

    async fn send_one(&self, req: &FetchRequest) -> Result<Response, Error> {
        let uri: Uri = req.url.parse()?;
        match uri.scheme_str() {
            Some("http") | Some("https") => {}
            Some(other) => return Err(Error::Scheme(other.to_string())),
            None => return Err(Error::Scheme(String::new())),
        }

        let method = Method::from_bytes(req.method.as_bytes())
            .map_err(|_| Error::InvalidMethod(req.method.clone()))?;

        let mut builder = Request::builder().method(method).uri(uri);
        let header_map = builder.headers_mut().unwrap();
        for (name, value) in &req.headers {
            let n = HeaderName::try_from(name.as_bytes())
                .map_err(|_| Error::HeaderName(name.clone()))?;
            let v = HeaderValue::try_from(value.as_bytes())
                .map_err(|_| Error::HeaderValue(name.clone()))?;
            header_map.append(n, v);
        }

        // Default Accept-Encoding: real-browser-shaped, lets servers
        // hand us compressed bodies. Only added if the caller didn't
        // set their own (some servers behave differently for explicit
        // `identity` vs. missing).
        if !req
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("accept-encoding"))
        {
            header_map.append(
                http::header::ACCEPT_ENCODING,
                HeaderValue::from_static("gzip, deflate, br"),
            );
        }

        // Cookie jar: attach Cookie header for this origin if any.
        let origin = origin_for(&req.url);
        if let (Some(jar), Some(o)) = (self.cookie_jar.as_ref(), origin.as_deref()) {
            if !req
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("cookie"))
            {
                if let Some(cookie_value) = jar.cookie_header(o) {
                    if let Ok(v) = HeaderValue::try_from(cookie_value.as_bytes()) {
                        header_map.append(http::header::COOKIE, v);
                    }
                }
            }
        }

        let http_req = builder.body(Full::new(req.body.clone()))?;
        let send = async {
            match &self.client {
                InnerClient::Direct(c) => c.request(http_req).await,
                InnerClient::Proxied(c) => c.request(http_req).await,
            }
        };
        let resp = match self.request_timeout {
            Some(d) => match tokio::time::timeout(d, send).await {
                Ok(r) => r?,
                Err(_) => return Err(Error::Timeout(d)),
            },
            None => send.await?,
        };
        let status = resp.status().as_u16();
        let mut headers = resp.headers().clone();

        // Harvest Set-Cookie into the jar before consuming the response.
        if let (Some(jar), Some(o)) = (self.cookie_jar.as_ref(), origin.as_deref()) {
            for sc in headers.get_all(http::header::SET_COOKIE) {
                if let Ok(s) = sc.to_str() {
                    jar.record_set_cookie(o, s);
                }
            }
        }

        let raw_body = resp.into_body().collect().await?.to_bytes();
        let encoding = headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_ascii_lowercase());
        let body = match encoding.as_deref() {
            Some("gzip") | Some("x-gzip") => decode_gzip(&raw_body)?,
            Some("deflate") => decode_deflate(&raw_body)?,
            Some("br") => decode_brotli(&raw_body)?,
            // Identity, none, or anything we don't decode — pass through.
            _ => raw_body,
        };
        // If we decoded, strip the encoding-specific headers so callers
        // (and any downstream proxy) don't double-decode or trust the
        // pre-decode Content-Length.
        if encoding.as_deref().is_some_and(is_known_encoding) {
            headers.remove(http::header::CONTENT_ENCODING);
            headers.remove(http::header::CONTENT_LENGTH);
        }
        Ok(Response {
            status,
            body,
            headers,
        })
    }

    /// Returns a clone of the Fetcher's cookie jar, if one was configured.
    pub fn cookie_jar(&self) -> Option<CookieJar> {
        self.cookie_jar.clone()
    }

    /// Returns a clone of the Fetcher's tracker blocklist, if any.
    pub fn tracker_blocklist(&self) -> Option<TrackerBlocklist> {
        self.tracker_blocklist.clone()
    }
}
