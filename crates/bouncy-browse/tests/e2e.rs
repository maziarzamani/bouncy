//! Real-network E2E smokes for bouncy-browse. Hits public endpoints
//! (example.com, httpbin.org), so gated behind the `e2e` Cargo feature
//! to keep `cargo test` runnable offline.
//!
//! Run with: `cargo test -p bouncy-browse --features e2e --test e2e`.

#![cfg(feature = "e2e")]

use bouncy_browse::{BrowseOpts, BrowseSession, ReadMode};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_example_com_returns_expected_h1_and_link() {
    let (session, snap) = BrowseSession::open("https://example.com", BrowseOpts::default())
        .await
        .expect("open example.com");
    assert_eq!(snap.title, "Example Domain");
    assert!(
        snap.headings.iter().any(|h| h.text == "Example Domain"),
        "expected Example Domain heading, got: {:?}",
        snap.headings
    );
    // example.com has a single link to iana.org.
    assert!(
        snap.links.iter().any(|l| l.href.contains("iana.org")),
        "expected iana.org link, got: {:?}",
        snap.links
    );
    // Read it back via the read API for parity coverage.
    let h1s = session.read("h1", ReadMode::Text).await.unwrap();
    assert_eq!(h1s, vec!["Example Domain"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_agent_override_visible_to_httpbin() {
    let opts = BrowseOpts {
        user_agent: Some("bouncy-browse-e2e/1.0".to_string()),
        ..BrowseOpts::default()
    };
    let (_session, snap) = BrowseSession::open("https://httpbin.org/user-agent", opts)
        .await
        .expect("open httpbin");
    // httpbin returns a JSON page where the body text echoes the UA.
    assert!(
        snap.text_summary.contains("bouncy-browse-e2e/1.0"),
        "expected custom UA in httpbin response, got: {}",
        snap.text_summary
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_real_form_against_httpbin_posts_field_values() {
    // httpbin.org/forms/post serves a real HTML form that POSTs to /post,
    // which echoes the request back as JSON. End-to-end: open → fill →
    // submit → read the JSON for our values.
    let (session, _) = BrowseSession::open("https://httpbin.org/forms/post", BrowseOpts::default())
        .await
        .expect("open httpbin form");
    session
        .fill("[name=custname]", "Maziar")
        .await
        .expect("fill custname");
    session
        .fill("[name=custtel]", "555-0100")
        .await
        .expect("fill custtel");
    let snap = session.submit("form").await.expect("submit form");
    // /post echoes JSON; the snapshot's text_summary will contain it.
    assert!(
        snap.text_summary.contains("Maziar"),
        "expected POSTed custname to appear in /post echo, got: {}",
        snap.text_summary
    );
    assert!(
        snap.text_summary.contains("555-0100"),
        "expected POSTed custtel to appear in /post echo, got: {}",
        snap.text_summary
    );
}
