//! CSS-selector extraction helpers for `bouncy fetch --select` and
//! `bouncy scrape --select`.
//!
//! Wraps `bouncy_dom`'s `query_selector_all` plus per-element text /
//! attribute readout. Selector grammar is whatever bouncy-dom supports
//! today: tag, `#id`, `.class`, `[attr]`, `[attr=value]` — no
//! combinators or pseudo-classes. That covers the common scraping
//! cases; richer selectors land when bouncy-dom grows them.

use bouncy_dom::Document;

/// Extract the text content of every element matching `selector`,
/// in document order. Empty `Vec` if the document is empty, the
/// selector matches nothing, or the selector is unparseable
/// (bouncy-dom currently treats invalid selectors as no-match —
/// callers should sanity-check their selector if they get an empty
/// result they didn't expect).
pub fn select_text(html: &str, selector: &str) -> Result<Vec<String>, anyhow::Error> {
    let doc = Document::parse(html)?;
    let ids = doc.query_selector_all(selector);
    Ok(ids.into_iter().map(|id| doc.text_content(id)).collect())
}

/// Extract the named attribute value of every element matching
/// `selector`. Elements where the attribute is absent are skipped
/// rather than producing empty strings — `--select "a" --attr href`
/// against `<a>plain</a><a href="x">link</a>` returns `["x"]`, not
/// `["", "x"]`. Use `select_text` if you want one entry per match
/// regardless of attribute presence.
pub fn select_attr(html: &str, selector: &str, attr: &str) -> Result<Vec<String>, anyhow::Error> {
    let doc = Document::parse(html)?;
    let ids = doc.query_selector_all(selector);
    Ok(ids
        .into_iter()
        .filter_map(|id| doc.get_attribute(id, attr))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        <html><body>
            <h1 id="main">Hello</h1>
            <h1 class="sub">World</h1>
            <a href="https://a.example">Link A</a>
            <a>No href</a>
            <a href="https://b.example">Link B</a>
        </body></html>
    "#;

    #[test]
    fn select_text_returns_text_of_each_match_in_doc_order() {
        let out = select_text(SAMPLE, "h1").unwrap();
        assert_eq!(out, vec!["Hello", "World"]);
    }

    #[test]
    fn select_text_class_selector() {
        let out = select_text(SAMPLE, ".sub").unwrap();
        assert_eq!(out, vec!["World"]);
    }

    #[test]
    fn select_text_id_selector() {
        let out = select_text(SAMPLE, "#main").unwrap();
        assert_eq!(out, vec!["Hello"]);
    }

    #[test]
    fn select_text_no_match_returns_empty() {
        let out = select_text(SAMPLE, "h2").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn select_text_empty_html_returns_empty() {
        let out = select_text("", "h1").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn select_text_malformed_html_recovers() {
        // html5ever is forgiving; <h1>Hi (no close) becomes <h1>Hi</h1>.
        let out = select_text("<h1>Hi", "h1").unwrap();
        assert_eq!(out, vec!["Hi"]);
    }

    #[test]
    fn select_attr_returns_attribute_values_skipping_absent() {
        let out = select_attr(SAMPLE, "a", "href").unwrap();
        // The <a>No href</a> element is skipped, not empty-string'd.
        assert_eq!(out, vec!["https://a.example", "https://b.example"]);
    }

    #[test]
    fn select_attr_no_match_returns_empty() {
        let out = select_attr(SAMPLE, "img", "src").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn select_attr_missing_attribute_on_all_matches_returns_empty() {
        // All matches lack the attribute → all skipped, not panicked.
        let out = select_attr(SAMPLE, "h1", "href").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn select_text_nested_text_is_concatenated() {
        // text_content() recurses through children — nested spans
        // contribute their text in document order.
        let html = r#"<div class="card"><span>Hello </span><strong>world</strong></div>"#;
        let out = select_text(html, ".card").unwrap();
        assert_eq!(out, vec!["Hello world"]);
    }
}
