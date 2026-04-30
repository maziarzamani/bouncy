//! Tests for the NodeId-indexed read + mutation API.

use bouncy_dom::{Document, NodeId};

const PAGE: &str = r#"<!doctype html>
<html><head><title>X</title></head>
<body>
  <h1 id="title">Hello</h1>
  <ul id="items">
    <li class="item" data-id="1">A</li>
    <li class="item" data-id="2">B</li>
  </ul>
</body></html>"#;

#[test]
fn body_and_html_root_addressable() {
    let doc = Document::parse(PAGE).unwrap();
    let html = doc.html_root().unwrap();
    let body = doc.body().unwrap();
    assert_eq!(doc.tag_name(html).as_deref(), Some("html"));
    assert_eq!(doc.tag_name(body).as_deref(), Some("body"));
    assert_ne!(html, body);
    assert_ne!(html, NodeId::DOCUMENT);
}

#[test]
fn get_element_by_id() {
    let doc = Document::parse(PAGE).unwrap();
    let title = doc.get_element_by_id("title").unwrap();
    assert_eq!(doc.tag_name(title).as_deref(), Some("h1"));
    assert_eq!(doc.text_content(title).trim(), "Hello");

    let items = doc.get_element_by_id("items").unwrap();
    let kids = doc
        .children(items)
        .into_iter()
        .filter(|c| doc.tag_name(*c).is_some())
        .collect::<Vec<_>>();
    assert_eq!(kids.len(), 2);
}

#[test]
fn read_attribute() {
    let doc = Document::parse(PAGE).unwrap();
    let items = doc.get_element_by_id("items").unwrap();
    let first_li = doc
        .children(items)
        .into_iter()
        .find(|c| doc.tag_name(*c).as_deref() == Some("li"))
        .unwrap();
    assert_eq!(
        doc.get_attribute(first_li, "class").as_deref(),
        Some("item")
    );
    assert_eq!(doc.get_attribute(first_li, "data-id").as_deref(), Some("1"));
    assert_eq!(doc.get_attribute(first_li, "missing"), None);
}

#[test]
fn set_attribute_round_trip() {
    let mut doc = Document::parse(PAGE).unwrap();
    let body = doc.body().unwrap();
    doc.set_attribute(body, "data-ready", "1").unwrap();
    assert_eq!(doc.get_attribute(body, "data-ready").as_deref(), Some("1"));
    doc.set_attribute(body, "data-ready", "2").unwrap();
    assert_eq!(doc.get_attribute(body, "data-ready").as_deref(), Some("2"));
    doc.remove_attribute(body, "data-ready").unwrap();
    assert_eq!(doc.get_attribute(body, "data-ready"), None);
}

#[test]
fn set_text_content_replaces_children() {
    let mut doc = Document::parse(PAGE).unwrap();
    let title = doc.get_element_by_id("title").unwrap();
    doc.set_text_content(title, "Updated").unwrap();
    assert_eq!(doc.text_content(title), "Updated");
}

#[test]
fn create_and_append_child() {
    let mut doc = Document::parse(PAGE).unwrap();
    let items = doc.get_element_by_id("items").unwrap();
    let li = doc.create_element("li");
    doc.set_attribute(li, "class", "item").unwrap();
    doc.set_attribute(li, "data-id", "3").unwrap();
    let txt = doc.create_text_node("C");
    doc.append_child(li, txt).unwrap();
    doc.append_child(items, li).unwrap();

    // Verify it was inserted.
    let elements = doc
        .children(items)
        .into_iter()
        .filter(|c| doc.tag_name(*c).as_deref() == Some("li"))
        .collect::<Vec<_>>();
    assert_eq!(elements.len(), 3);
    assert_eq!(doc.text_content(elements[2]).trim(), "C");
    assert_eq!(
        doc.get_attribute(elements[2], "data-id").as_deref(),
        Some("3")
    );
}

#[test]
fn set_inner_html_replaces_subtree() {
    let mut doc = Document::parse(PAGE).unwrap();
    let title = doc.get_element_by_id("title").unwrap();
    doc.set_inner_html(title, "New <em>Hi</em>").unwrap();
    let kids: Vec<_> = doc.children(title);
    // Should now contain a text node + em element
    let tags: Vec<_> = kids
        .iter()
        .map(|id| doc.tag_name(*id).unwrap_or_default())
        .collect();
    assert!(tags.iter().any(|t| t == "em"), "tags: {tags:?}");
    assert!(
        doc.text_content(title).contains("Hi"),
        "got: {}",
        doc.text_content(title)
    );
}

#[test]
fn remove_child() {
    let mut doc = Document::parse(PAGE).unwrap();
    let items = doc.get_element_by_id("items").unwrap();
    let lis: Vec<_> = doc
        .children(items)
        .into_iter()
        .filter(|c| doc.tag_name(*c).as_deref() == Some("li"))
        .collect();
    assert_eq!(lis.len(), 2);
    doc.remove_child(items, lis[0]).unwrap();
    let after: Vec<_> = doc
        .children(items)
        .into_iter()
        .filter(|c| doc.tag_name(*c).as_deref() == Some("li"))
        .collect();
    assert_eq!(after.len(), 1);
}

#[test]
fn outer_html_round_trips() {
    let doc = Document::parse(PAGE).unwrap();
    let title = doc.get_element_by_id("title").unwrap();
    let outer = doc.outer_html(title);
    assert!(outer.contains("<h1"), "got: {outer}");
    assert!(outer.contains("Hello"), "got: {outer}");
    assert!(outer.contains("</h1>"), "got: {outer}");
}
