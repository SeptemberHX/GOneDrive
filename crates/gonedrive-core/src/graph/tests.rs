
use super::*;

#[test]
fn resync_required_gets_its_own_variant() {
    let body = br#"{"error":{"code":"resyncRequired","message":"Resync required."}}"#;
    assert!(matches!(graph_error(410, body), Error::ResyncRequired));
    // A different 410 must not be mistaken for one.
    let body = br#"{"error":{"code":"itemNotFound","message":"gone"}}"#;
    assert!(matches!(graph_error(410, body), Error::Graph { .. }));
}

#[test]
fn unparseable_error_bodies_still_produce_an_error() {
    let err = graph_error(500, b"<html>oops</html>");
    match err {
        Error::Graph {
            status, message, ..
        } => {
            assert_eq!(status, 500);
            assert!(message.contains("oops"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn retry_after_accepts_seconds() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
    assert_eq!(retry_after(&headers), Some(Duration::from_secs(30)));
}

#[test]
fn retry_after_is_absent_when_the_header_is_missing() {
    assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
}

#[test]
fn backoff_grows_and_is_capped() {
    assert!(backoff(1) < backoff(2));
    assert!(backoff(20) <= MAX_RETRY_AFTER);
}

#[test]
fn segment_encoding_escapes_path_syntax_characters() {
    assert_eq!(urlencode_segment("a b.txt"), "a%20b.txt");
    assert_eq!(urlencode_segment("a#b"), "a%23b");
    assert_eq!(urlencode_segment("a%b"), "a%25b");
    // A name that would otherwise terminate the `items/{id}:/name:` form.
    assert_eq!(urlencode_segment("a:b"), "a%3Ab");
    // Non-ASCII stays readable as UTF-8 percent-encoding.
    assert_eq!(urlencode_segment("报告.docx"), "%E6%8A%A5%E5%91%8A.docx");
}

#[test]
fn continuation_links_cannot_redirect_the_bearer_token() {
    let graph = GraphClient::new(reqwest::Client::new(), TokenSource::Static("secret".into()))
        .with_base_url("https://graph.microsoft.com/v1.0");

    assert!(graph
        .trusted_graph_link("https://graph.microsoft.com/v1.0/me/drive/root/delta?t=1")
        .is_ok());
    assert!(graph
        .trusted_graph_link("https://attacker.invalid/collect")
        .is_err());
    assert!(graph
        .trusted_graph_link("http://graph.microsoft.com/v1.0/me/drive/root/delta")
        .is_err());
}

#[test]
fn upload_session_urls_cannot_downgrade_or_embed_credentials() {
    let graph = GraphClient::new(reqwest::Client::new(), TokenSource::Static("secret".into()))
        .with_base_url("https://graph.microsoft.com/v1.0");

    // Upload sessions are opaque and may use a different Microsoft host.
    assert!(graph
        .validated_upload_url("https://sn3302.up.1drv.com/up/session?token=opaque")
        .is_ok());
    assert!(graph
        .validated_upload_url("http://sn3302.up.1drv.com/up/session")
        .is_err());
    assert!(graph
        .validated_upload_url("https://user:pass@sn3302.up.1drv.com/up/session")
        .is_err());
    assert!(graph.validated_upload_url("file:///tmp/session").is_err());
    assert!(graph.validated_upload_url("/relative/session").is_err());
}

#[test]
fn upload_ranges_are_nonempty_bounded_and_overflow_safe() {
    assert_eq!(upload_range_end(10, 20, 5).unwrap(), 14);
    assert!(upload_range_end(0, 20, 0).is_err());
    assert!(upload_range_end(19, 20, 2).is_err());
    assert!(upload_range_end(u64::MAX, u64::MAX, 1).is_err());
}
