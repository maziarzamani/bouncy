//! Phase 4 tests for bouncy-dom — html5ever wrapper.

use bouncy_dom::Document;

#[test]
fn parses_and_serialises_round_trip_text() {
    // Round-trip for the string content; the serialiser may normalise the
    // doctype / whitespace, so we only assert text content is preserved.
    let src = "<!doctype html><html><head><title>X</title></head><body><p>hi</p></body></html>";
    let doc = Document::parse(src).unwrap();
    let s = doc.serialize();
    assert!(s.contains("<title>X</title>"), "got: {s}");
    assert!(s.contains("<p>hi</p>"), "got: {s}");
}

#[test]
fn extracts_title() {
    let doc = Document::parse(
        "<!doctype html><html><head><title>Hello</title></head><body></body></html>",
    )
    .unwrap();
    assert_eq!(doc.title().as_deref(), Some("Hello"));
}

#[test]
fn missing_title_yields_none() {
    let doc = Document::parse("<html><body><h1>x</h1></body></html>").unwrap();
    assert_eq!(doc.title(), None);
}

#[test]
fn body_text_skips_script_and_style() {
    let doc = Document::parse(
        r#"<html>
            <head><style>.a{color:red}</style></head>
            <body>
              <script>console.log('x')</script>
              <p>visible</p>
            </body>
        </html>"#,
    )
    .unwrap();
    let body = doc.body_text();
    assert!(body.contains("visible"));
    assert!(!body.contains("color:red"), "style leaked: {body}");
    assert!(!body.contains("console.log"), "script leaked: {body}");
}

#[test]
fn empty_input_does_not_panic() {
    let doc = Document::parse("").unwrap();
    assert_eq!(doc.title(), None);
    assert_eq!(doc.body_text(), "");
}
