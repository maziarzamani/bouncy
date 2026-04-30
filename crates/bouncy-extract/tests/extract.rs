//! Phase 2 tests for bouncy-extract.

use bouncy_extract::{extract_links, extract_text, extract_title};
use url::Url;

const STATIC_FIXTURE: &str = r#"
<!doctype html>
<html lang="en">
<head>
  <title>Demo Page</title>
  <style>.x { color: red; }</style>
  <script>console.log('hi');</script>
</head>
<body>
  <header><h1>Hello</h1></header>
  <nav>
    <a href="/about">About</a>
    <a href="https://example.com/help">Help</a>
    <a href="page.html"><span>Span text</span></a>
  </nav>
  <article>
    <p>First paragraph.</p>
    <p>Second <strong>paragraph</strong>.</p>
    <noscript>You need JS.</noscript>
  </article>
  <footer>End.</footer>
</body>
</html>
"#;

#[test]
fn title_returns_title_text() {
    let t = extract_title(STATIC_FIXTURE.as_bytes()).unwrap();
    assert_eq!(t.as_deref(), Some("Demo Page"));
}

#[test]
fn title_missing_yields_none() {
    let t = extract_title(b"<html><body>no title</body></html>").unwrap();
    assert_eq!(t, None);
}

#[test]
fn links_resolves_relative_against_base() {
    let base = Url::parse("https://site.test/path/").unwrap();
    let links = extract_links(STATIC_FIXTURE.as_bytes(), &base).unwrap();
    let urls: Vec<&str> = links.iter().map(|l| l.url.as_str()).collect();
    assert!(urls.contains(&"https://site.test/about"));
    assert!(urls.contains(&"https://example.com/help"));
    assert!(urls.contains(&"https://site.test/path/page.html"));
    assert_eq!(links.len(), 3);
}

#[test]
fn links_capture_text_including_descendants() {
    let base = Url::parse("https://site.test/").unwrap();
    let links = extract_links(STATIC_FIXTURE.as_bytes(), &base).unwrap();
    let third = links.iter().find(|l| l.url.ends_with("page.html")).unwrap();
    assert_eq!(third.text, "Span text");
}

#[test]
fn text_skips_script_and_style() {
    let txt = extract_text(STATIC_FIXTURE.as_bytes()).unwrap();
    assert!(!txt.contains("console.log"), "script content leaked: {txt}");
    assert!(!txt.contains("color: red"), "style content leaked: {txt}");
    assert!(txt.contains("Hello"));
    assert!(txt.contains("First paragraph"));
    assert!(txt.contains("Second paragraph."), "got: {txt:?}");
}

#[test]
fn text_skips_noscript() {
    let txt = extract_text(STATIC_FIXTURE.as_bytes()).unwrap();
    assert!(!txt.contains("You need JS"), "noscript leaked: {txt}");
}

#[test]
fn empty_html_does_not_panic() {
    assert_eq!(extract_title(b"").unwrap(), None);
    assert_eq!(extract_text(b"").unwrap(), "");
    let base = Url::parse("https://x.test/").unwrap();
    assert!(extract_links(b"", &base).unwrap().is_empty());
}

#[test]
fn text_decodes_html_entities() {
    let html = b"<html><body><p>&copy; 2026 &amp; &#169; &#xA9; &nbsp;ok</p></body></html>";
    let txt = extract_text(html).unwrap();
    assert!(
        txt.contains("\u{00A9}"),
        "expected © (U+00A9), got: {txt:?}"
    );
    assert!(txt.contains(" & "), "&amp; not decoded: {txt:?}");
    assert!(!txt.contains("&copy;"), "literal &copy; leaked: {txt:?}");
    assert!(
        !txt.contains("&#169"),
        "numeric entity not decoded: {txt:?}"
    );
}

#[test]
fn link_text_decodes_entities() {
    let base = Url::parse("https://x.test/").unwrap();
    let links = extract_links(
        b"<html><body><a href='/'>Tom &amp; Jerry</a></body></html>",
        &base,
    )
    .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].text, "Tom & Jerry");
}
