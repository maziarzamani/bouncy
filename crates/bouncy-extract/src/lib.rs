//! Streaming HTML extractor.
//!
//! Backed by `lol_html` — SIMD-accelerated tag scanner with CSS selector
//! matching. Never builds a DOM tree; passes the document through once,
//! firing per-element / per-text handlers along the way.
//!
//! Phase 2 surface:
//!   - `extract_title(html) -> Option<String>`
//!   - `extract_text(html)  -> String`        (visible body text)
//!   - `extract_links(html, base) -> Vec<Link>` (resolved hrefs + text)

use std::cell::RefCell;
use std::rc::Rc;

use lol_html::{doc_text, element, end_tag, text, HtmlRewriter, Settings};
use thiserror::Error;
use url::Url;

#[derive(Error, Debug)]
pub enum Error {
    #[error("html rewriter: {0}")]
    Rewriter(#[from] lol_html::errors::RewritingError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub text: String,
}

/// Returns the `<title>` text, or `None` if there is no `<title>` element.
pub fn extract_title(html: &[u8]) -> Result<Option<String>, Error> {
    let title = Rc::new(RefCell::new(String::new()));
    let seen = Rc::new(RefCell::new(false));

    {
        let title_e = title.clone();
        let seen_e = seen.clone();
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("title", move |_| {
                        *seen_e.borrow_mut() = true;
                        Ok(())
                    }),
                    text!("title", move |t| {
                        title_e.borrow_mut().push_str(t.as_str());
                        Ok(())
                    }),
                ],
                ..Settings::new()
            },
            |_: &[u8]| {},
        );
        rewriter.write(html)?;
        rewriter.end()?;
    }

    if *seen.borrow() {
        Ok(Some(title.borrow().clone()))
    } else {
        Ok(None)
    }
}

/// Returns the document's visible text, skipping `<head>`, `<script>`,
/// `<style>`, `<noscript>`, and `<template>` content.
pub fn extract_text(html: &[u8]) -> Result<String, Error> {
    let text_buf = Rc::new(RefCell::new(String::new()));
    let skip_depth = Rc::new(RefCell::new(0i32));

    {
        let skip_e = skip_depth.clone();
        let text_d = text_buf.clone();
        let skip_d = skip_depth.clone();

        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![element!(
                    "head, script, style, noscript, template",
                    move |el| {
                        *skip_e.borrow_mut() += 1;
                        let skip_close = skip_e.clone();
                        el.on_end_tag(end_tag!(move |_| {
                            *skip_close.borrow_mut() -= 1;
                            Ok(())
                        }))?;
                        Ok(())
                    }
                )],
                document_content_handlers: vec![doc_text!(move |t| {
                    if *skip_d.borrow() == 0 {
                        // lol_html hands us the chunk verbatim — entities
                        // (&copy;, &amp;, numeric) are not decoded for us.
                        let decoded = html_escape::decode_html_entities(t.as_str());
                        text_d.borrow_mut().push_str(&decoded);
                    }
                    Ok(())
                })],
                ..Settings::new()
            },
            |_: &[u8]| {},
        );
        rewriter.write(html)?;
        rewriter.end()?;
    }

    let out = text_buf.borrow().clone();
    Ok(out)
}

/// Returns the document's `<a href>` links, with the href resolved against
/// `base` and the link text concatenated from all descendant text nodes.
pub fn extract_links(html: &[u8], base: &Url) -> Result<Vec<Link>, Error> {
    let links = Rc::new(RefCell::new(Vec::<Link>::new()));
    let pending = Rc::new(RefCell::new(None::<(String, String)>));

    {
        let pending_e = pending.clone();
        let pending_end = pending.clone();
        let links_end = links.clone();
        let pending_t = pending.clone();
        let base = base.clone();

        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("a[href]", move |el| {
                        let href = el.get_attribute("href").unwrap_or_default();
                        let resolved = base
                            .join(&href)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|_| href.clone());
                        *pending_e.borrow_mut() = Some((resolved, String::new()));

                        let pending_end = pending_end.clone();
                        let links_end = links_end.clone();
                        el.on_end_tag(end_tag!(move |_| {
                            if let Some((url, text)) = pending_end.borrow_mut().take() {
                                links_end.borrow_mut().push(Link {
                                    url,
                                    text: text.trim().to_string(),
                                });
                            }
                            Ok(())
                        }))?;
                        Ok(())
                    }),
                    text!("a[href]", move |t| {
                        if let Some((_, s)) = pending_t.borrow_mut().as_mut() {
                            let decoded = html_escape::decode_html_entities(t.as_str());
                            s.push_str(&decoded);
                        }
                        Ok(())
                    }),
                ],
                ..Settings::new()
            },
            |_: &[u8]| {},
        );
        rewriter.write(html)?;
        rewriter.end()?;
    }

    let out = links.borrow().clone();
    Ok(out)
}
