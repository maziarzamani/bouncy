//! Sanity tests for the `bouncy` facade.
//!
//! These tests prove the re-exports actually resolve under each Cargo
//! feature combo we ship. They don't exercise behavior — that's what the
//! sub-crates' own tests do. The point here is: if someone adds `bouncy =
//! "0.1"` to their Cargo.toml and writes the same code we promise in
//! lib.rs's doc examples, it compiles.

#[cfg(feature = "fetch")]
#[test]
fn fetch_reexports_resolve() {
    // Just take the path; if it compiles, the re-export is wired.
    let _ = std::any::type_name::<bouncy::fetch::Fetcher>();
    let _ = std::any::type_name::<bouncy::fetch::FetchRequest>();
    let _ = std::any::type_name::<bouncy::fetch::Response>();
    let _ = std::any::type_name::<bouncy::fetch::CookieJar>();
    let _ = std::any::type_name::<bouncy::fetch::Error>();
}

#[cfg(feature = "extract")]
#[test]
fn extract_reexports_resolve() {
    let _ = std::any::type_name::<bouncy::extract::Link>();
    // Functions: take a fn pointer to confirm they exist with the
    // expected signatures.
    let _: fn(&[u8]) -> Result<Option<String>, _> = bouncy::extract::extract_title;
    let _: fn(&[u8]) -> Result<String, _> = bouncy::extract::extract_text;
    let _: fn(&[u8], &url::Url) -> Result<Vec<bouncy::extract::Link>, _> =
        bouncy::extract::extract_links;
}

#[cfg(feature = "browse")]
#[test]
fn browse_reexports_resolve() {
    let _ = std::any::type_name::<bouncy::browse::BrowseSession>();
    let _ = std::any::type_name::<bouncy::browse::BrowseOpts>();
    let _ = std::any::type_name::<bouncy::browse::PageSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::FormSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::LinkSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::ButtonSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::InputSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::HeadingSnapshot>();
    let _ = std::any::type_name::<bouncy::browse::ReadMode>();
    let _ = std::any::type_name::<bouncy::browse::EvalResult>();
    let _ = std::any::type_name::<bouncy::browse::BrowseError>();
}

#[cfg(feature = "dom")]
#[test]
fn dom_reexports_resolve() {
    let _ = std::any::type_name::<bouncy::dom::Document>();
    let _ = std::any::type_name::<bouncy::dom::NodeId>();
}

#[cfg(feature = "js")]
#[test]
fn js_reexports_resolve() {
    let _ = std::any::type_name::<bouncy::js::Runtime>();
}

#[test]
fn version_constant_matches_pkg() {
    assert_eq!(bouncy::VERSION, env!("CARGO_PKG_VERSION"));
}
