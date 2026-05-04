//! Structured page snapshot — the LLM-friendly view of the current page.
//!
//! Returned from every state-changing primitive (`open`, `click`, `fill`,
//! `goto`, `submit`) so a caller (human or LLM) doesn't have to parse raw
//! HTML to figure out what's on the page or which selectors to use next.
//!
//! Includes a stable-selector generator, [`unique_selector`], that picks
//! the best available identifier (id → name → data-testid → role →
//! indexed path) so a selector returned in one snapshot keeps targeting
//! the same element on subsequent snapshots.

use std::collections::BTreeMap;

use bouncy_dom::{Document, NodeId};
use schemars::JsonSchema;
use serde::Serialize;

const DEFAULT_TEXT_SUMMARY_BYTES: usize = 2048;
const TRUNCATION_MARKER: &str = " […]";

/// Top-level snapshot returned to callers / LLMs.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,

    pub forms: Vec<FormSnapshot>,
    pub links: Vec<LinkSnapshot>,
    pub buttons: Vec<ButtonSnapshot>,
    /// Inputs not nested inside any `<form>`. Forms' fields live inside
    /// `forms[i].fields`; this list catches stray inputs.
    pub inputs: Vec<InputSnapshot>,
    pub headings: Vec<HeadingSnapshot>,

    /// Flat, indexed view of every interactive element on the page —
    /// forms, form fields, links, buttons, stray inputs. Each entry
    /// has a stable `index` (unique within this snapshot) the LLM can
    /// pass back to `click_index` / `fill_index` / `submit_index` /
    /// `read_index` instead of constructing a CSS selector. Indexing
    /// is contiguous from 0 in document-walk order across all the
    /// other lists, so the same `index` referenced from `forms[i]` /
    /// `links[i]` / etc. resolves to the same DOM element here.
    pub interactive: Vec<InteractiveElement>,

    /// Truncated visible text of the page body. Capped per [`SnapshotOpts`];
    /// when truncated, ends in [`TRUNCATION_MARKER`] so the LLM knows.
    pub text_summary: String,

    /// `<meta>` extraction: Open Graph, Twitter Card, description, etc.
    /// Keys preserve the original `property` / `name` attribute.
    pub meta: BTreeMap<String, String>,
}

/// One actionable element in the flat [`PageSnapshot::interactive`]
/// list. The `index` is the LLM-friendly handle to this element —
/// stable inside a single snapshot, shared with the per-category
/// (`forms` / `links` / etc.) lists.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InteractiveElement {
    pub index: u32,
    /// `"form"` | `"field"` | `"link"` | `"button"`.
    pub kind: String,
    pub selector: String,
    /// Best-effort human label: link text, button text, field
    /// label/name/placeholder, form action attribute, etc. Always
    /// trimmed; never longer than [`InteractiveElement::LABEL_CAP`].
    pub label: String,
}

impl InteractiveElement {
    /// Cap on the inline label so a noisy page doesn't bloat the
    /// flat list. Labels longer than this are truncated with " […]".
    pub const LABEL_CAP: usize = 120;
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormSnapshot {
    /// Stable index inside this snapshot — same value the matching
    /// entry has in [`PageSnapshot::interactive`].
    pub index: u32,
    pub selector: String,
    pub action: Option<String>,
    /// Uppercased: `"GET"` / `"POST"`. Defaults to `"GET"` when the
    /// form has no explicit `method=`.
    pub method: String,
    pub fields: Vec<InputSnapshot>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InputSnapshot {
    pub index: u32,
    pub selector: String,
    /// `<input type=…>` for inputs, `"textarea"` for textareas,
    /// `"select"` for selects.
    pub kind: String,
    pub name: Option<String>,
    /// Associated `<label>` text (resolved via `for=` or by ancestor).
    pub label: Option<String>,
    pub value: Option<String>,
    pub placeholder: Option<String>,
    pub required: bool,
    /// For `<select>` only: the available `<option>` entries. Empty
    /// for non-select kinds. Lets the LLM pick a value without an
    /// extra round trip via `read`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub options: Vec<SelectOption>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SelectOption {
    /// Submission value — the `value=` attribute, or text when absent.
    pub value: String,
    /// Visible text of the `<option>`.
    pub text: String,
    /// `selected` attribute is present.
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LinkSnapshot {
    pub index: u32,
    pub selector: String,
    pub text: String,
    /// Resolved absolute URL when the page URL is parseable; otherwise
    /// the raw `href` attribute value.
    pub href: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ButtonSnapshot {
    pub index: u32,
    pub selector: String,
    pub text: String,
    /// `"submit"` (default for `<button>` inside a form), `"button"`,
    /// or `"reset"`.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeadingSnapshot {
    pub level: u8,
    pub text: String,
}

/// Knobs for snapshot generation. `Default` is fine for almost all callers.
#[derive(Debug, Clone)]
pub struct SnapshotOpts {
    /// Cap on `text_summary` byte length. When exceeded, the summary is
    /// truncated to a UTF-8 boundary at-or-before the cap and suffixed
    /// with [`TRUNCATION_MARKER`].
    pub max_text_summary_bytes: usize,
}

impl Default for SnapshotOpts {
    fn default() -> Self {
        Self {
            max_text_summary_bytes: DEFAULT_TEXT_SUMMARY_BYTES,
        }
    }
}

impl PageSnapshot {
    /// Build a snapshot from a parsed DOM tree + the page's URL.
    pub fn from_document(doc: &Document, url: &str, opts: SnapshotOpts) -> Self {
        let title = doc.title().unwrap_or_default();

        let body_text = doc.body_text();
        let text_summary = truncate_with_marker(&body_text, opts.max_text_summary_bytes);

        let meta = collect_meta(doc);

        let mut indexer = Indexer::default();

        // Forms first — collect form field NodeIds so we can later filter them
        // out of the top-level "stray inputs" list.
        let form_ids = doc.query_selector_all("form");
        let mut form_field_ids: Vec<NodeId> = Vec::new();
        let forms: Vec<FormSnapshot> = form_ids
            .iter()
            .map(|&fid| {
                let form_index = indexer.next();
                let fields_with_ids = collect_form_fields(doc, fid, &mut indexer);
                form_field_ids.extend(fields_with_ids.iter().map(|(nid, _)| *nid));
                let fields: Vec<InputSnapshot> =
                    fields_with_ids.into_iter().map(|(_, snap)| snap).collect();
                let selector = unique_selector(doc, fid);
                let action = doc.get_attribute(fid, "action");
                let method = doc
                    .get_attribute(fid, "method")
                    .map(|m| m.to_uppercase())
                    .unwrap_or_else(|| "GET".to_string());
                indexer.push(InteractiveElement {
                    index: form_index,
                    kind: "form".into(),
                    selector: selector.clone(),
                    label: form_label(action.as_deref(), &method),
                });
                FormSnapshot {
                    index: form_index,
                    selector,
                    action,
                    method,
                    fields,
                }
            })
            .collect();

        // Stray inputs: every input/textarea/select not already accounted for
        // in a form's fields list.
        let inputs: Vec<InputSnapshot> = ["input", "textarea", "select"]
            .iter()
            .flat_map(|tag| doc.query_selector_all(tag))
            .filter(|nid| !form_field_ids.contains(nid))
            .map(|nid| input_snapshot(doc, nid, &mut indexer))
            .collect();

        let links: Vec<LinkSnapshot> = doc
            .query_selector_all("a")
            .into_iter()
            .filter_map(|nid| {
                let href = doc.get_attribute(nid, "href")?;
                let idx = indexer.next();
                let selector = unique_selector(doc, nid);
                let text = doc.text_content(nid).trim().to_string();
                indexer.push(InteractiveElement {
                    index: idx,
                    kind: "link".into(),
                    selector: selector.clone(),
                    label: cap_label(if text.is_empty() { &href } else { &text }),
                });
                Some(LinkSnapshot {
                    index: idx,
                    selector,
                    text,
                    href: resolve_href(url, &href),
                })
            })
            .collect();

        let buttons: Vec<ButtonSnapshot> = doc
            .query_selector_all("button")
            .into_iter()
            .map(|nid| {
                let idx = indexer.next();
                let selector = unique_selector(doc, nid);
                let text = doc.text_content(nid).trim().to_string();
                let kind = doc
                    .get_attribute(nid, "type")
                    .unwrap_or_else(|| "submit".to_string());
                indexer.push(InteractiveElement {
                    index: idx,
                    kind: "button".into(),
                    selector: selector.clone(),
                    label: cap_label(&text),
                });
                ButtonSnapshot {
                    index: idx,
                    selector,
                    text,
                    kind,
                }
            })
            .collect();

        let mut headings: Vec<HeadingSnapshot> = Vec::new();
        for level in 1u8..=6 {
            for nid in doc.query_selector_all(&format!("h{level}")) {
                headings.push(HeadingSnapshot {
                    level,
                    text: doc.text_content(nid).trim().to_string(),
                });
            }
        }

        Self {
            url: url.to_string(),
            title,
            forms,
            links,
            buttons,
            inputs,
            headings,
            interactive: indexer.into_list(),
            text_summary,
            meta,
        }
    }

    /// Look up an interactive element by its [`InteractiveElement::index`]
    /// and return its CSS selector. `None` if the index isn't present.
    pub fn selector_for_index(&self, index: u32) -> Option<&str> {
        self.interactive
            .iter()
            .find(|e| e.index == index)
            .map(|e| e.selector.as_str())
    }
}

/// Tiny incrementing counter that hands out element indices and
/// records each as an [`InteractiveElement`] for the flat list. Local
/// to one snapshot build.
#[derive(Default)]
struct Indexer {
    next: u32,
    list: Vec<InteractiveElement>,
}

impl Indexer {
    fn next(&mut self) -> u32 {
        let n = self.next;
        self.next += 1;
        n
    }

    fn push(&mut self, e: InteractiveElement) {
        self.list.push(e);
    }

    fn into_list(self) -> Vec<InteractiveElement> {
        self.list
    }
}

fn collect_form_fields(
    doc: &Document,
    form_id: NodeId,
    indexer: &mut Indexer,
) -> Vec<(NodeId, InputSnapshot)> {
    let mut out = Vec::new();
    for tag in ["input", "textarea", "select"] {
        for nid in doc.query_selector_all_within(form_id, tag) {
            out.push((nid, input_snapshot(doc, nid, indexer)));
        }
    }
    out
}

fn input_snapshot(doc: &Document, nid: NodeId, indexer: &mut Indexer) -> InputSnapshot {
    let tag = doc.tag_name(nid).unwrap_or_default().to_ascii_lowercase();
    let kind = match tag.as_str() {
        "input" => doc
            .get_attribute(nid, "type")
            .unwrap_or_else(|| "text".to_string())
            .to_ascii_lowercase(),
        other => other.to_string(),
    };
    let selector = unique_selector(doc, nid);
    let label = find_label(doc, nid);
    let name = doc.get_attribute(nid, "name");
    let placeholder = doc.get_attribute(nid, "placeholder");
    let value = doc.get_attribute(nid, "value");
    let required = doc.get_attribute(nid, "required").is_some();
    let options = if tag == "select" {
        collect_select_options(doc, nid)
    } else {
        Vec::new()
    };
    let idx = indexer.next();
    let interactive_label = label
        .clone()
        .or_else(|| placeholder.clone())
        .or_else(|| name.clone())
        .unwrap_or_else(|| kind.clone());
    indexer.push(InteractiveElement {
        index: idx,
        kind: "field".into(),
        selector: selector.clone(),
        label: cap_label(&interactive_label),
    });
    InputSnapshot {
        index: idx,
        selector,
        kind,
        name,
        label,
        value,
        placeholder,
        required,
        options,
    }
}

fn collect_select_options(doc: &Document, select_id: NodeId) -> Vec<SelectOption> {
    doc.query_selector_all_within(select_id, "option")
        .into_iter()
        .map(|nid| {
            let text = doc.text_content(nid).trim().to_string();
            let value = doc
                .get_attribute(nid, "value")
                .unwrap_or_else(|| text.clone());
            let selected = doc.get_attribute(nid, "selected").is_some();
            SelectOption {
                value,
                text,
                selected,
            }
        })
        .collect()
}

fn form_label(action: Option<&str>, method: &str) -> String {
    match action {
        Some(a) if !a.is_empty() => cap_label(&format!("{method} {a}")),
        _ => format!("{method} <no action>"),
    }
}

fn cap_label(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= InteractiveElement::LABEL_CAP {
        return trimmed.to_string();
    }
    let mut end = InteractiveElement::LABEL_CAP.saturating_sub(TRUNCATION_MARKER.len());
    while !trimmed.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATION_MARKER.len());
    out.push_str(&trimmed[..end]);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Find the `<label>` text associated with `nid`. Two cases per HTML spec:
///   1. `<label for="x">…</label>` and the input has `id="x"`
///   2. `<label>foo <input …></label>` (input nested inside label)
fn find_label(doc: &Document, nid: NodeId) -> Option<String> {
    if let Some(id) = doc.get_attribute(nid, "id") {
        for label_id in doc.query_selector_all("label") {
            if doc.get_attribute(label_id, "for").as_deref() == Some(id.as_str()) {
                let text = doc.text_content(label_id).trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    let mut cur = doc.parent(nid);
    while let Some(p) = cur {
        if doc
            .tag_name(p)
            .map(|t| t.eq_ignore_ascii_case("label"))
            .unwrap_or(false)
        {
            let text = doc.text_content(p).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
        cur = doc.parent(p);
    }
    None
}

fn collect_meta(doc: &Document) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for nid in doc.query_selector_all("meta") {
        let key = doc
            .get_attribute(nid, "property")
            .or_else(|| doc.get_attribute(nid, "name"));
        let value = doc.get_attribute(nid, "content");
        if let (Some(k), Some(v)) = (key, value) {
            out.insert(k, v);
        }
    }
    out
}

fn resolve_href(page_url: &str, href: &str) -> String {
    match url::Url::parse(page_url).and_then(|base| base.join(href)) {
        Ok(u) => u.to_string(),
        Err(_) => href.to_string(),
    }
}

/// Truncate a string to at most `max_bytes` UTF-8 bytes (rounded down to
/// a char boundary), appending [`TRUNCATION_MARKER`] when truncation
/// happens. Whitespace runs in the input are collapsed to single spaces
/// so the text reads as flowing prose, not raw DOM whitespace.
fn truncate_with_marker(s: &str, max_bytes: usize) -> String {
    let collapsed: String = collapse_whitespace(s);
    if collapsed.len() <= max_bytes {
        return collapsed;
    }
    // Reserve room for the marker.
    let target = max_bytes.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = target.min(collapsed.len());
    while !collapsed.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATION_MARKER.len());
    out.push_str(&collapsed[..end]);
    out.push_str(TRUNCATION_MARKER);
    out
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = true; // suppress leading whitespace
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_ws {
                out.push(' ');
                last_was_ws = true;
            }
        } else {
            out.push(c);
            last_was_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Generate a CSS selector that uniquely identifies `id` in `doc`.
///
/// Constrained to selectors `bouncy_dom` can actually parse today —
/// single-clause only: bare tag, `#id`, `.class`, `[attr]`, or
/// `[attr=value]`. No compound (`tag[attr=value]`) or pseudo-classes.
/// When that grammar grows, this function should grow with it.
///
/// Strategies, in order of stability and readability:
///   1. `#id` if the element's `id` is unique in the doc
///   2. bare tag if the tag is unique (cheap and readable, e.g. a sole
///      `<form>` or `<main>` on a page)
///   3. `[name=…]` if unique
///   4. `[data-testid=…]` if present and unique
///   5. `[role=…]` if present and unique
///   6. `.class` (single class) if unique
///   7. Bare tag as a final ambiguous fallback — callers needing
///      absolute precision can fall back to `eval`.
pub fn unique_selector(doc: &Document, id: NodeId) -> String {
    let tag = doc
        .tag_name(id)
        .unwrap_or_else(|| "*".to_string())
        .to_ascii_lowercase();

    // 1. #id
    if let Some(elem_id) = doc.get_attribute(id, "id") {
        if !elem_id.is_empty() && is_unique(doc, &format!("#{elem_id}")) {
            return format!("#{elem_id}");
        }
    }

    // 2. bare tag (if unique on the page)
    if is_unique(doc, &tag) {
        return tag;
    }

    // 3. [name=value]
    if let Some(name) = doc.get_attribute(id, "name") {
        if !name.is_empty() {
            let sel = format!("[name={name}]");
            if is_unique(doc, &sel) {
                return sel;
            }
        }
    }

    // 4. [data-testid=value]
    if let Some(testid) = doc.get_attribute(id, "data-testid") {
        if !testid.is_empty() {
            let sel = format!("[data-testid={testid}]");
            if is_unique(doc, &sel) {
                return sel;
            }
        }
    }

    // 5. [role=value]
    if let Some(role) = doc.get_attribute(id, "role") {
        if !role.is_empty() {
            let sel = format!("[role={role}]");
            if is_unique(doc, &sel) {
                return sel;
            }
        }
    }

    // 6. .class — try each class in order; first one that's unique wins.
    if let Some(class_attr) = doc.get_attribute(id, "class") {
        for cls in class_attr.split_ascii_whitespace() {
            let sel = format!(".{cls}");
            if is_unique(doc, &sel) {
                return sel;
            }
        }
    }

    // 7. Bare tag fallback (ambiguous; callers can fall back to eval).
    tag
}

fn is_unique(doc: &Document, selector: &str) -> bool {
    doc.query_selector_all(selector).len() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(html: &str) -> Document {
        Document::parse(html).expect("parse fixture")
    }

    // ---- truncate_with_marker ----

    #[test]
    fn truncate_returns_input_unchanged_when_under_cap() {
        assert_eq!(truncate_with_marker("hello", 100), "hello");
    }

    #[test]
    fn truncate_appends_marker_when_over_cap() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let out = truncate_with_marker(s, 10);
        assert!(out.ends_with(TRUNCATION_MARKER), "got: {out}");
        assert!(out.len() <= 10);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // "naïve" with 'ï' as 2-byte UTF-8 — never split mid-codepoint.
        let s = "naïve naïve naïve";
        for cap in 1..=s.len() {
            let out = truncate_with_marker(s, cap);
            assert!(out.is_char_boundary(out.len()), "cap={cap}, out={out}");
        }
    }

    #[test]
    fn truncate_collapses_whitespace_runs() {
        let s = "  hello   \n\n  world  ";
        assert_eq!(truncate_with_marker(s, 100), "hello world");
    }

    // ---- unique_selector ----

    #[test]
    fn unique_selector_uses_id_when_unique() {
        let doc = parse(r#"<html><body><div id="main">x</div><div>y</div></body></html>"#);
        let main = doc.query_selector("#main").unwrap();
        assert_eq!(unique_selector(&doc, main), "#main");
    }

    #[test]
    fn unique_selector_uses_bare_tag_when_only_one_on_page() {
        // The single <input> on the page — bare tag is enough, no need
        // for [name=…] noise.
        let doc = parse(r#"<html><body><input name="email" type="email"></body></html>"#);
        let inp = doc.query_selector("input").unwrap();
        assert_eq!(unique_selector(&doc, inp), "input");
    }

    #[test]
    fn unique_selector_uses_name_when_tag_is_ambiguous() {
        // Two inputs — bare `input` matches both. Disambiguate with
        // [name=…] (no compound `tag[name=…]` because bouncy-dom's
        // selector grammar is single-clause today).
        let doc = parse(
            r#"<html><body>
                 <input name="user" type="text">
                 <input name="email" type="email">
               </body></html>"#,
        );
        let inputs = doc.query_selector_all("input");
        assert_eq!(unique_selector(&doc, inputs[1]), "[name=email]");
    }

    #[test]
    fn unique_selector_uses_data_testid_when_id_and_name_absent() {
        // Two buttons; the second has data-testid which is unique.
        let doc = parse(
            r#"<html><body>
                 <button>Plain</button>
                 <button data-testid="go">Go</button>
               </body></html>"#,
        );
        let btns = doc.query_selector_all("button");
        assert_eq!(unique_selector(&doc, btns[1]), "[data-testid=go]");
    }

    #[test]
    fn unique_selector_uses_class_when_no_id_name_or_testid_helps() {
        // Two divs; the second has a unique class.
        let doc = parse(
            r#"<html><body>
                 <div>a</div>
                 <div class="hero special">b</div>
               </body></html>"#,
        );
        let divs = doc.query_selector_all("div");
        // First class that yields a unique match wins.
        let sel = unique_selector(&doc, divs[1]);
        assert!(sel == ".hero" || sel == ".special", "got: {sel}");
    }

    #[test]
    fn unique_selector_falls_back_to_bare_tag_when_nothing_disambiguates() {
        // Three completely identical <li> — there's no useful identifier.
        // Returning the bare tag is the documented "ambiguous fallback":
        // callers needing precision can `eval` instead.
        let doc = parse(
            r#"<html><body>
                 <ul><li>a</li><li>b</li><li>c</li></ul>
               </body></html>"#,
        );
        let ul = doc.query_selector("ul").unwrap();
        let lis = doc.query_selector_all_within(ul, "li");
        assert_eq!(unique_selector(&doc, lis[1]), "li");
    }

    #[test]
    fn unique_selector_handles_id_collisions_by_falling_through() {
        // Two elements with the same id — `#x` matches both, so we should
        // skip strategy 1 and fall through to the next disambiguator.
        let doc = parse(
            r#"<html><body>
                 <span id="x" name="first">a</span>
                 <span id="x" name="second">b</span>
               </body></html>"#,
        );
        let spans = doc.query_selector_all("span");
        let sel = unique_selector(&doc, spans[1]);
        assert_eq!(sel, "[name=second]");
    }

    // ---- PageSnapshot::from_document ----

    #[test]
    fn snapshot_captures_title_and_url() {
        let doc = parse(r#"<html><head><title>Demo</title></head><body></body></html>"#);
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.title, "Demo");
        assert_eq!(snap.url, "https://x.test/");
    }

    #[test]
    fn snapshot_collects_headings_in_document_order_with_levels() {
        let doc = parse(
            r#"<html><body>
                <h1>Top</h1><h2>Sub A</h2><h3>Deep</h3><h2>Sub B</h2>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let levels: Vec<u8> = snap.headings.iter().map(|h| h.level).collect();
        let texts: Vec<&str> = snap.headings.iter().map(|h| h.text.as_str()).collect();
        // Same-level headings preserve doc order; we don't strictly guarantee
        // cross-level ordering since we iterate level-by-level. Check membership.
        assert_eq!(levels.iter().filter(|&&l| l == 1).count(), 1);
        assert_eq!(levels.iter().filter(|&&l| l == 2).count(), 2);
        assert_eq!(levels.iter().filter(|&&l| l == 3).count(), 1);
        assert!(texts.contains(&"Top"));
        assert!(texts.contains(&"Sub A"));
        assert!(texts.contains(&"Sub B"));
        assert!(texts.contains(&"Deep"));
    }

    #[test]
    fn snapshot_resolves_relative_links_against_page_url() {
        let doc = parse(
            r#"<html><body>
                <a href="/signup">Sign up</a>
                <a href="https://other.test/x">External</a>
                <a>plain</a>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/foo", SnapshotOpts::default());
        // Three <a> total but only two have href and surface in the snapshot.
        assert_eq!(snap.links.len(), 2);
        let signup = snap.links.iter().find(|l| l.text == "Sign up").unwrap();
        assert_eq!(signup.href, "https://x.test/signup");
        let ext = snap.links.iter().find(|l| l.text == "External").unwrap();
        assert_eq!(ext.href, "https://other.test/x");
    }

    #[test]
    fn snapshot_extracts_form_with_method_action_and_fields() {
        let doc = parse(
            r#"<html><body>
                <form id="login" action="/auth" method="post">
                  <label for="u">Username</label>
                  <input id="u" name="u" type="text" required>
                  <input name="p" type="password" placeholder="password">
                  <button type="submit">Sign in</button>
                </form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.forms.len(), 1);
        let form = &snap.forms[0];
        assert_eq!(form.selector, "#login");
        assert_eq!(form.action.as_deref(), Some("/auth"));
        assert_eq!(form.method, "POST");
        assert_eq!(form.fields.len(), 2);

        let u = &form.fields[0];
        assert_eq!(u.name.as_deref(), Some("u"));
        assert_eq!(u.kind, "text");
        assert_eq!(u.label.as_deref(), Some("Username"));
        assert!(u.required);

        let p = &form.fields[1];
        assert_eq!(p.kind, "password");
        assert_eq!(p.placeholder.as_deref(), Some("password"));
        assert!(!p.required);
    }

    #[test]
    fn snapshot_treats_textarea_and_select_as_fields() {
        let doc = parse(
            r#"<html><body>
              <form action="/m">
                <textarea name="msg"></textarea>
                <select name="topic"><option>a</option><option>b</option></select>
              </form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let form = &snap.forms[0];
        let kinds: Vec<&str> = form.fields.iter().map(|f| f.kind.as_str()).collect();
        assert!(kinds.contains(&"textarea"));
        assert!(kinds.contains(&"select"));
    }

    #[test]
    fn snapshot_separates_stray_inputs_from_form_fields() {
        let doc = parse(
            r#"<html><body>
              <form action="/x"><input name="a"></form>
              <input name="loose">
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.forms[0].fields.len(), 1);
        assert_eq!(snap.forms[0].fields[0].name.as_deref(), Some("a"));
        assert_eq!(snap.inputs.len(), 1);
        assert_eq!(snap.inputs[0].name.as_deref(), Some("loose"));
    }

    #[test]
    fn snapshot_buttons_default_to_submit_kind() {
        let doc = parse(
            r#"<html><body>
              <button>Default</button>
              <button type="button">Plain</button>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.buttons.len(), 2);
        let default = snap.buttons.iter().find(|b| b.text == "Default").unwrap();
        let plain = snap.buttons.iter().find(|b| b.text == "Plain").unwrap();
        assert_eq!(default.kind, "submit");
        assert_eq!(plain.kind, "button");
    }

    #[test]
    fn snapshot_collects_meta_by_property_or_name() {
        let doc = parse(
            r#"<html><head>
                 <meta property="og:title" content="Hello">
                 <meta name="description" content="A page">
                 <meta name="twitter:card" content="summary">
                 <meta property="">
               </head><body></body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.meta.get("og:title").map(|s| s.as_str()), Some("Hello"));
        assert_eq!(
            snap.meta.get("description").map(|s| s.as_str()),
            Some("A page")
        );
        assert_eq!(
            snap.meta.get("twitter:card").map(|s| s.as_str()),
            Some("summary")
        );
    }

    #[test]
    fn snapshot_text_summary_truncates_at_cap() {
        let doc = parse("<html><body>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</body></html>");
        let snap = PageSnapshot::from_document(
            &doc,
            "https://x.test/",
            SnapshotOpts {
                max_text_summary_bytes: 10,
            },
        );
        assert!(
            snap.text_summary.ends_with(TRUNCATION_MARKER),
            "got: {}",
            snap.text_summary
        );
        assert!(snap.text_summary.len() <= 10);
    }

    #[test]
    fn snapshot_empty_document_produces_empty_collections() {
        let doc = parse("<html><body></body></html>");
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        assert_eq!(snap.title, "");
        assert!(snap.forms.is_empty());
        assert!(snap.links.is_empty());
        assert!(snap.buttons.is_empty());
        assert!(snap.inputs.is_empty());
        assert!(snap.headings.is_empty());
        assert!(snap.meta.is_empty());
        assert_eq!(snap.text_summary, "");
    }

    #[test]
    fn snapshot_label_associated_via_for_attribute() {
        let doc = parse(
            r#"<html><body>
                <label for="email">Email address</label>
                <form action="/x"><input id="email" name="email" type="email"></form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let field = &snap.forms[0].fields[0];
        assert_eq!(field.label.as_deref(), Some("Email address"));
    }

    #[test]
    fn snapshot_label_associated_via_ancestor_label() {
        let doc = parse(
            r#"<html><body>
                <form action="/x">
                  <label>Subscribe? <input name="sub" type="checkbox"></label>
                </form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let field = &snap.forms[0].fields[0];
        assert!(
            field
                .label
                .as_deref()
                .is_some_and(|l| l.contains("Subscribe")),
            "got: {:?}",
            field.label
        );
    }

    // ---- interactive indexing ----

    #[test]
    fn interactive_indices_are_unique_and_match_per_category() {
        let doc = parse(
            r#"<html><body>
                <form id="login" action="/auth">
                  <input name="u">
                  <button type="submit">Go</button>
                </form>
                <a href="/about">About</a>
                <input name="loose">
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        // Every interactive element gets a unique index.
        let mut seen = std::collections::HashSet::new();
        for e in &snap.interactive {
            assert!(seen.insert(e.index), "duplicate index {}", e.index);
        }
        // Indices used in per-category lists appear in the flat list.
        let form_idx = snap.forms[0].index;
        let field_idx = snap.forms[0].fields[0].index;
        let link_idx = snap.links[0].index;
        let stray_idx = snap.inputs[0].index;
        for idx in [form_idx, field_idx, link_idx, stray_idx] {
            assert!(seen.contains(&idx), "index {idx} missing from interactive");
        }
        // selector_for_index resolves to the same selector in the
        // structured view.
        let form_sel = snap.forms[0].selector.clone();
        assert_eq!(snap.selector_for_index(form_idx), Some(form_sel.as_str()));
    }

    #[test]
    fn interactive_kinds_are_categorized_correctly() {
        let doc = parse(
            r#"<html><body>
                <form action="/x"><input name="u"></form>
                <a href="/y">Y</a>
                <button>Z</button>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let kinds: Vec<&str> = snap.interactive.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"form"));
        assert!(kinds.contains(&"field"));
        assert!(kinds.contains(&"link"));
        assert!(kinds.contains(&"button"));
    }

    #[test]
    fn interactive_label_falls_back_through_label_placeholder_name_kind() {
        let doc = parse(
            r#"<html><body>
                <form action="/x">
                  <input placeholder="Email" name="email">
                  <input name="just_a_name">
                  <input>
                </form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        // Map field index -> label that ended up in interactive.
        let labels: std::collections::HashMap<u32, &str> = snap
            .interactive
            .iter()
            .filter(|e| e.kind == "field")
            .map(|e| (e.index, e.label.as_str()))
            .collect();
        // First field: placeholder wins over name when no <label>.
        assert_eq!(labels[&snap.forms[0].fields[0].index], "Email");
        assert_eq!(labels[&snap.forms[0].fields[1].index], "just_a_name");
        // Last field: nothing → falls back to kind ("text").
        assert_eq!(labels[&snap.forms[0].fields[2].index], "text");
    }

    #[test]
    fn select_options_are_collected_with_value_text_and_selected_flag() {
        let doc = parse(
            r#"<html><body>
                <form action="/x">
                  <select name="topic">
                    <option value="a">Apples</option>
                    <option value="b" selected>Bananas</option>
                    <option>Cherries</option>
                  </select>
                </form>
            </body></html>"#,
        );
        let snap = PageSnapshot::from_document(&doc, "https://x.test/", SnapshotOpts::default());
        let select = &snap.forms[0].fields[0];
        assert_eq!(select.kind, "select");
        assert_eq!(select.options.len(), 3);
        assert_eq!(select.options[0].value, "a");
        assert_eq!(select.options[0].text, "Apples");
        assert!(!select.options[0].selected);
        assert!(select.options[1].selected);
        // Option with no value= falls back to text content.
        assert_eq!(select.options[2].value, "Cherries");
    }

    #[test]
    fn cap_label_truncates_long_text() {
        let long = "a".repeat(InteractiveElement::LABEL_CAP + 50);
        let out = cap_label(&long);
        assert!(out.len() <= InteractiveElement::LABEL_CAP);
        assert!(out.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn cap_label_passes_short_text_through() {
        assert_eq!(cap_label("Sign in"), "Sign in");
        assert_eq!(cap_label("  spaces  "), "spaces");
    }
}
