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
use url::Url;

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
/// Direct children per request. Graph may return fewer and provide a nextLink;
/// callers must always follow the server's continuation rather than assuming
/// this requested size was honoured.
const CHILDREN_PAGE_SIZE: u32 = 1000;

pub struct GraphClient {
    http: reqwest::Client,
    auth: TokenSource,
    /// Overridable so the engine can be tested against a local fake.
    base: String,
    /// Set when the account this client belongs to has been retired. Requests
    /// already in flight finish; no new one starts. This is what stops a
    /// signed-out account from refreshing its token — and so re-creating the
    /// keyring entry that signing out just deleted.
    stopped: std::sync::atomic::AtomicBool,
}

/// What happened to one chunk of a resumable upload.
#[derive(Debug)]
pub enum ChunkOutcome {
    /// The service accepted the chunk and wants more, starting at this offset.
    Accepted {
        next_offset: Option<u64>,
        expires_at: Option<OffsetDateTime>,
    },
    /// The final chunk landed; the upload is done.
    Completed(Box<DriveItem>),
    /// Graph acknowledged the final fragment but its driveItem response could
    /// not be decoded. The caller must verify the item independently before it
    /// marks the upload complete or sends any more bytes.
    CompletionUnknown { reason: String },
}

impl GraphClient {
    pub fn new(http: reqwest::Client, auth: impl Into<TokenSource>) -> Self {
        Self {
            http,
            auth: auth.into(),
            base: GRAPH_ENDPOINT.to_string(),
            stopped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Refuse every further request. Used when an account is signed out or
    /// replaced while work is still in flight.
    pub fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn ensure_running(&self) -> Result<()> {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        Ok(())
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
        let response = self.send(|http| http.get(format!("{base}/me"))).await?;
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
            Some(link) => self.trusted_graph_link(link)?,
            None => format!("{base}/me/drive/root/delta?$top={DELTA_PAGE_SIZE}"),
        };
        let response = self.send(|http| http.get(&url)).await?;
        Ok(response.json().await?)
    }

    /// Establish a change cursor at the service's current point without walking
    /// every historical item first. Files-on-demand combines this cursor with
    /// direct-children listings: the cursor is persisted before a directory is
    /// listed, so changes racing that non-snapshot pagination are observed by the
    /// following delta poll.
    pub async fn latest_delta(&self) -> Result<DeltaPage> {
        let url = format!("{}/me/drive/root/delta?token=latest", self.base);
        let response = self.send(|http| http.get(&url)).await?;
        Ok(response.json().await?)
    }

    /// Fetch one page of a folder's direct children.
    ///
    /// This is the partial-population path used when a directory is enumerated.
    /// A children listing is not an authoritative drive snapshot; the already
    /// established delta cursor reconciles writes that race its pagination.
    pub async fn children(&self, parent_id: &str, link: Option<&str>) -> Result<DeltaPage> {
        let base = &self.base;
        let url = match link {
            Some(link) => self.trusted_graph_link(link)?,
            None => {
                let parent_id = urlencode_segment(parent_id);
                format!("{base}/me/drive/items/{parent_id}/children?$top={CHILDREN_PAGE_SIZE}")
            }
        };
        let response = self.send(|http| http.get(&url)).await?;
        Ok(response.json().await?)
    }

    /// Validate an opaque continuation URL before attaching an access token.
    ///
    /// Graph continuation links are server-provided, but they are still data.
    /// Treating an arbitrary absolute URL as trusted would turn a compromised
    /// response or state database into a bearer-token disclosure primitive.
    fn trusted_graph_link(&self, link: &str) -> Result<String> {
        let base = Url::parse(&self.base)
            .map_err(|error| Error::other(format!("invalid Graph endpoint: {error}")))?;
        let candidate = base
            .join(link)
            .map_err(|error| Error::other(format!("invalid Graph continuation link: {error}")))?;

        if candidate.origin() != base.origin()
            || candidate.scheme() != base.scheme()
            || !candidate.username().is_empty()
            || candidate.password().is_some()
        {
            return Err(Error::other(
                "refusing a Graph continuation link from an untrusted origin",
            ));
        }
        Ok(candidate.into())
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

    /// Resolve a direct child by name. Normal sync bookkeeping stays id-based;
    /// this path form is reserved for reconciling a newly-created upload whose
    /// final `driveItem` response was lost after Graph committed the bytes.
    pub async fn child_by_name(&self, parent_id: &str, name: &str) -> Result<DriveItem> {
        let encoded = urlencode_segment(name);
        let url = format!("{}/me/drive/items/{parent_id}:/{encoded}", self.base);
        let response = self.send(|http| http.get(&url)).await?;
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
            // Never "replace": for a folder that already exists it replaces
            // the item, sending the existing folder and everything in it to
            // the recycle bin. The caller adopts the existing folder on 409.
            conflict_behavior: "fail",
        };
        let base = &self.base;
        let response = self
            .send(|http| {
                http.post(format!("{base}/me/drive/items/{parent_id}/children"))
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
                    // Keep this explicit for empty files. Some Graph frontends
                    // reject a zero-byte PUT as 411 when an intermediary turns
                    // an inferred empty body into a request with no length.
                    .header("Content-Length", content.len().to_string())
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
            Some(mtime) => self.set_mtime(&item.id, mtime, item.etag.as_deref()).await,
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
        let session: UploadSession = response.json().await?;
        self.validated_upload_url(&session.upload_url)?;
        Ok(session)
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
        let upload_url = self.validated_upload_url(upload_url)?;
        let chunk_len = u64::try_from(chunk.len())
            .map_err(|_| Error::other("upload chunk length does not fit in u64"))?;
        let end = upload_range_end(offset, total_size, chunk_len)?;
        let mut attempt = 0;
        loop {
            // A session URL carries its own authorisation and never refreshes
            // the token, but a retired account's bytes still must not keep
            // flowing.
            self.ensure_running()?;
            attempt += 1;
            let response = self
                .http
                .put(upload_url.clone())
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
                    tracing::debug!(
                        attempt,
                        timeout = e.is_timeout(),
                        connect = e.is_connect(),
                        "retrying upload chunk"
                    );
                    continue;
                }
                Err(e) => return Err(e.without_url().into()),
            };

            let status = response.status().as_u16();
            match status {
                // 202: more chunks wanted. 200/201: the file is complete.
                202 => {
                    let status = match response.json::<UploadStatus>().await {
                        Ok(status) => status,
                        Err(error) => {
                            // The fragment may already be durable even when its
                            // response body was cut short. Querying the session
                            // is the only safe way to choose the next offset.
                            tracing::debug!(
                                error = %error.without_url(),
                                "recovering upload offset after an unreadable 202 response"
                            );
                            self.upload_session_status_url(upload_url.clone()).await?
                        }
                    };
                    return Ok(ChunkOutcome::Accepted {
                        next_offset: status.next_offset(),
                        expires_at: status.expiration_date_time,
                    });
                }
                200 | 201 => {
                    return match response.json::<DriveItem>().await {
                        Ok(item) => Ok(ChunkOutcome::Completed(Box::new(item))),
                        Err(error) => Ok(ChunkOutcome::CompletionUnknown {
                            reason: error.without_url().to_string(),
                        }),
                    };
                }
                404 | 410 => {
                    // The session is gone; the caller must start a new one
                    // rather than keep pushing bytes into a dead URL.
                    return Err(Error::UploadSessionExpired {
                        item: "resumable upload".into(),
                        reason: format!("the service returned {status}"),
                    });
                }
                416 => {
                    // Graph uses 416 when a retried fragment overlaps bytes it
                    // already accepted. Ask for the authoritative missing range
                    // instead of discarding or blindly recreating the session.
                    let status = self.upload_session_status_url(upload_url.clone()).await?;
                    return Ok(ChunkOutcome::Accepted {
                        next_offset: status.next_offset(),
                        expires_at: status.expiration_date_time,
                    });
                }
                429 | 500..=599 if attempt < MAX_ATTEMPTS => {
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(delay.min(MAX_RETRY_AFTER)).await;
                }
                _ => {
                    let body = response
                        .bytes()
                        .await
                        .map_err(|error| Error::Http(error.without_url()))?;
                    return Err(graph_error(status, &body));
                }
            }
        }
    }

    /// Query which bytes are still missing from a resumable upload.
    pub async fn upload_session_status(&self, upload_url: &str) -> Result<UploadStatus> {
        let upload_url = self.validated_upload_url(upload_url)?;
        self.upload_session_status_url(upload_url).await
    }

    async fn upload_session_status_url(&self, upload_url: Url) -> Result<UploadStatus> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self.http.get(upload_url.clone()).send().await;
            let response = match response {
                Ok(response) => response,
                Err(error) if attempt < MAX_ATTEMPTS && is_transient_transport(&error) => {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(error) => return Err(Error::Http(error.without_url())),
            };

            let status = response.status().as_u16();
            match status {
                200 => {
                    return response
                        .json::<UploadStatus>()
                        .await
                        .map_err(|error| Error::Http(error.without_url()));
                }
                404 | 410 => {
                    return Err(Error::UploadSessionExpired {
                        item: "resumable upload".into(),
                        reason: format!("status query returned {status}"),
                    });
                }
                429 | 500..=599 if attempt < MAX_ATTEMPTS => {
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(delay.min(MAX_RETRY_AFTER)).await;
                }
                _ => {
                    let body = response
                        .bytes()
                        .await
                        .map_err(|error| Error::Http(error.without_url()))?;
                    return Err(graph_error(status, &body));
                }
            }
        }
    }

    /// Cancel a resumable upload, releasing the service-side staging area.
    pub async fn cancel_upload_session(&self, upload_url: &str) -> Result<()> {
        let upload_url = self.validated_upload_url(upload_url)?;
        // Best-effort: a session we cannot cancel expires on its own.
        let _ = self.http.delete(upload_url).send().await;
        Ok(())
    }

    /// Validate a pre-authenticated upload endpoint before sending file bytes.
    ///
    /// Drive upload hosts legitimately differ from the Graph API host (for
    /// example `*.up.1drv.com`), so origin equality would reject valid sessions.
    /// Requiring the configured endpoint's scheme still prevents production
    /// HTTPS sessions from being downgraded, while rejecting credentials and
    /// non-network URL schemes outright.
    fn validated_upload_url(&self, upload_url: &str) -> Result<Url> {
        let base = Url::parse(&self.base)
            .map_err(|error| Error::other(format!("invalid Graph endpoint: {error}")))?;
        let candidate = Url::parse(upload_url)
            .map_err(|error| Error::other(format!("invalid upload session URL: {error}")))?;
        if candidate.scheme() != base.scheme()
            || candidate.host_str().is_none()
            || !candidate.username().is_empty()
            || candidate.password().is_some()
            || candidate.fragment().is_some()
        {
            return Err(Error::other("refusing an unsafe upload session URL"));
        }
        Ok(candidate)
    }

    pub async fn delete(&self, item_id: &str, if_match: Option<&str>) -> Result<()> {
        let base = &self.base;
        let if_match = if_match.map(str::to_owned);
        self.send(move |http| {
            let mut request = http.delete(format!("{base}/me/drive/items/{item_id}"));
            if let Some(tag) = &if_match {
                request = request.header("If-Match", tag);
            }
            request
        })
        .await?;
        Ok(())
    }

    /// Rename and/or move an item.
    pub async fn move_item(
        &self,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
        if_match: Option<&str>,
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
        let if_match = if_match.map(str::to_owned);
        let response = self
            .send(move |http| {
                let mut request = http
                    .patch(format!("{base}/me/drive/items/{item_id}"))
                    .json(&body);
                if let Some(tag) = &if_match {
                    request = request.header("If-Match", tag);
                }
                request
            })
            .await?;
        Ok(response.json().await?)
    }

    /// Stamp an item's client-side modification time.
    pub async fn set_mtime(
        &self,
        item_id: &str,
        mtime: OffsetDateTime,
        if_match: Option<&str>,
    ) -> Result<DriveItem> {
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
        let if_match = if_match.map(str::to_owned);
        let response = self
            .send(move |http| {
                let mut request = http
                    .patch(format!("{base}/me/drive/items/{item_id}"))
                    .json(&body);
                if let Some(tag) = &if_match {
                    request = request.header("If-Match", tag);
                }
                request
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
            // Checked per attempt, not once: a retry must not outlive the
            // account either, and `access_token` below is what would rotate
            // the credential of an account that no longer exists.
            self.ensure_running()?;
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
                429 | 503 | 504 => {
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    // Running out of in-call attempts is not a verdict on the
                    // request: the service is asking for patience. Hand the
                    // wait back to the queue, which retries with its own
                    // backoff, instead of letting it read as a permanent
                    // 429 and give the transfer up.
                    if attempt >= MAX_ATTEMPTS || delay > MAX_RETRY_AFTER {
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

fn upload_range_end(offset: u64, total_size: u64, chunk_len: u64) -> Result<u64> {
    if chunk_len == 0 {
        return Err(Error::other("refusing to upload an empty chunk"));
    }
    let end_exclusive = offset
        .checked_add(chunk_len)
        .ok_or_else(|| Error::other("upload byte range overflow"))?;
    if end_exclusive > total_size {
        return Err(Error::other(format!(
            "upload range {offset}-{end_exclusive} exceeds {total_size} bytes"
        )));
    }
    Ok(end_exclusive - 1)
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
mod tests;
