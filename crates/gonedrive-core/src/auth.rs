//! OAuth 2.0 authorization code flow with PKCE against the Microsoft identity
//! platform.
//!
//! GOnedrive is a *public* client: there is no client secret, so PKCE is the
//! only thing binding the authorization code to this process. The code is
//! delivered to a loopback listener on an ephemeral port, which is the flow
//! Microsoft documents for native apps. Notably we open the user's real browser
//! rather than an embedded WebView — Microsoft blocks embedded WebViews for new
//! OAuth flows, and it is also the only way the user can see the address bar
//! they are typing their password into.
//!
//! The redirect URI is deliberately **`http://localhost:<port>` with no path**:
//!
//! * Microsoft ignores the port component when matching a `localhost` redirect
//!   URI (and only for `localhost`), which is what lets us bind an ephemeral
//!   port instead of squatting on a fixed one.
//! * The *path* is **not** ignored. `http://localhost` is one of the ready-made
//!   checkboxes under the portal's "Mobile and desktop applications" platform,
//!   so matching it exactly turns app registration into one click. Asking for
//!   `/callback` instead would fail with AADSTS50011 for anyone who ticked the
//!   suggested box rather than hand-typing a URI with a path.
//! * The docs suggest preferring the literal `127.0.0.1` over `localhost`, but
//!   the portal UI refuses to add an `http` loopback URI on that address (it
//!   requires hand-editing the application manifest), and the documented
//!   port-exemption is written for `localhost`. One click beats one manifest
//!   edit here.
//!
//! Because a redirect URI with no path segment comes back with a trailing
//! slash, the code arrives at `http://localhost:<port>/?code=…`.
//!
//! Refresh tokens are rotated by the service on every use, so a refresh that
//! returns a new token *must* persist it or the account silently breaks on the
//! next daemon restart.

use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::config::{AUTHORITY, SCOPES};
use crate::error::{Error, Result};

/// Refresh this long before the access token actually expires, so an in-flight
/// request never races the expiry.
const REFRESH_SKEW: Duration = Duration::from_secs(300);

/// How long to wait for the user to finish signing in before giving up.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// A PKCE verifier/challenge pair (RFC 7636, S256).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        // 32 random bytes base64url-encode to 43 characters, the minimum
        // verifier length RFC 7636 allows.
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| Error::other(format!("no secure randomness: {e}")))?;
        let verifier = URL_SAFE_NO_PAD.encode(raw);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// Opaque anti-CSRF value echoed back by the authorization server.
fn random_state() -> Result<String> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| Error::other(format!("no secure randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

/// A live set of credentials for one account.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: OffsetDateTime,
}

impl TokenSet {
    /// True once we are inside the refresh window, not merely once expired.
    pub fn needs_refresh(&self) -> bool {
        let skew = time::Duration::seconds(REFRESH_SKEW.as_secs() as i64);
        OffsetDateTime::now_utc() + skew >= self.expires_at
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Absent when the tenant declines to rotate; we then keep the old one.
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: String,
}

/// A bound loopback listener plus the redirect URI that points at it.
///
/// Bound *before* the authorization URL is built, because the URI has to carry
/// the port the OS actually gave us.
pub struct LoopbackReceiver {
    listener: TcpListener,
    redirect_uri: String,
}

impl LoopbackReceiver {
    pub async fn bind() -> Result<Self> {
        // Port 0 asks the kernel for a free ephemeral port. Binding to
        // 127.0.0.1 rather than 0.0.0.0 keeps the code off the network.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            // No path: see the module docs. We bind the loopback *address* but
            // name it `localhost`, because that is what the registration says
            // and what Microsoft matches against.
            redirect_uri: format!("http://localhost:{port}"),
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Serve requests until one carries the authorization code.
    ///
    /// Browsers happily fire off `/favicon.ico` and speculative preconnects at
    /// this listener, so anything that is not the callback gets a 404 and the
    /// loop continues rather than aborting the login.
    async fn recv(&self, expected_state: &str, timeout: Duration) -> Result<String> {
        tokio::time::timeout(timeout, self.accept_loop(expected_state))
            .await
            .map_err(|_| Error::AuthRequired("timed out waiting for the browser".into()))?
    }

    async fn accept_loop(&self, expected_state: &str) -> Result<String> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            match self.handle(stream, expected_state).await {
                Ok(Some(code)) => return Ok(code),
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    async fn handle(&self, mut stream: TcpStream, expected_state: &str) -> Result<Option<String>> {
        let Some(target) = read_request_target(&mut stream).await? else {
            return Ok(None);
        };

        // The request target is origin-form (`/callback?...`); give it a base
        // so `Url` will parse the query for us.
        let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
            respond(&mut stream, 400, PAGE_BAD_REQUEST).await?;
            return Ok(None);
        };
        // A redirect URI with no path segment is returned with a trailing
        // slash, so the code lands on "/". Anything else is a browser probe.
        if url.path() != "/" {
            respond(&mut stream, 404, PAGE_NOT_FOUND).await?;
            return Ok(None);
        }

        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();

        if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| error.clone());
            respond(&mut stream, 200, &page_failed(&description)).await?;
            return Err(Error::AuthRequired(description));
        }

        // Check `state` before touching `code`: a mismatch means this response
        // did not come from the authorization request we started.
        match params.get("state") {
            Some(state) if state == expected_state => {}
            _ => {
                respond(&mut stream, 400, PAGE_BAD_REQUEST).await?;
                return Err(Error::AuthRequired(
                    "the sign-in response did not match this request (state mismatch)".into(),
                ));
            }
        }

        match params.get("code") {
            Some(code) if !code.is_empty() => {
                respond(&mut stream, 200, PAGE_SUCCESS).await?;
                Ok(Some(code.clone()))
            }
            _ => {
                respond(&mut stream, 400, PAGE_BAD_REQUEST).await?;
                Err(Error::AuthRequired(
                    "the sign-in response carried no authorization code".into(),
                ))
            }
        }
    }
}

/// Read just enough of the request to get its target, then stop.
///
/// We never read a body, so we only need bytes up to the end of the headers.
async fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>> {
    const MAX: usize = 8 * 1024;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None); // client hung up (preconnect probes do this)
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX {
            return Ok(None);
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let Some(line) = head.lines().next() else {
        return Ok(None);
    };
    // "GET /callback?code=... HTTP/1.1"
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    if method != "GET" {
        return Ok(None);
    }
    Ok(Some(target.to_string()))
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Everything needed to send the user to the authorization server.
pub struct PendingLogin {
    pub authorize_url: Url,
    receiver: LoopbackReceiver,
    pkce: Pkce,
    state: String,
    client_id: String,
}

impl PendingLogin {
    /// Bind the loopback listener and build the authorization URL.
    ///
    /// The caller opens `authorize_url` in the user's browser and then awaits
    /// [`PendingLogin::complete`].
    pub async fn start(client_id: &str) -> Result<Self> {
        let receiver = LoopbackReceiver::bind().await?;
        let pkce = Pkce::generate()?;
        let state = random_state()?;

        let mut authorize_url = Url::parse(&format!("{AUTHORITY}/oauth2/v2.0/authorize"))
            .map_err(|e| Error::other(format!("bad authority URL: {e}")))?;
        authorize_url
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", receiver.redirect_uri())
            .append_pair("response_mode", "query")
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            // Always show the picker: without it a user who is signed into a
            // work account in their browser cannot add a personal one.
            .append_pair("prompt", "select_account");

        Ok(Self {
            authorize_url,
            receiver,
            pkce,
            state,
            client_id: client_id.to_string(),
        })
    }

    /// Wait for the browser redirect and exchange the code for tokens.
    pub async fn complete(self, http: &reqwest::Client, timeout: Duration) -> Result<TokenSet> {
        let code = self.receiver.recv(&self.state, timeout).await?;
        let scope = SCOPES.join(" ");
        let form = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.receiver.redirect_uri()),
            ("code_verifier", self.pkce.verifier.as_str()),
            ("scope", scope.as_str()),
        ];
        post_token(http, &form, None).await
    }
}

/// Exchange a refresh token for a fresh access token.
///
/// Returns the new [`TokenSet`], carrying forward the previous refresh token
/// when the service chose not to rotate it.
pub async fn refresh(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    let scope = SCOPES.join(" ");
    let form = [
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", scope.as_str()),
    ];
    post_token(http, &form, Some(refresh_token)).await
}

async fn post_token(
    http: &reqwest::Client,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<TokenSet> {
    let response = http
        .post(format!("{AUTHORITY}/oauth2/v2.0/token"))
        .form(form)
        .send()
        .await?;

    let status = response.status();
    let body = response.bytes().await?;

    if !status.is_success() {
        // The identity platform returns a structured error; `invalid_grant`
        // specifically means the refresh token is dead and only a fresh
        // interactive login can recover.
        let parsed: std::result::Result<TokenErrorResponse, _> = serde_json::from_slice(&body);
        return Err(match parsed {
            Ok(e) => {
                let message = if e.error_description.is_empty() {
                    e.error.clone()
                } else {
                    e.error_description
                };
                Error::AuthRequired(format!("{}: {message}", e.error))
            }
            Err(_) => Error::AuthRequired(format!(
                "token endpoint returned {status}: {}",
                String::from_utf8_lossy(&body)
            )),
        });
    }

    let token: TokenResponse = serde_json::from_slice(&body)?;
    let refresh_token = token
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_string))
        .ok_or_else(|| {
            Error::AuthRequired("the token response contained no refresh token".into())
        })?;

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token,
        expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(token.expires_in),
    })
}

const PAGE_SUCCESS: &str = r#"<!doctype html><meta charset="utf-8">
<title>GOnedrive</title>
<style>body{font-family:system-ui,sans-serif;display:grid;place-items:center;height:100vh;margin:0;background:#faf9f8;color:#242424}
div{text-align:center}h1{font-weight:600;font-size:1.4rem}p{color:#616161}</style>
<div><h1>登录成功</h1><p>可以关闭此页面，返回 GOnedrive。</p></div>"#;

const PAGE_NOT_FOUND: &str = r#"<!doctype html><meta charset="utf-8"><title>GOnedrive</title>Not found."#;

const PAGE_BAD_REQUEST: &str = r#"<!doctype html><meta charset="utf-8">
<title>GOnedrive</title>
<style>body{font-family:system-ui,sans-serif;display:grid;place-items:center;height:100vh;margin:0}</style>
<div><h1>请求无效</h1><p>请返回 GOnedrive 重新登录。</p></div>"#;

fn page_failed(description: &str) -> String {
    // The description comes from the identity platform, so escape it rather
    // than trusting it into the DOM.
    let escaped = description
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>GOnedrive</title>
<style>body{{font-family:system-ui,sans-serif;display:grid;place-items:center;height:100vh;margin:0}}
div{{text-align:center;max-width:40rem}}p{{color:#616161}}</style>
<div><h1>登录失败</h1><p>{escaped}</p></div>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = Pkce::generate().unwrap();
        assert!(pkce.verifier.len() >= 43 && pkce.verifier.len() <= 128);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        // base64url must not contain characters that need percent-encoding.
        assert!(!pkce.challenge.contains('+') && !pkce.challenge.contains('/'));
    }

    #[test]
    fn pkce_is_not_deterministic() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn token_set_enters_refresh_window_early() {
        let almost = TokenSet {
            access_token: "a".into(),
            refresh_token: "r".into(),
            // Inside the 5-minute skew but not yet expired.
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(60),
        };
        assert!(almost.needs_refresh());

        let fresh = TokenSet {
            expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
            ..almost.clone()
        };
        assert!(!fresh.needs_refresh());
    }

    #[tokio::test]
    async fn authorize_url_carries_pkce_and_loopback_redirect() {
        let login = PendingLogin::start("test-client").await.unwrap();
        let params: HashMap<_, _> = login.authorize_url.query_pairs().into_owned().collect();
        assert_eq!(params["client_id"], "test-client");
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["code_challenge"], login.pkce.challenge);
        // Must stay path-less: the portal's ready-made `http://localhost`
        // checkbox has no path, and paths are matched exactly.
        let redirect = &params["redirect_uri"];
        assert!(redirect.starts_with("http://localhost:"));
        assert!(
            !redirect[("http://localhost:".len())..].contains('/'),
            "redirect URI must have no path segment, got {redirect}"
        );
        assert!(params["scope"].contains("offline_access"));
        // The verifier must never leave the process.
        assert!(!login.authorize_url.as_str().contains(&login.pkce.verifier));
    }

    #[tokio::test]
    async fn callback_rejects_mismatched_state() {
        let receiver = LoopbackReceiver::bind().await.unwrap();
        let uri = receiver.redirect_uri().to_string();
        tokio::spawn(async move {
            let url = format!("{uri}/?code=abc&state=WRONG");
            let _ = reqwest::get(&url).await;
        });
        let err = receiver
            .recv("EXPECTED", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::AuthRequired(ref m) if m.contains("state mismatch")));
    }

    #[tokio::test]
    async fn callback_returns_code_and_ignores_unrelated_requests() {
        let receiver = LoopbackReceiver::bind().await.unwrap();
        let base = receiver.redirect_uri().to_string();
        tokio::spawn(async move {
            // A browser probe first, then the real redirect.
            let _ = reqwest::get(format!("{base}/favicon.ico")).await;
            let _ = reqwest::get(format!("{base}/?code=THE_CODE&state=S")).await;
        });
        let code = receiver.recv("S", Duration::from_secs(5)).await.unwrap();
        assert_eq!(code, "THE_CODE");
    }

    #[tokio::test]
    async fn callback_surfaces_server_side_errors() {
        let receiver = LoopbackReceiver::bind().await.unwrap();
        let uri = receiver.redirect_uri().to_string();
        tokio::spawn(async move {
            let _ = reqwest::get(format!(
                "{uri}/?error=access_denied&error_description=User+cancelled&state=S"
            ))
            .await;
        });
        let err = receiver.recv("S", Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, Error::AuthRequired(ref m) if m.contains("User cancelled")));
    }
}
