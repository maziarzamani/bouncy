//! DOM tree wrapper for the JS path.
//!
//! Wraps html5ever's RcDom in a `Document` indexed by stable `NodeId`s. The
//! V8 bridge talks to this crate using NodeIds (u32) — never DOM handles
//! and never JSON-stringified ID arrays — so the FunctionTemplate
//! callbacks stay branchless on the hot path. Mutations (createElement,
//! appendChild, set attribute, set inner HTML, …) keep both the rcdom
//! tree and the NodeId table coherent.
//!
//! The static path does NOT use this crate — bouncy-extract's lol_html
//! streaming engine is faster when no mutation is required.

use std::cell::RefCell;
use std::collections::HashMap;

use html5ever::driver::ParseOpts;
use html5ever::serialize::SerializeOpts;
use html5ever::serialize::TraversalScope;
use html5ever::tendril::TendrilSink;
use html5ever::{
    ns, parse_document, parse_fragment, serialize, Attribute, LocalName, Namespace, QualName,
};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom, SerializableHandle};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("serialize error: {0}")]
    Serialize(String),

    #[error("unknown node id {0}")]
    UnknownNode(u32),
}

/// Stable identity for a DOM node, handed across the V8 bridge as an
/// integer. Indexes into `Document::nodes`. NodeIds are never reused.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const DOCUMENT: NodeId = NodeId(0);

    pub fn raw(self) -> u32 {
        self.0
    }
}

pub struct Document {
    /// rcdom keeps html5ever's idea of the tree; we own it but never hand
    /// out the `Handle` directly.
    dom: RcDom,
    /// `nodes[id.raw() as usize]` is the live `Handle` for that NodeId, or
    /// `None` once the node has been detached and forgotten.
    nodes: Vec<Option<Handle>>,
    /// Reverse lookup: pointer-equality on the underlying allocation
    /// (stable across `Rc::clone`) maps back to the original NodeId.
    by_ptr: HashMap<*const Node, NodeId>,
}

impl Document {
    /// Parse a complete HTML document and assign NodeIds to every node.
    pub fn parse(html: &str) -> Result<Self, Error> {
        let dom = parse_document(RcDom::default(), ParseOpts::default()).one(html);
        let mut doc = Document {
            dom,
            nodes: Vec::new(),
            by_ptr: HashMap::new(),
        };
        let root = doc.dom.document.clone();
        doc.intern_subtree(&root); // assigns NodeId(0) to Document
        Ok(doc)
    }

    fn intern(&mut self, h: &Handle) -> NodeId {
        let ptr = handle_ptr(h);
        if let Some(&id) = self.by_ptr.get(&ptr) {
            return id;
        }
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Some(h.clone()));
        self.by_ptr.insert(ptr, id);
        id
    }

    fn intern_subtree(&mut self, h: &Handle) {
        self.intern(h);
        for child in h.children.borrow().iter() {
            self.intern_subtree(child);
        }
    }

    fn handle(&self, id: NodeId) -> Result<&Handle, Error> {
        self.nodes
            .get(id.0 as usize)
            .and_then(|s| s.as_ref())
            .ok_or(Error::UnknownNode(id.0))
    }

    // ---- top-level navigation ------------------------------------------

    pub fn document_node(&self) -> NodeId {
        NodeId::DOCUMENT
    }

    pub fn html_root(&self) -> Option<NodeId> {
        self.find_descendant(NodeId::DOCUMENT, "html").ok()?
    }

    pub fn head(&self) -> Option<NodeId> {
        self.find_descendant(NodeId::DOCUMENT, "head").ok()?
    }

    pub fn body(&self) -> Option<NodeId> {
        self.find_descendant(NodeId::DOCUMENT, "body").ok()?
    }

    fn find_descendant(&self, root: NodeId, tag: &str) -> Result<Option<NodeId>, Error> {
        let h = self.handle(root)?.clone();
        Ok(self.walk_first(&h, tag))
    }

    fn walk_first(&self, h: &Handle, tag: &str) -> Option<NodeId> {
        if let NodeData::Element { name, .. } = &h.data {
            if name.local.as_ref() == tag {
                return self.by_ptr.get(&handle_ptr(h)).copied();
            }
        }
        for c in h.children.borrow().iter() {
            if let Some(id) = self.walk_first(c, tag) {
                return Some(id);
            }
        }
        None
    }

    // ---- read accessors ------------------------------------------------

    pub fn tag_name(&self, id: NodeId) -> Option<String> {
        let h = self.handle(id).ok()?;
        match &h.data {
            NodeData::Element { name, .. } => Some(name.local.as_ref().to_string()),
            _ => None,
        }
    }

    pub fn get_attribute(&self, id: NodeId, name: &str) -> Option<String> {
        let h = self.handle(id).ok()?;
        if let NodeData::Element { attrs, .. } = &h.data {
            for a in attrs.borrow().iter() {
                if a.name.local.as_ref() == name {
                    return Some(a.value.to_string());
                }
            }
        }
        None
    }

    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let Ok(h) = self.handle(id) else {
            return Vec::new();
        };
        h.children
            .borrow()
            .iter()
            .filter_map(|c| self.by_ptr.get(&handle_ptr(c)).copied())
            .collect()
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let h = self.handle(id).ok()?;
        let parent_weak = h.parent.take();
        let result = parent_weak
            .as_ref()
            .and_then(|w| w.upgrade())
            .and_then(|p| self.by_ptr.get(&handle_ptr(&p)).copied());
        h.parent.set(parent_weak);
        result
    }

    pub fn text_content(&self, id: NodeId) -> String {
        let Ok(h) = self.handle(id) else {
            return String::new();
        };
        let mut out = String::new();
        collect_text(h, &mut out);
        out
    }

    /// Like `text_content` but does NOT skip `<script>` / `<style>` /
    /// `<noscript>` / `<template>`. Used for fetching the source code of
    /// a `<script>` element to feed back into V8 — that path actively
    /// wants the script body.
    pub fn raw_text_content(&self, id: NodeId) -> String {
        let Ok(h) = self.handle(id) else {
            return String::new();
        };
        let mut out = String::new();
        collect_text_raw(h, &mut out);
        out
    }

    /// Returns the first descendant element whose `id` attribute matches.
    pub fn get_element_by_id(&self, target: &str) -> Option<NodeId> {
        let root = self.handle(NodeId::DOCUMENT).ok()?.clone();
        self.find_id(&root, target)
    }

    /// Find the first element matching a SIMPLE CSS selector. Supports:
    ///   `tag`            — element name match
    ///   `#id`            — element with `id` attribute match
    ///   `.class`         — element with `class` containing token
    ///   `[attr]`         — element with attribute present
    ///   `[attr=value]`   — quoted or unquoted value match
    /// Combinators / pseudo-classes / multi-clause selectors are NOT
    /// supported — the recipe says polyfill only what fixtures need; we
    /// graduate to the `selectors` crate when something asks for more.
    pub fn query_selector(&self, selector: &str) -> Option<NodeId> {
        self.query_selector_within(NodeId::DOCUMENT, selector)
    }

    /// All descendants matching `selector` (same simple grammar as
    /// `query_selector`).
    pub fn query_selector_all(&self, selector: &str) -> Vec<NodeId> {
        self.query_selector_all_within(NodeId::DOCUMENT, selector)
    }

    /// Like `query_selector` but starts the descendant walk from `root`
    /// instead of the document. Used by `Element.prototype.querySelector`
    /// and `ShadowRoot.querySelector`.
    pub fn query_selector_within(&self, root: NodeId, selector: &str) -> Option<NodeId> {
        let pat = SimpleSelector::parse(selector)?;
        let h = self.handle(root).ok()?.clone();
        let mut out = Vec::with_capacity(1);
        self.collect_matches(&h, &pat, &mut out, true);
        out.into_iter().next()
    }

    /// Like `query_selector_all` but starts from `root`.
    pub fn query_selector_all_within(&self, root: NodeId, selector: &str) -> Vec<NodeId> {
        let Some(pat) = SimpleSelector::parse(selector) else {
            return Vec::new();
        };
        let Ok(h) = self.handle(root) else {
            return Vec::new();
        };
        let h = h.clone();
        let mut out = Vec::new();
        self.collect_matches(&h, &pat, &mut out, false);
        out
    }

    fn collect_matches(
        &self,
        h: &Handle,
        pat: &SimpleSelector,
        out: &mut Vec<NodeId>,
        first_only: bool,
    ) {
        if first_only && !out.is_empty() {
            return;
        }
        if pat.matches(h) {
            if let Some(&id) = self.by_ptr.get(&handle_ptr(h)) {
                out.push(id);
                if first_only {
                    return;
                }
            }
        }
        for c in h.children.borrow().iter() {
            self.collect_matches(c, pat, out, first_only);
            if first_only && !out.is_empty() {
                return;
            }
        }
    }

    fn find_id(&self, h: &Handle, target: &str) -> Option<NodeId> {
        if let NodeData::Element { attrs, .. } = &h.data {
            for a in attrs.borrow().iter() {
                if a.name.local.as_ref() == "id" && a.value.as_ref() == target {
                    return self.by_ptr.get(&handle_ptr(h)).copied();
                }
            }
        }
        for c in h.children.borrow().iter() {
            if let Some(id) = self.find_id(c, target) {
                return Some(id);
            }
        }
        None
    }

    // ---- mutation ------------------------------------------------------

    pub fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> Result<(), Error> {
        let h = self.handle(id)?.clone();
        if let NodeData::Element { attrs, .. } = &h.data {
            let mut a = attrs.borrow_mut();
            for at in a.iter_mut() {
                if at.name.local.as_ref() == name {
                    at.value = value.into();
                    return Ok(());
                }
            }
            a.push(Attribute {
                name: QualName::new(None, ns!(), LocalName::from(name)),
                value: value.into(),
            });
        }
        Ok(())
    }

    pub fn remove_attribute(&mut self, id: NodeId, name: &str) -> Result<(), Error> {
        let h = self.handle(id)?.clone();
        if let NodeData::Element { attrs, .. } = &h.data {
            attrs.borrow_mut().retain(|a| a.name.local.as_ref() != name);
        }
        Ok(())
    }

    pub fn set_text_content(&mut self, id: NodeId, text: &str) -> Result<(), Error> {
        // textContent setter: drop all children, replace with one text node.
        let h = self.handle(id)?.clone();
        // Forget the existing subtree NodeIds so we don't leak entries.
        for c in h.children.borrow().iter() {
            self.forget_subtree(c);
        }
        h.children.borrow_mut().clear();

        let text_node = Node::new(NodeData::Text {
            contents: RefCell::new(text.into()),
        });
        text_node.parent.set(Some(std::rc::Rc::downgrade(&h)));
        self.intern(&text_node);
        h.children.borrow_mut().push(text_node);
        Ok(())
    }

    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let qname = QualName::new(None, ns!(html), LocalName::from(tag));
        let node = Node::new(NodeData::Element {
            name: qname,
            attrs: RefCell::new(Vec::new()),
            template_contents: RefCell::new(None),
            mathml_annotation_xml_integration_point: false,
        });
        self.intern(&node)
    }

    pub fn create_text_node(&mut self, text: &str) -> NodeId {
        let node = Node::new(NodeData::Text {
            contents: RefCell::new(text.into()),
        });
        self.intern(&node)
    }

    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), Error> {
        let p = self.handle(parent)?.clone();
        let c = self.handle(child)?.clone();
        // Detach from old parent first.
        self.detach(&c);
        c.parent.set(Some(std::rc::Rc::downgrade(&p)));
        p.children.borrow_mut().push(c);
        Ok(())
    }

    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), Error> {
        let p = self.handle(parent)?.clone();
        let c = self.handle(child)?.clone();
        let cptr = handle_ptr(&c);
        p.children.borrow_mut().retain(|h| handle_ptr(h) != cptr);
        c.parent.set(None);
        Ok(())
    }

    fn detach(&self, c: &Handle) {
        let parent_weak = c.parent.take();
        if let Some(parent) = parent_weak.and_then(|w| w.upgrade()) {
            let cptr = handle_ptr(c);
            parent
                .children
                .borrow_mut()
                .retain(|h| handle_ptr(h) != cptr);
        }
        c.parent.set(None);
    }

    fn forget_subtree(&mut self, h: &Handle) {
        if let Some(id) = self.by_ptr.remove(&handle_ptr(h)) {
            if let Some(slot) = self.nodes.get_mut(id.0 as usize) {
                *slot = None;
            }
        }
        for child in h.children.borrow().iter() {
            self.forget_subtree(child);
        }
    }

    pub fn inner_html(&self, id: NodeId) -> String {
        let Ok(h) = self.handle(id) else {
            return String::new();
        };
        let mut buf = Vec::new();
        for child in h.children.borrow().iter() {
            let serializable: SerializableHandle = child.clone().into();
            let _ = serialize(&mut buf, &serializable, SerializeOpts::default());
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn outer_html(&self, id: NodeId) -> String {
        let Ok(h) = self.handle(id) else {
            return String::new();
        };
        let serializable: SerializableHandle = h.clone().into();
        let mut buf = Vec::new();
        let opts = SerializeOpts {
            traversal_scope: TraversalScope::IncludeNode,
            ..SerializeOpts::default()
        };
        let _ = serialize(&mut buf, &serializable, opts);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Replace `id`'s children by parsing `html` as a fragment in the
    /// element's context.
    pub fn set_inner_html(&mut self, id: NodeId, html: &str) -> Result<(), Error> {
        let h = self.handle(id)?.clone();
        let context_local = match &h.data {
            NodeData::Element { name, .. } => name.local.clone(),
            _ => LocalName::from("body"),
        };

        // Drop existing subtree's NodeId table entries.
        for c in h.children.borrow().iter() {
            self.forget_subtree(c);
        }
        h.children.borrow_mut().clear();

        // parse_fragment puts the fragment under an internal context
        // element, then re-parents it to <html>. The shape of the
        // resulting RcDom is: document → <html> → (fragment children).
        let context_qname = QualName::new(None, ns!(html), context_local);
        let frag_dom = parse_fragment(
            RcDom::default(),
            ParseOpts::default(),
            context_qname,
            Vec::new(),
            false,
        )
        .one(html);

        // CRITICAL: rcdom's `Node` Drop impl walks descendants and drains
        // their `children` Vec to avoid deep-recursion stack overflow. If we
        // leave the fragment children in `frag_dom`, the moment `frag_dom`
        // drops it walks *into* every node we just took, mem::take()ing
        // their children — even though we hold strong Rcs to them
        // elsewhere. Move the kids out of the html wrapper before that
        // can happen.
        let new_children: Vec<Handle> = {
            let document_children = frag_dom.document.children.borrow();
            match document_children.first() {
                Some(html_root) => std::mem::take(&mut *html_root.children.borrow_mut()),
                None => Vec::new(),
            }
        };
        for child in new_children {
            self.intern_subtree(&child);
            child.parent.set(Some(std::rc::Rc::downgrade(&h)));
            h.children.borrow_mut().push(child);
        }
        Ok(())
    }

    // ---- legacy API (kept for bouncy-extract compat) --------------------

    pub fn serialize(&self) -> String {
        let handle: SerializableHandle = self.dom.document.clone().into();
        let mut buf = Vec::new();
        let _ = serialize(&mut buf, &handle, SerializeOpts::default());
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn title(&self) -> Option<String> {
        let id = self.find_descendant(NodeId::DOCUMENT, "title").ok()??;
        Some(self.text_content(id).trim().to_string())
    }

    pub fn body_text(&self) -> String {
        match self.body() {
            Some(id) => {
                let h = self.handle(id).unwrap().clone();
                let mut out = String::new();
                collect_text(&h, &mut out);
                out
            }
            None => String::new(),
        }
    }
}

// -------- selector parser/matcher ---------------------------------------

#[derive(Debug)]
enum SimpleSelector {
    Tag(String),
    Id(String),
    Class(String),
    Attr { name: String, value: Option<String> },
}

impl SimpleSelector {
    fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(rest) = s.strip_prefix('#') {
            return Some(SimpleSelector::Id(rest.to_string()));
        }
        if let Some(rest) = s.strip_prefix('.') {
            return Some(SimpleSelector::Class(rest.to_string()));
        }
        if let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            return match inner.find('=') {
                Some(eq) => {
                    let name = inner[..eq].trim().to_string();
                    let mut val = inner[eq + 1..].trim();
                    if (val.starts_with('"') && val.ends_with('"'))
                        || (val.starts_with('\'') && val.ends_with('\''))
                    {
                        val = &val[1..val.len() - 1];
                    }
                    Some(SimpleSelector::Attr {
                        name,
                        value: Some(val.to_string()),
                    })
                }
                None => Some(SimpleSelector::Attr {
                    name: inner.trim().to_string(),
                    value: None,
                }),
            };
        }
        // bare tag name
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(SimpleSelector::Tag(s.to_ascii_lowercase()));
        }
        None
    }

    fn matches(&self, h: &Handle) -> bool {
        let NodeData::Element { name, attrs, .. } = &h.data else {
            return false;
        };
        let local = name.local.as_ref();
        let attrs = attrs.borrow();
        match self {
            SimpleSelector::Tag(t) => local.eq_ignore_ascii_case(t),
            SimpleSelector::Id(id) => attrs
                .iter()
                .any(|a| a.name.local.as_ref() == "id" && a.value.as_ref() == id.as_str()),
            SimpleSelector::Class(c) => attrs.iter().any(|a| {
                a.name.local.as_ref() == "class"
                    && a.value
                        .split_ascii_whitespace()
                        .any(|tok| tok == c.as_str())
            }),
            SimpleSelector::Attr { name, value } => attrs.iter().any(|a| {
                a.name.local.as_ref() == name
                    && match value {
                        Some(v) => a.value.as_ref() == v.as_str(),
                        None => true,
                    }
            }),
        }
    }
}

// -------- helpers -------------------------------------------------------

fn handle_ptr(h: &Handle) -> *const Node {
    std::rc::Rc::as_ptr(h)
}

fn collect_text(node: &Handle, out: &mut String) {
    if let NodeData::Element { name, .. } = &node.data {
        let n = name.local.as_ref();
        if matches!(n, "script" | "style" | "noscript" | "template") {
            return;
        }
    }
    if let NodeData::Text { contents } = &node.data {
        out.push_str(&contents.borrow());
    }
    for child in node.children.borrow().iter() {
        collect_text(child, out);
    }
}

fn collect_text_raw(node: &Handle, out: &mut String) {
    if let NodeData::Text { contents } = &node.data {
        out.push_str(&contents.borrow());
    }
    for child in node.children.borrow().iter() {
        collect_text_raw(child, out);
    }
}

#[allow(dead_code)]
fn _ns_check() {
    // Compile-time sanity that the ns macro works for the html namespace.
    let _: Namespace = ns!(html);
}
