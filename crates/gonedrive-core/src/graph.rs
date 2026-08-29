//! Microsoft Graph client.
//!
//! Every request goes through [`GraphClient::send`], which owns the three
//! failure modes that matter for a sync client and that callers should never
//! have to reimplement:
//!
//! * **401** — the token died early (revocation, password change, conditional
//!   access). Force a refresh and retry once.
//! * **429 / 503 / 504** — throttling. Honour `Retry-After`; the service is
//!   explicit that ignoring it makes throttling worse.
//! * **410 `resyncRequired`** — the stored deltaLink is dead, and the sync
//!   engine has to restart enumeration rather than treat it as an error.
//!
//! Items are addressed by id, never by path. Path addressing needs a colon
//! syntax (`/root:/a/b:`) that is fragile around names containing `:`, `#` or
//! `%`, and ids are stable across renames anyway.

use std::time::Duration;

use bytes::Bytes;
use serde::Serialize;
use time::OffsetDateTime;

use crate::config::GRAPH_ENDPOINT;
use crate::credentials::TokenSource;
use crate::error::{Error, Result};
use crate::models::*;

/// Attempts per request before giving up, including throttled retries.
const MAX_ATTEMPTS: u32 = 5;

/// Cap on how long we will obey a `Retry-After`. Beyond this the caller should
/// get the error back and reschedule the whole operation instead of parking a
/// task for minutes.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Items per delta page. Fewer round trips on large drives; the service caps
/// what it will actually return.
const DELTA_PAGE_SIZE: u32 = 1000;

pub struct GraphClient {
    http: reqwest::Client,
    auth: TokenSource,
    /// Overridable so the engine can be tested against a local fake.
    base: String,
}

/// What happened to one chunk of a resumable upload.
#[derive(Debug)]
pub enum ChunkOutcome {
    /// The service accepted the chunk and wants more, starting at this offset.
    Accepted { next_offset: Option<u64> },
    /// The final chunk landed; the upload is done.
    Completed(Box<DriveItem>),
}

impl GraphClient {
    pub fn new(http: reqwest::Client, auth: impl Into<TokenSource>) -> Self {
        Self {
            http,
            auth: auth.into(),
            base: GRAPH_ENDPOINT.to_string(),
        }
    }

    /// Point the client at a different Graph endpoint.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    // ---- identity and drive metadata -------------------------------------

    pub async fn me(&self) -> Result<User> {
        let base = &self.base;
        let response = self
            .send(|http| http.get(format!("{base}/me")))
            .await?;
        Ok(response.json().await?)
    }

    pub async fn drive(&self) -> Result<Drive> {
        let base = &self.base;
        let response = self
            .send(|http| http.get(format!("{base}/me/drive")))
            .await?;
        Ok(response.json().await?)
    }

    // ---- change enumeration ----------------------------------------------

    /// Fetch one delta page.
    ///
    /// `link` is either `None` (start a fresh enumeration) or a `nextLink` /
    /// `deltaLink` returned previously. Those links are passed back **verbatim**
    /// — the service documents them as opaque, and hand-rebuilding a URL from
    /// an extracted token is the usual cause of spurious 410s.
    pub async fn delta(&self, link: Option<&str>) -> Result<DeltaPage> {
        let base = &self.base;
        let url = match link {
            Some(link) => link.to_string(),
            None => format!("{base}/me/drive/root/delta?$top={DELTA_PAGE_SIZE}"),
        };
        let response = self.send(|http| http.get(&url)).await?;
        Ok(response.json().await?)
    }

    /// Walk every page of a delta enumeration, returning all items plus the
    /// deltaLink to persist for next time.
    pub async fn delta_all(&self, link: Option<&str>) -> Result<(Vec<DriveItem>, Option<String>)> {
        let mut items = Vec::new();
        let mut next = link.map(str::to_string);
        let mut first = true;

        loop {
            let page = self
                .delta(if first { link } else { next.as_deref() })
                .await?;
            first = false;
            items.extend(page.value);

            match (page.next_link, page.delta_link) {
                (Some(next_link), _) => next = Some(next_link),
                (None, delta_link) => return Ok((items, delta_link)),
            }
        }
    }

    /// The drive's root item, which anchors every path we derive.
    pub async fn root(&self) -> Result<DriveItem> {
        let base = &self.base;
        let response = self
            .send(|http| http.get(format!("{base}/me/drive/root")))
            .await?;
        Ok(response.json().await?)
    }

    pub async fn item(&self, item_id: &str) -> Result<DriveItem> {
        let base = &self.base;
        let response = self
            .send(|http| http.get(format!("{base}/me/drive/items/{item_id}")))
            .await?;
        Ok(response.json().await?)
    }

    // ---- download ---------------------------------------------------------

    /// Open a streaming read of an item's content.
    ///
    /// `range` requests a byte range, which is what on-demand hydration uses to
    /// fill only the part of a file that was actually touched.
    pub async fn download(
        &self,
        item_id: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/me/drive/items/{item_id}/content", self.base);
        self.send(move |http| {
            let request = http.get(&url);
            match &range {
                // HTTP ranges are inclusive at both ends.
                Some(r) => request.header("Range", format!("bytes={}-{}", r.start, r.end - 1)),
                None => request,
            }
        })
        .await
    }

    /// Download from `offset` to the end, resuming an interrupted transfer.
    ///
    /// `if_range` (an eTag) makes the resume safe: the service replies `206`
    /// and streams only the bytes from `offset` on *if the content still
    /// matches*, or `200` with the whole file if it changed — so a stale
    /// partial is transparently restarted rather than corrupted. `offset == 0`
    /// is an ordinary full download.
    pub async fn download_from(
        &self,
        item_id: &str,
        offset: u64,
        if_range: Option<&str>,
    ) -> Result<reqwest::Response> {
        if offset == 0 {
            return self.download(item_id, None).await;
        }
        let url = format!("{}/me/drive/items/{item_id}/content", self.base);
        let if_range = if_range.map(str::to_owned);
        self.send(move |http| {
            let mut request = http.get(&url).header("Range", format!("bytes={offset}-"));
            if let Some(etag) = &if_range {
                request = request.header("If-Range", etag.clone());
            }
            request
        })
        .await
    }

    // ---- mutations --------------------------------------------------------

    pub async fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveItem> {
        #[derive(Serialize)]
        struct NewFolder<'a> {
            name: &'a str,
            folder: EmptyFacet,
            #[serde(rename = "@microsoft.graph.conflictBehavior")]
            conflict_behavior: &'a str,
        }
        #[derive(Serialize)]
        struct EmptyFacet {}

        let body = NewFolder {
            name,
            folder: EmptyFacet {},
            // A folder that already exists is not an error worth failing on;
            // two clients racing to create the same directory is normal.
            conflict_behavior: "replace",
        };
        let base = &self.base;
        let response = self
            .send(|http| {
                http.post(format!(
                    "{base}/me/drive/items/{parent_id}/children"
                ))
                .json(&body)
            })
            .await?;
        Ok(response.json().await?)
    }

    /// Upload a whole file in one request.
    ///
    /// Only valid below 4 MiB; larger files must use an upload session.
    ///
    /// `item_id` is the crux: replacing the content of a file we already track
    /// addresses it *by id*, which is unambiguous. Addressing by parent+name
    /// instead forces a choice of conflict behaviour, and `fail` — the right
    /// answer for a genuinely new file — makes every edit to an existing file
    /// come back as `409 nameAlreadyExists`.
    pub async fn upload_simple(
        &self,
        parent_id: &str,
        name: &str,
        item_id: Option<&str>,
        content: Bytes,
        mtime: Option<OffsetDateTime>,
        if_match: Option<&str>,
    ) -> Result<DriveItem> {
        let url = match item_id {
            Some(id) => format!("{}/me/drive/items/{id}/content", self.base),
            None => {
                let encoded = urlencode_segment(name);
                format!(
                    "{}/me/drive/items/{parent_id}:/{encoded}:/content\
                     ?@microsoft.graph.conflictBehavior=fail",
                    self.base
                )
            }
        };
        // Replacing a known item is conditional on its etag: if the remote
        // changed since our last pull, the service returns 412 and we treat it
        // as a conflict rather than overwriting an edit we never saw.
        let if_match = if_match.map(|s| s.to_string());
        let response = self
            .send(move |http| {
                let mut req = http
                    .put(&url)
                    .header("Content-Type", "application/octet-stream")
                    .body(content.clone());
                if let Some(tag) = &if_match {
                    req = req.header("If-Match", tag);
                }
                req
            })
            .await?;
        let item: DriveItem = response.json().await?;

        // The content upload cannot carry fileSystemInfo, so the mtime is
        // applied as a follow-up PATCH. Without this every uploaded file comes
        // back stamped "now" and looks remotely-modified on the next sync.
        match mtime {
            Some(mtime) => self.set_mtime(&item.id, mtime).await,
            None => Ok(item),
        }
    }

    /// Begin a resumable upload.
    ///
    /// As with [`Self::upload_simple`], a file we already track is addressed by
    /// id so that replacing its content needs no conflict negotiation.
    pub async fn create_upload_session(
        &self,
        parent_id: &str,
        name: &str,
        item_id: Option<&str>,
        mtime: Option<OffsetDateTime>,
        if_match: Option<&str>,
    ) -> Result<UploadSession> {
        #[derive(Serialize)]
        struct Request<'a> {
            item: Item<'a>,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Item<'a> {
            #[serde(rename = "@microsoft.graph.conflictBehavior")]
            conflict_behavior: &'a str,
            name: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            file_system_info: Option<FileSystemInfo>,
        }

        let body = Request {
            item: Item {
                // `fail` for a new name: the sync engine decides what a
                // collision means, rather than letting the service silently
                // overwrite a version the user has not seen. When uploading to
                // a known id there is no name to collide with.
                conflict_behavior: if item_id.is_some() { "replace" } else { "fail" },
                name,
                file_system_info: mtime.map(|t| FileSystemInfo {
                    created_date_time: None,
                    last_modified_date_time: Some(t),
                }),
            },
        };

        let url = match item_id {
            Some(id) => format!("{}/me/drive/items/{id}/createUploadSession", self.base),
            None => {
                let encoded = urlencode_segment(name);
                format!(
                    "{}/me/drive/items/{parent_id}:/{encoded}:/createUploadSession",
                    self.base
                )
            }
        };
        let if_match = if_match.map(|s| s.to_string());
        let response = self
            .send(move |http| {
                let mut req = http.post(&url).json(&body);
                if let Some(tag) = &if_match {
                    req = req.header("If-Match", tag);
                }
                req
            })
            .await?;
        Ok(response.json().await?)
    }

    /// Send one chunk to an upload session.
    ///
    /// The session URL is pre-authenticated, so this deliberately does **not**
    /// attach the bearer token — Graph documents that doing so can fail the
    /// request. It also bypasses [`Self::send`] for the same reason.
    pub async fn upload_chunk(
        &self,
        upload_url: &str,
        offset: u64,
        total_size: u64,
        chunk: Bytes,
    ) -> Result<ChunkOutcome> {
        let end = offset + chunk.len() as u64 - 1;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self
                .http
                .put(upload_url)
                .header("Content-Length", chunk.len().to_string())
                .header(
                    "Content-Range",
                    format!("bytes {offset}-{end}/{total_size}"),
                )
                .body(chunk.clone())
                .send()
                .await;

            let response = match response {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(backoff(attempt)).await;
                    tracing::debug!(attempt, error = %e, "retrying upload chunk");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            let status = response.status().as_u16();
            match status {
                // 202: more chunks wanted. 200/201: the file is complete.
                202 => {
                    let session: UploadSession = response.json().await?;
                    return Ok(ChunkOutcome::Accepted {
                        next_offset: session.next_offset(),
                    });
                }
                200 | 201 => {
                    let item: DriveItem = response.json().await?;
                    return Ok(ChunkOutcome::Completed(Box::new(item)));
                }
                404 | 409 | 410 => {
                    // The session is gone; the caller must start a new one
                    // rather than keep pushing bytes into a dead URL.
                    return Err(Error::UploadSessionExpired {
                        item: upload_url.to_string(),
                        reason: format!("the service returned {status}"),
                    });
                }
                429 | 500..=599 if attempt < MAX_ATTEMPTS => {
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(delay.min(MAX_RETRY_AFTER)).await;
                }
                _ => return Err(graph_error(status, &response.bytes().await?)),
            }
        }
    }

    /// Cancel a resumable upload, releasing the service-side staging area.
    pub async fn cancel_upload_session(&self, upload_url: &str) -> Result<()> {
        // Best-effort: a session we cannot cancel expires on its own.
        let _ = self.http.delete(upload_url).send().await;
        Ok(())
    }

    pub async fn delete(&self, item_id: &str) -> Result<()> {
        let base = &self.base;
        self.send(|http| http.delete(format!("{base}/me/drive/items/{item_id}")))
            .await?;
        Ok(())
    }

    /// Rename and/or move an item.
    pub async fn move_item(
        &self,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
    ) -> Result<DriveItem> {
        #[derive(Serialize)]
        struct Patch<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
            #[serde(rename = "parentReference", skip_serializing_if = "Option::is_none")]
            parent_reference: Option<Parent<'a>>,
        }
        #[derive(Serialize)]
        struct Parent<'a> {
            id: &'a str,
        }

        let body = Patch {
            name: new_name,
            parent_reference: new_parent_id.map(|id| Parent { id }),
        };
        let base = &self.base;
        let response = self
            .send(|http| {
                http.patch(format!("{base}/me/drive/items/{item_id}"))
                    .json(&body)
            })
            .await?;
        Ok(response.json().await?)
    }

    /// Stamp an item's client-side modification time.
    pub async fn set_mtime(&self, item_id: &str, mtime: OffsetDateTime) -> Result<DriveItem> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Patch {
            file_system_info: FileSystemInfo,
        }

        let body = Patch {
            file_system_info: FileSystemInfo {
                created_date_time: None,
                last_modified_date_time: Some(mtime),
            },
        };
        let base = &self.base;
        let response = self
            .send(|http| {
                http.patch(format!("{base}/me/drive/items/{item_id}"))
                    .json(&body)
            })
            .await?;
        Ok(response.json().await?)
    }

    // ---- request plumbing -------------------------------------------------

    /// Send an authenticated request, retrying the failures that are worth
    /// retrying. `build` is called once per attempt so each retry gets a fresh
    /// request with a fresh token.
    async fn send<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0;
        let mut reauthenticated = false;

        loop {
            attempt += 1;
            let token = self.auth.access_token().await?;
            let result = build(&self.http).bearer_auth(&token).send().await;

            let response = match result {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS && is_transient_transport(&e) => {
                    tracing::debug!(attempt, error = %e, "retrying request");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            if response.status().is_success() {
                return Ok(response);
            }

            let status = response.status().as_u16();
            match status {
                401 if !reauthenticated => {
                    // Retried exactly once: a second 401 after a fresh token
                    // means the grant itself is gone, and looping would just
                    // hammer the identity platform.
                    reauthenticated = true;
                    self.auth.invalidate().await;
                    continue;
                }
                429 | 503 | 504 if attempt < MAX_ATTEMPTS => {
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    if delay > MAX_RETRY_AFTER {
                        return Err(Error::Throttled(delay));
                    }
                    tracing::info!(?delay, status, "throttled; backing off");
                    tokio::time::sleep(delay).await;
                }
                500..=599 if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                _ => {
                    let body = response.bytes().await.unwrap_or_default();
                    return Err(graph_error(status, &body));
                }
            }
        }
    }
}

/// Turn a failed response body into a typed error.
fn graph_error(status: u16, body: &[u8]) -> Error {
    let envelope: Option<ErrorEnvelope> = serde_json::from_slice(body).ok();
    let (code, message) = match envelope {
        Some(e) => (e.error.code, e.error.message),
        None => (
            String::new(),
            String::from_utf8_lossy(body).chars().take(500).collect(),
        ),
    };

    // The sync engine branches on this one, so it gets its own variant rather
    // than being buried in a 410.
    if status == 410 && code.eq_ignore_ascii_case("resyncRequired") {
        return Error::ResyncRequired;
    }
    if status == 401 {
        return Error::AuthRequired(if message.is_empty() { code } else { message });
    }
    Error::Graph {
        status,
        code,
        message,
    }
}

/// Parse `Retry-After`, which may be either a delay in seconds or an HTTP date.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date form: compute the remaining delay, clamped at zero.
    let when = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822).ok()?;
    let delta = when - OffsetDateTime::now_utc();
    (delta.is_positive()).then(|| Duration::from_secs(delta.whole_seconds() as u64))
}

/// Exponential backoff: 1s, 2s, 4s, 8s, capped.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6)).min(MAX_RETRY_AFTER)
}

fn is_transient_transport(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// Percent-encode a single path segment for the `items/{id}:/{name}:` syntax.
fn urlencode_segment(name: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    // On top of the standard path set, `:` and `#` must go: `:` terminates the
    // path-addressing syntax and `#` would start a fragment.
    const SEGMENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'<')
        .add(b'>')
        .add(b'`')
        .add(b'#')
        .add(b'?')
        .add(b'{')
        .add(b'}')
        .add(b'/')
        .add(b'%')
        .add(b':');
    utf8_percent_encode(name, SEGMENT).to_string()
}

#[cfg(test)]
mod tests {
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
}
