//! Google OAuth 2.0 for installed apps (spec §7.3): PKCE (S256), a one-shot
//! loopback redirect on 127.0.0.1, code exchange and refresh. The browser
//! is opened by the app; nothing here embeds a web view.
//!
//! Scopes: `gmail.modify` for mail, plus `openid profile` so the token
//! response carries an ID token with the account's name and picture for
//! the account switcher (spec §7.7). Both identity scopes are
//! non-sensitive. Gmail's profile call still supplies the address.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mail_domain::Redacted;
use provider_api::{AccessToken, ProviderError, ProviderResult, TokenSource};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify openid profile";

/// Refresh this long before the access token actually expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// A desktop OAuth client. Installed apps cannot keep secrets (Google's own
/// guidance), so the secret is not treated as confidential, but it is still
/// kept out of logs.
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: Option<Redacted<String>>,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Present when `openid` was granted.
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Who signed in, from the ID token (spec §7.7).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
}

/// Read the claims of an ID token. Not verified: it came straight from
/// Google's token endpoint over TLS, which Google documents as sufficient
/// for a token the client requested itself; it is used only for display.
pub fn identity_from_id_token(id_token: &str) -> Option<Identity> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let mut identity: Identity = serde_json::from_slice(&bytes).ok()?;
    identity.name = identity.name.filter(|n| !n.trim().is_empty());
    // Only Google's own image hosts, over HTTPS.
    identity.picture = identity.picture.filter(|p| {
        url::Url::parse(p).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str().is_some_and(|h| h == "googleusercontent.com" || h.ends_with(".googleusercontent.com"))
        })
    });
    Some(identity)
}

pub const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// The signed-in user's name and picture, for refreshing the avatar.
pub async fn fetch_userinfo(http: &reqwest::Client, url: &str, token: &AccessToken) -> ProviderResult<Identity> {
    let response =
        http.get(url).bearer_auth(token.expose()).send().await.map_err(|e| ProviderError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        401 => return Err(ProviderError::Unauthorized),
        403 => return Err(ProviderError::Forbidden("the profile scope was not granted".into())),
        s => return Err(ProviderError::Invalid(format!("userinfo: HTTP {s}"))),
    }
    let claims: serde_json::Value = response.json().await.map_err(|e| ProviderError::Decode(e.to_string()))?;
    // Same filtering as the ID token: reuse it on a synthetic token.
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap_or_default());
    Ok(identity_from_id_token(&format!("x.{payload}.x")).unwrap_or_default())
}

/// Picture URLs take a size suffix (`=s96-c`); ask for a small square.
pub fn sized_picture_url(url: &str, pixels: u32) -> String {
    let base = url.split('=').next().unwrap_or(url);
    format!("{base}=s{pixels}-c")
}

/// Download an avatar image, at most 1 MB.
pub async fn download_picture(http: &reqwest::Client, url: &str) -> ProviderResult<Vec<u8>> {
    let response = http.get(url).send().await.map_err(|e| ProviderError::Network(e.to_string()))?;
    if !response.status().is_success() {
        return Err(ProviderError::Invalid(format!("picture: HTTP {}", response.status().as_u16())));
    }
    let bytes = response.bytes().await.map_err(|e| ProviderError::Network(e.to_string()))?;
    if bytes.len() > 1_000_000 {
        return Err(ProviderError::Invalid("picture too large".into()));
    }
    Ok(bytes.to_vec())
}

/// A started authorization: open `url` in the browser, then await the code.
pub struct PendingAuthorization {
    pub url: String,
    pub redirect_uri: String,
    verifier: Redacted<String>,
    state: String,
    listener: TcpListener,
}

#[derive(Debug)]
pub struct AuthorizationCode {
    pub code: Redacted<String>,
    pub redirect_uri: String,
    pub verifier: Redacted<String>,
}

fn random_token(bytes: usize) -> ProviderResult<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| ProviderError::Invalid(format!("no secure randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Bind the loopback listener and build the authorization URL.
pub async fn begin(client: &OAuthClient, login_hint: Option<&str>) -> ProviderResult<PendingAuthorization> {
    begin_with(client, AUTH_URL, login_hint).await
}

pub async fn begin_with(
    client: &OAuthClient,
    auth_url: &str,
    login_hint: Option<&str>,
) -> ProviderResult<PendingAuthorization> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| ProviderError::Network(e.to_string()))?;
    let port = listener.local_addr().map_err(|e| ProviderError::Network(e.to_string()))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let mut url = url::Url::parse(auth_url).map_err(|e| ProviderError::Invalid(e.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &code_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("access_type", "offline")
            // Always return a refresh token, even on re-authorization; and
            // when adding an account (no hint), let the user pick which one
            // instead of silently reusing the browser's session.
            .append_pair("prompt", if login_hint.is_some() { "consent" } else { "consent select_account" });
        if let Some(hint) = login_hint {
            q.append_pair("login_hint", hint);
        }
    }
    Ok(PendingAuthorization { url: url.into(), redirect_uri, verifier: Redacted::new(verifier), state, listener })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>OpenAGC</title>\
<body style=\"font:15px -apple-system,system-ui;margin:4em;text-align:center\">\
<h2>You're signed in</h2><p>You can close this window and return to OpenAGC.</p></body>";

const FAILED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>OpenAGC</title>\
<body style=\"font:15px -apple-system,system-ui;margin:4em;text-align:center\">\
<h2>Sign-in didn't complete</h2><p>Return to OpenAGC and try again.</p></body>";

impl PendingAuthorization {
    /// Wait for the browser's redirect. Requests that are not the callback
    /// (favicon, probes) are answered and ignored; a callback with the wrong
    /// `state` is rejected.
    pub async fn wait_for_code(self, timeout: Duration) -> ProviderResult<AuthorizationCode> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let accepted = tokio::time::timeout(remaining, self.listener.accept())
                .await
                .map_err(|_| ProviderError::Invalid("sign-in timed out".into()))?;
            let (mut stream, _) = accepted.map_err(|e| ProviderError::Network(e.to_string()))?;
            let mut buf = vec![0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let target = request.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
            let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
                respond(&mut stream, 400, FAILED_PAGE).await;
                continue;
            };
            if url.path() != "/callback" {
                respond(&mut stream, 404, "").await;
                continue;
            }
            let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
            if param("state").as_deref() != Some(self.state.as_str()) {
                respond(&mut stream, 400, FAILED_PAGE).await;
                return Err(ProviderError::Invalid("sign-in response did not match this request".into()));
            }
            if let Some(error) = param("error") {
                respond(&mut stream, 200, FAILED_PAGE).await;
                return Err(if error == "access_denied" {
                    ProviderError::Forbidden("access was not granted".into())
                } else {
                    ProviderError::Invalid(format!("authorization failed: {error}"))
                });
            }
            let Some(code) = param("code") else {
                respond(&mut stream, 400, FAILED_PAGE).await;
                return Err(ProviderError::Invalid("authorization response had no code".into()));
            };
            respond(&mut stream, 200, DONE_PAGE).await;
            return Ok(AuthorizationCode {
                code: Redacted::new(code),
                redirect_uri: self.redirect_uri,
                verifier: self.verifier,
            });
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

pub async fn exchange_code(
    http: &reqwest::Client,
    token_url: &str,
    client: &OAuthClient,
    code: &AuthorizationCode,
) -> ProviderResult<TokenResponse> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.code.expose().clone()),
        ("redirect_uri", code.redirect_uri.clone()),
        ("client_id", client.client_id.clone()),
        ("code_verifier", code.verifier.expose().clone()),
    ];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.expose().clone()));
    }
    token_request(http, token_url, &form).await
}

pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    client: &OAuthClient,
    refresh_token: &Redacted<String>,
) -> ProviderResult<TokenResponse> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh_token.expose().clone()),
        ("client_id", client.client_id.clone()),
    ];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.expose().clone()));
    }
    token_request(http, token_url, &form).await
}

async fn token_request(http: &reqwest::Client, url: &str, form: &[(&str, String)]) -> ProviderResult<TokenResponse> {
    let response = http.post(url).form(form).send().await.map_err(|e| ProviderError::Network(e.to_string()))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| ProviderError::Network(e.to_string()))?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| ProviderError::Invalid(format!("token response: {e}")));
    }
    let error = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    // invalid_grant: the refresh token was revoked or expired; sign in again.
    if error == "invalid_grant" || status.as_u16() == 401 {
        return Err(ProviderError::Unauthorized);
    }
    if status.is_server_error() {
        return Err(ProviderError::Server { status: status.as_u16(), message: error });
    }
    Err(ProviderError::Invalid(format!("token endpoint: {error} ({})", status.as_u16())))
}

/// A [`TokenSource`] backed by a refresh token: caches the access token and
/// refreshes shortly before it expires or after the provider rejects it.
pub struct GoogleTokenSource {
    http: reqwest::Client,
    token_url: String,
    client: OAuthClient,
    refresh_token: Redacted<String>,
    cache: Mutex<Option<(AccessToken, Instant)>>,
}

impl GoogleTokenSource {
    pub fn new(client: OAuthClient, refresh_token: Redacted<String>) -> Arc<Self> {
        Self::with_token_url(client, refresh_token, TOKEN_URL)
    }

    pub fn with_token_url(client: OAuthClient, refresh_token: Redacted<String>, token_url: &str) -> Arc<Self> {
        Arc::new(Self {
            http: reqwest::Client::new(),
            token_url: token_url.to_owned(),
            client,
            refresh_token,
            cache: Mutex::new(None),
        })
    }

    /// Seed the cache with a token obtained during sign-in.
    pub async fn prime(&self, access_token: String, expires_in: Option<u64>) {
        let expires = Instant::now() + Duration::from_secs(expires_in.unwrap_or(3600));
        *self.cache.lock().await = Some((Redacted::new(access_token), expires));
    }
}

#[async_trait]
impl TokenSource for GoogleTokenSource {
    async fn access_token(&self) -> ProviderResult<AccessToken> {
        let mut cache = self.cache.lock().await;
        if let Some((token, expires)) = cache.as_ref()
            && Instant::now() + EXPIRY_MARGIN < *expires
        {
            return Ok(token.clone());
        }
        let fresh = refresh(&self.http, &self.token_url, &self.client, &self.refresh_token).await?;
        let expires = Instant::now() + Duration::from_secs(fresh.expires_in.unwrap_or(3600));
        let token = Redacted::new(fresh.access_token);
        *cache = Some((token.clone(), expires));
        Ok(token)
    }

    async fn invalidate(&self, token: &AccessToken) {
        let mut cache = self.cache.lock().await;
        if cache.as_ref().is_some_and(|(t, _)| t == token) {
            *cache = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> OAuthClient {
        OAuthClient {
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: Some(Redacted::new("sec".into())),
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[tokio::test]
    async fn the_authorization_url_carries_pkce_state_and_offline_access() {
        let pending = begin(&client(), Some("me@example.com")).await.unwrap();
        let url = url::Url::parse(&pending.url).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], code_challenge(pending.verifier.expose()));
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["redirect_uri"], pending.redirect_uri);
        assert!(pending.redirect_uri.starts_with("http://127.0.0.1:"));
        assert_eq!(q["login_hint"], "me@example.com");
        assert_eq!(q["prompt"], "consent", "signing in again: the hinted account");
        assert!(q["state"].len() >= 32);
        assert!(q["scope"].split(' ').any(|s| s == "openid") && q["scope"].split(' ').any(|s| s == "profile"));

        let adding = begin(&client(), None).await.unwrap();
        let q: std::collections::HashMap<String, String> =
            url::Url::parse(&adding.url).unwrap().query_pairs().into_owned().collect();
        assert_eq!(q["prompt"], "consent select_account", "adding: Google shows its account chooser");
        assert!(!q.contains_key("login_hint"));
    }

    fn id_token(claims: serde_json::Value) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()))
    }

    #[test]
    fn the_id_token_gives_a_name_and_only_a_google_hosted_picture() {
        let who = identity_from_id_token(&id_token(serde_json::json!({
            "sub": "1", "name": "Ada Lovelace", "picture": "https://lh3.googleusercontent.com/a/abc=s96-c"
        })))
        .unwrap();
        assert_eq!(who.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(who.picture.as_deref(), Some("https://lh3.googleusercontent.com/a/abc=s96-c"));
        assert_eq!(
            sized_picture_url(who.picture.as_deref().unwrap(), 128),
            "https://lh3.googleusercontent.com/a/abc=s128-c"
        );

        let hostile = identity_from_id_token(&id_token(serde_json::json!({
            "name": " ", "picture": "http://evil.example/p.png"
        })))
        .unwrap();
        assert_eq!(hostile, Identity::default(), "blank names and foreign or plain-http pictures are dropped");
        let lookalike = identity_from_id_token(&id_token(
            serde_json::json!({"picture": "https://googleusercontent.com.evil.example/x"}),
        ))
        .unwrap();
        assert_eq!(lookalike.picture, None);
        assert_eq!(identity_from_id_token("not-a-jwt"), None);
    }

    #[tokio::test]
    async fn userinfo_gives_the_name_and_picture() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .and(wiremock::matchers::header("authorization", "Bearer at"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": "1", "name": "Ada", "picture": "https://lh3.googleusercontent.com/a/x"
            })))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let who = fetch_userinfo(&http, &format!("{}/userinfo", server.uri()), &Redacted::new("at".to_owned()))
            .await
            .unwrap();
        assert_eq!(who.name.as_deref(), Some("Ada"));
        assert_eq!(who.picture.as_deref(), Some("https://lh3.googleusercontent.com/a/x"));
        let denied =
            fetch_userinfo(&http, &format!("{}/userinfo", server.uri()), &Redacted::new("other".to_owned())).await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn pictures_download_with_a_size_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/small"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xd8, 0xff]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/huge"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 1_200_000]))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        assert_eq!(download_picture(&http, &format!("{}/small", server.uri())).await.unwrap(), vec![0xff, 0xd8, 0xff]);
        assert!(download_picture(&http, &format!("{}/huge", server.uri())).await.is_err());
        assert!(download_picture(&http, &format!("{}/missing", server.uri())).await.is_err());
    }

    async fn hit(redirect_uri: &str, path_and_query: &str) -> String {
        let base = redirect_uri.trim_end_matches("/callback");
        reqwest::get(format!("{base}{path_and_query}")).await.unwrap().text().await.unwrap()
    }

    #[tokio::test]
    async fn the_loopback_listener_returns_the_code_and_ignores_other_requests() {
        let pending = begin(&client(), None).await.unwrap();
        let redirect = pending.redirect_uri.clone();
        let state =
            url::Url::parse(&pending.url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
        let waiter = tokio::spawn(pending.wait_for_code(Duration::from_secs(10)));
        assert_eq!(hit(&redirect, "/favicon.ico").await, "");
        let page = hit(&redirect, &format!("/callback?code=4%2Fabc&state={state}&scope={SCOPE}")).await;
        assert!(page.contains("signed in"));
        let code = waiter.await.unwrap().unwrap();
        assert_eq!(code.code.expose(), "4/abc");
        assert_eq!(code.redirect_uri, redirect);
    }

    #[tokio::test]
    async fn a_mismatched_state_is_rejected() {
        let pending = begin(&client(), None).await.unwrap();
        let redirect = pending.redirect_uri.clone();
        let waiter = tokio::spawn(pending.wait_for_code(Duration::from_secs(10)));
        let page = hit(&redirect, "/callback?code=x&state=forged").await;
        assert!(page.contains("didn't complete"));
        assert!(matches!(waiter.await.unwrap(), Err(ProviderError::Invalid(_))));
    }

    #[tokio::test]
    async fn a_denied_consent_is_reported() {
        let pending = begin(&client(), None).await.unwrap();
        let redirect = pending.redirect_uri.clone();
        let state =
            url::Url::parse(&pending.url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
        let waiter = tokio::spawn(pending.wait_for_code(Duration::from_secs(10)));
        hit(&redirect, &format!("/callback?error=access_denied&state={state}")).await;
        assert!(matches!(waiter.await.unwrap(), Err(ProviderError::Forbidden(_))));
    }

    #[tokio::test]
    async fn waiting_times_out() {
        let pending = begin(&client(), None).await.unwrap();
        let err = pending.wait_for_code(Duration::from_millis(50)).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(ref m) if m.contains("timed out")));
    }

    #[tokio::test]
    async fn code_exchange_and_refresh_send_the_right_forms() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code_verifier=ver"))
            .and(body_string_contains("client_secret=sec"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at1", "expires_in": 3599, "refresh_token": "rt1", "token_type": "Bearer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "at2", "expires_in": 3599})),
            )
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let code = AuthorizationCode {
            code: Redacted::new("c".into()),
            redirect_uri: "http://127.0.0.1:1/callback".into(),
            verifier: Redacted::new("ver".into()),
        };
        let t = exchange_code(&http, &token_url, &client(), &code).await.unwrap();
        assert_eq!((t.access_token.as_str(), t.refresh_token.as_deref()), ("at1", Some("rt1")));

        let source = GoogleTokenSource::with_token_url(client(), Redacted::new("rt1".into()), &token_url);
        let first = source.access_token().await.unwrap();
        assert_eq!(first.expose(), "at2");
        let cached = source.access_token().await.unwrap();
        assert_eq!(cached, first, "cached until near expiry");
        let refreshes = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("refresh_token"))
            .count();
        assert_eq!(refreshes, 1);
        source.invalidate(&first).await;
        source.access_token().await.unwrap();
        let refreshes = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("refresh_token"))
            .count();
        assert_eq!(refreshes, 2, "invalidation forces a refresh");
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_means_sign_in_again() {
        let server = MockServer::start().await;
        Mock::given(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid_grant"})))
            .mount(&server)
            .await;
        let source = GoogleTokenSource::with_token_url(
            client(),
            Redacted::new("revoked".into()),
            &format!("{}/token", server.uri()),
        );
        assert_eq!(source.access_token().await.unwrap_err(), ProviderError::Unauthorized);
    }
}
